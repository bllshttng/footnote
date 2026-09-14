"""One plan-level hold reader shared by every PR merge path."""
from __future__ import annotations

import os
import sys
from typing import Optional

from fno.graph.ladder import DispatchHoldState, DispatchHoldVerdict, dispatch_hold_verdict


class HoldLookupError(RuntimeError):
    """The merge path could not prove whether its bound plan is held."""


def hold_for_pr(pr_number: int, cwd: str) -> Optional[DispatchHoldVerdict]:
    """Return the held/invalid plan ancestry for a PR, or None when unheld.

    Checks BOTH the ref-stamped node (``_find_pr_node_id``) and every node
    named on the PR's exact ``Backlog-Closure`` trailer - a trailer-only
    claim (a node never individually stamped at creation) was invisible to
    the ref-based match alone, so a held node named only on the trailer
    passed this gate and closed post-merge via ``bind_closure_claims``,
    which performs no hold check of its own (round-10 review fix).
    """
    from fno.graph.store import read_graph
    from fno.paths import graph_json
    from fno.pr import _merge
    from fno.pr._merge import _find_pr_node_id
    from fno.pr._proc import ToolMissing
    from fno.pr.closure import ClosureQueryError, fetch_pr_closure_context, parse_closure_trailer
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        # A hold is a footnote-graph-resident concept: under an external
        # tracker backend this repo's graph.json is not the delivery record
        # of truth, so there is no plan hold to read - never "unreadable".
        return None

    try:
        entries = read_graph(graph_json())
    except Exception as exc:  # noqa: BLE001 - hold reads fail closed
        raise HoldLookupError(f"backlog graph is unreadable: {exc}") from exc

    by_id = {
        entry.get("id"): entry
        for entry in entries
        if isinstance(entry, dict) and entry.get("id")
    }
    if not by_id:
        # Nothing in the graph could possibly be held - a ref match or a
        # trailer claim can only ever name an id this graph carries, and
        # by_id is what candidate_ids gets checked against below. Skip the PR
        # fetch entirely rather than paying a gh call (and a fail-closed trip
        # on its failure) for a lookup whose answer is always None.
        return None

    import os
    from pathlib import Path

    from fno.paths import resolve_canonical_worktree

    # resolve_canonical_worktree(cwd) - NOT a plain `git rev-parse
    # --show-toplevel` - is load-bearing: hold_for_pr is called with an
    # explicit `cwd` that is not always the process's own (`fno do pr
    # hold-check --repo <path>` passes an arbitrary directory), and a plain
    # toplevel resolution run FROM A LINKED WORKTREE returns that worktree's
    # own path, not the canonical/main root every node's `cwd` field
    # actually stores - so either ignoring `cwd` (review fix) or resolving
    # the wrong root from it would both silently mismatch a genuinely-held
    # node's own repo.
    _canonical_root = resolve_canonical_worktree(Path(cwd))
    if _canonical_root is not None:
        _root = os.path.normpath(str(_canonical_root))
    else:
        # resolve_canonical_worktree's own docstring: a bare repo or a
        # separate-git-dir checkout returns None, "the caller's
        # --show-toplevel fallback yields the real checkout" - never the
        # RAW, unresolved cwd. A relative or unnormalized cwd (e.g. `fno do pr
        # hold-check 42 --repo .`) can never equal or prefix an entry's
        # absolute stored cwd, which would silently fail the project-scope
        # gate open for a repo that genuinely has held nodes (review fix).
        try:
            _toplevel = _merge.run(["git", "rev-parse", "--show-toplevel"], cwd=cwd)
        except ToolMissing:
            _toplevel = None
        _root = os.path.normpath(
            _toplevel.stdout.strip() if _toplevel is not None and _toplevel.ok and _toplevel.stdout.strip() else cwd
        )
    _root_prefix = _root.rstrip(os.sep) + os.sep

    from fno.graph._intake import project_root_from_settings

    # Memoized per project name: project_root_from_settings re-parses the
    # settings file(s) from disk on every call, and most graphs share a
    # small number of distinct project names across many entries - without
    # this, the any(...) scan below would re-read the same settings file
    # once per entry (review fix, efficiency).
    _settings_root_cache: dict[str, Optional[str]] = {}

    def _effective_cwd(entry: dict) -> Optional[str]:
        # Prefer the LIVE settings-resolved root for the entry's project
        # over its stored `cwd` - the same precedence every other cwd
        # consumer in this codebase uses (graph/cli.py, dispatch.py,
        # backlog/advance.py, pr_watch/_discover.py, provenance/spawn_think.py
        # all read `_resolved_cwd` before `cwd`). A node's stored `cwd` is an
        # intake-time snapshot; the work-map is the current source of truth,
        # and the two can drift (a moved checkout, a re-pointed workspace
        # path) - matching only the stale snapshot would silently misread a
        # node that genuinely belongs here as "not this repo" (review fix).
        project = entry.get("project")
        resolved = None
        if isinstance(project, str) and project:
            if project not in _settings_root_cache:
                _settings_root_cache[project] = project_root_from_settings(project)
            resolved = _settings_root_cache[project]
        raw = resolved or entry.get("cwd")
        return raw if isinstance(raw, str) and raw else None

    def _cwd_in_this_repo(entry: object) -> bool:
        if not isinstance(entry, dict):
            return False
        raw_cwd = _effective_cwd(entry)
        if raw_cwd is None:
            # Neither a live project root nor a stored cwd - can't be proven
            # NOT this repo's, and there is no cost to including it - only
            # candidate_ids (populated below from the ref/trailer match) can
            # ever trigger a gh call.
            return True
        norm = os.path.normpath(os.path.expanduser(raw_cwd))
        return norm == _root or norm.startswith(_root_prefix)

    if not any(_cwd_in_this_repo(entry) for entry in entries):
        # graph.json is a single store shared across every project on the
        # machine - a non-empty by_id only proves SOME project has nodes,
        # never THIS one. Matched on cwd (the same field/normalization
        # detect_project uses to find a node's own project at intake), not
        # on a resolved project-NAME string: an earlier version derived "our
        # project id" independently (settings.toml -> git remote -> dirname)
        # and compared that string against each entry's stored `project`
        # field, but the two schemes read different config and can drift -
        # and a node created via `fno backlog new`, not yet claimed by a
        # plan, legitimately carries `project: null`, which a name-string
        # comparison would silently treat as "not this repo" even when its
        # cwd matches exactly. Matching on cwd sidesteps both failure modes
        # (review fix). Local only (a `git worktree list` read - no
        # network) so this costs nothing extra when it doesn't
        # short-circuit.
        return None

    from fno.graph._reconcile import node_pr_refs

    stamped = any(
        number == pr_number
        for entry in entries
        if isinstance(entry, dict)
        for number, _url in node_pr_refs(entry)
    )

    def _gh_runner(cmd, *, cwd=None, timeout=None, **_ignored):
        # Route through fno.pr._merge.run - the SAME swappable seam every
        # other gh call in the merge path uses - rather than
        # fetch_pr_closure_context's own default subprocess.run. That default
        # bypasses every test fixture's FakeGH mock (which only ever
        # monkeypatches `_merge.run`), so it shelled to the REAL gh binary in
        # dozens of unrelated tests; round-10's fail-open-when-unstamped path
        # masked that by silently swallowing the resulting failure, and
        # round-11's fail-closed fix turned that masked bug into a hard
        # failure across the suite (round-11 review fix, self-caught).
        try:
            result = _merge.run(cmd, cwd=cwd, timeout=timeout)
        except ToolMissing as exc:
            raise FileNotFoundError(str(exc)) from exc
        return result

    try:
        pr_ctx = fetch_pr_closure_context(pr_number, cwd=cwd, runner=_gh_runner)
    except ClosureQueryError as exc:
        # Fail closed unconditionally, matching the graph-read handler above
        # and this module's own stated policy ("hold reads fail closed") - a
        # transient gh blip must never read as "nothing to check" for a
        # trailer-only claimed node (round-11 review fix: an earlier version
        # returned None here when nothing was ref-stamped, silently skipping
        # the hold check for exactly the trailer-only case round-10 added
        # this lookup to catch, just reachable via gh flakiness instead of a
        # design gap).
        raise HoldLookupError(
            f"hold lookup unavailable: gh call failed before PR content was read: {exc}; "
            "retry the hold lookup; do not edit the PR body"
        ) from exc

    ref_node_id: Optional[str] = None
    if stamped:
        ref_node_id = _find_pr_node_id(entries, pr_number, pr_ctx.url or "")
        if ref_node_id is None:
            # _find_pr_node_id collapses ambiguity to None - a safe skip for
            # reconcile, but here None reads as "no binding" and lets the
            # merge proceed even if one of the ambiguous candidates carries an
            # active hold (round-12 finding 2). Surface the candidates and
            # fail closed, the policy this module applies everywhere else.
            from fno.graph._reconcile import repo_slug_from_url

            our_slug = repo_slug_from_url(pr_ctx.url or "")
            if our_slug is not None:
                from fno.pr._merge import _repo_scoped_number_matches

                ambiguous = _repo_scoped_number_matches(
                    entries, pr_number, our_slug
                )
                if len(ambiguous) > 1:
                    raise HoldLookupError(
                        f"PR {pr_number} maps ambiguously to graph nodes "
                        f"{', '.join(ambiguous)} with no discriminating pr_url; "
                        "refusing to assume unheld; disambiguate the entries' "
                        "pr_url and retry"
                    )

    candidate_ids: list[str] = []
    if ref_node_id is not None:
        candidate_ids.append(ref_node_id)
    for claimed in parse_closure_trailer(pr_ctx.body):
        if claimed not in candidate_ids:
            candidate_ids.append(claimed)

    if not candidate_ids:
        # No graph-bound delivery of either kind means there is no Footnote
        # plan hold to read. A same-number node in another repo is
        # deliberately not a match.
        return None

    for node_id in candidate_ids:
        node = by_id.get(node_id)
        if not isinstance(node, dict):
            if node_id == ref_node_id:
                raise HoldLookupError(f"graph node {node_id} disappeared during hold lookup")
            # A trailer can name an id this graph slice does not carry (a
            # typo, or another project's node) - nothing to hold-check.
            continue
        verdict = dispatch_hold_verdict(node, by_id)
        if verdict is not None:
            return verdict
    return None


def merge_hold_reason(pr_number: int, cwd: str) -> Optional[str]:
    """A human-readable refusal shared by sanctioned and direct merge paths.

    A returned reason ALSO disarms any armed GitHub ``--auto`` merge for the
    PR (round-12 finding 3): the queue fires server-side with no re-check, so
    a hold added after arming was silently ignored. Refusal and disarm agree -
    if this path refuses to assume the PR unheld, the queued merge must not
    assume it either. Recovery from a wrong disarm is one re-arm.
    """
    try:
        verdict = hold_for_pr(pr_number, cwd)
    except HoldLookupError as exc:
        _disarm_queued_auto_merge(pr_number, cwd, f"dispatch-hold-invalid: {exc}")
        return f"dispatch-hold-invalid: {exc}; refusing to assume unheld"
    if verdict is None:
        return None
    hold = verdict.hold
    if hold.state is DispatchHoldState.INVALID:
        _disarm_queued_auto_merge(pr_number, cwd, f"{verdict.guard_reason}: {hold.detail}")
        return f"{verdict.guard_reason}: {hold.detail}; refusing to assume unheld"
    _disarm_queued_auto_merge(pr_number, cwd, f"{verdict.guard_reason}: {hold.reason}")
    return (
        f"{verdict.guard_reason}: {hold.reason}; set_by={hold.set_by}; "
        f"release_when={hold.release_when}; review_on={hold.review_on}"
    )


def _disarm_queued_auto_merge(pr_number: int, cwd: str, why: str) -> None:
    """Best-effort ``gh pr merge <n> --disable-auto`` when a hold is seen.

    The stacked-base guard closed its identical TOCTOU window with a CI
    workflow because base lineage is visible to GitHub; a plan hold is not,
    so the closure has to be local - at every reader that refuses. Fires only
    on a positive hold/invalid verdict, never on an unheld read. Any failure
    (no gh, network, gh error) logs one note and returns: the refusal above
    stands regardless, and the operator can re-arm once the hold clears.
    """
    from fno.pr import _merge
    from fno.pr._proc import ToolMissing

    try:
        result = _merge.run(
            ["gh", "pr", "merge", str(pr_number), "--disable-auto"],
            cwd=cwd,
            timeout=15,
        )
    except ToolMissing:
        return
    except Exception as exc:  # noqa: BLE001 - disarm is best-effort, never fatal
        print(f"hold: auto-merge disarm probe failed for PR {pr_number}: {exc}", file=sys.stderr)
        return
    if result.ok:
        print(
            f"hold: disabled an armed auto-merge for PR {pr_number} ({why}); "
            "re-arm with `gh pr merge "
            f"{pr_number} --auto` once the hold clears",
            file=sys.stderr,
        )
    else:
        # A non-zero exit includes the benign "auto-merge not enabled"
        # (nothing armed) AND the dangerous auth/network/permission failure
        # (something armed, disarm NOT landed) - gh's own output is the only
        # thing that distinguishes them, so surface it (codex round on PR
        # 1282). One note, never fatal: the refusal above stands, but the
        # operator must be able to see the queue may still be armed.
        tail = (result.stderr or result.stdout or "").strip().splitlines()
        detail = tail[-1] if tail else f"exit {result.returncode}"
        print(
            f"hold: auto-merge disarm for PR {pr_number} returned nonzero "
            f"({detail}); an armed queue entry may still exist - re-run "
            "`gh pr merge --disable-auto` by hand if auth or network failed",
            file=sys.stderr,
        )


# ---------------------------------------------------------------------------
# The writer: fno do pr hold set|release (one node plan's dispatch_hold).
# Every merge path already reads the block; this is the receipted way to
# author and lift it. The readback is the same ladder reader the merge gate
# uses, so a write that cannot prove itself restores the original bytes.
# ---------------------------------------------------------------------------


class HoldWriteError(RuntimeError):
    """A set/release refusal or failure. ``exit_code``: 2 bad input,
    3 state refusal, 1 write/readback failure (bytes restored)."""

    def __init__(self, message: str, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code


def _resolve_plan_file(node: str, graph_path: Optional[str]) -> tuple[dict, str]:
    from pathlib import Path

    from fno.graph.ladder import resolve_plan_probe
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    entries = read_graph(Path(graph_path) if graph_path else graph_json())
    entry = next(
        (
            e
            for e in entries
            if isinstance(e, dict)
            and node.lower() in (str(e.get("id", "")).lower(), str(e.get("slug", "")).lower())
        ),
        None,
    )
    if entry is None:
        raise HoldWriteError(f"no node resolves to '{node}'", 2)
    probe = resolve_plan_probe(entry)
    if not probe or not os.path.exists(probe):
        raise HoldWriteError(
            f"node {entry.get('id')} has no usable plan file"
            f"{f' ({probe})' if probe else ''}; a merge hold lives in plan "
            "frontmatter, so the node needs a blueprint first",
            2,
        )
    return entry, probe


def _insert_hold_block(text: str, block: str) -> Optional[str]:
    """Insert ``block`` just before the closing ``---`` fence, byte-preserving."""
    lines = text.split("\n")
    if not lines or lines[0] != "---":
        return None
    close = next((i for i in range(1, len(lines)) if lines[i] == "---"), None)
    if close is None:
        return None
    return (
        "\n".join(lines[:close])
        + "\n"
        + block.rstrip("\n")
        + "\n"
        + "\n".join(lines[close:])
    )


def _remove_hold_block(text: str) -> Optional[str]:
    """Remove the ``dispatch_hold`` line and its indented/blank run."""
    lines = text.split("\n")
    if not lines or lines[0] != "---":
        return None
    close = next((i for i in range(1, len(lines)) if lines[i] == "---"), None)
    if close is None:
        return None
    start = next((i for i in range(1, close) if lines[i].startswith("dispatch_hold:")), None)
    if start is None:
        return None
    end = start + 1
    while end < close and (not lines[end] or lines[end][0] in " \t"):
        end += 1
    return "\n".join(lines[:start] + lines[end:])


def _write_proven(plan_text_new: str, probe: str, entry: dict, want_held: bool, original: str) -> None:
    from fno.graph.ladder import DispatchHoldState, dispatch_hold

    tmp = f"{probe}.hold.tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(plan_text_new)
    os.replace(tmp, probe)
    state = dispatch_hold(entry).state
    want = DispatchHoldState.HELD if want_held else DispatchHoldState.ABSENT
    if state is want:
        return
    with open(probe, "w", encoding="utf-8") as fh:
        fh.write(original)
    raise HoldWriteError(
        f"readback answered {state.value} where {want.value} was required; "
        "original bytes restored",
        1,
    )


def _notify_worker(node_id: str, text: str) -> str:
    import subprocess

    for quiet in (False, True):
        args = ["fno", "backlog", "note", node_id] + (["--quiet"] if quiet else []) + [text]
        try:
            code = subprocess.run(args, check=False).returncode
        except OSError:
            return "failed"
        if code == 0:
            return "quiet" if quiet else "mailed"
        if code != 3:
            return "failed"
    return "failed"


def hold_set(
    node: str,
    reason: str,
    release_when: str,
    set_by: str,
    review_on: str = "",
    graph_path: Optional[str] = None,
    notify: bool = True,
) -> dict:
    """Write one node plan's ``dispatch_hold`` block; return the receipt."""
    import datetime

    import yaml

    for name, value in (("reason", reason), ("release_when", release_when), ("set_by", set_by)):
        if not value.strip():
            raise HoldWriteError(f"set needs a non-blank --{name.replace('_', '-')}", 2)
    parsed_review = None
    if review_on:
        try:
            parsed_review = datetime.date.fromisoformat(review_on)
        except ValueError:
            raise HoldWriteError(f"--review-on must parse as YYYY-MM-DD, got: {review_on}", 2)
    else:
        parsed_review = datetime.date.today() + datetime.timedelta(days=7)
    entry, probe = _resolve_plan_file(node, graph_path)
    node_id = str(entry.get("id") or node)
    from fno.graph.ladder import DispatchHoldState, dispatch_hold

    existing = dispatch_hold(entry)
    if existing.state is not DispatchHoldState.ABSENT:
        raise HoldWriteError(
            f"node {node_id} is already held: reason={existing.reason} "
            f"release_when={existing.release_when}; lift it with "
            f"`fno do pr hold release {node_id} --evidence <proof>`",
            3,
        )
    block = yaml.safe_dump(
        {"dispatch_hold": {
            "reason": reason,
            "release_when": release_when,
            "review_on": parsed_review.isoformat(),
            "set_by": set_by,
        }},
        sort_keys=False,
        default_flow_style=False,
    )
    with open(probe, encoding="utf-8") as fh:
        original = fh.read()
    new_text = _insert_hold_block(original, block.rstrip("\n"))
    if new_text is None:
        raise HoldWriteError(f"plan {probe} has no closing --- fence; refusing to edit", 1)
    _write_proven(new_text, probe, entry, True, original)
    pr = entry.get("pr_number")
    disarm = "skipped"
    if isinstance(pr, int) and pr > 0:
        _disarm_queued_auto_merge(pr, str(entry.get("cwd") or os.getcwd()), reason)
        disarm = "best-effort issued"
    notified = (
        "skipped"
        if not notify
        else _notify_worker(
            node_id,
            f"merge hold set on {node_id}: reason={reason} release_when={release_when}; "
            f"merge is refused until it lifts. Lift it with `fno do pr hold release "
            f"{node_id} --evidence <proof>`",
        )
    )
    return {
        "node": node_id,
        "action": "set",
        "plan": probe,
        "hold": {
            "reason": reason,
            "release_when": release_when,
            "review_on": parsed_review.isoformat(),
            "set_by": set_by,
        },
        "pr": pr if isinstance(pr, int) else None,
        "disarm": disarm,
        "notified": notified,
    }


def hold_release(
    node: str,
    evidence: str,
    graph_path: Optional[str] = None,
    notify: bool = True,
) -> dict:
    """Remove one node plan's ``dispatch_hold`` block; return the receipt."""
    if not evidence.strip():
        raise HoldWriteError("release needs a non-blank --evidence", 2)
    entry, probe = _resolve_plan_file(node, graph_path)
    node_id = str(entry.get("id") or node)
    from fno.graph.ladder import DispatchHoldState, dispatch_hold, dispatch_hold_verdict

    if dispatch_hold(entry).state is DispatchHoldState.ABSENT:
        raise HoldWriteError(f"node {node_id} carries no merge hold; nothing to release", 3)
    with open(probe, encoding="utf-8") as fh:
        original = fh.read()
    new_text = _remove_hold_block(original)
    if new_text is None:
        raise HoldWriteError(f"plan {probe} carries no dispatch_hold block to remove", 3)
    _write_proven(new_text, probe, entry, False, original)
    from pathlib import Path

    from fno.graph.store import read_graph as _read_graph
    from fno.paths import graph_json as _graph_json

    by_id = {
        e.get("id"): e
        for e in _read_graph(Path(graph_path) if graph_path else _graph_json())
        if isinstance(e, dict) and e.get("id")
    }
    verdict = dispatch_hold_verdict(entry, by_id)
    return {
        "node": node_id,
        "action": "release",
        "plan": probe,
        "hold": {
            "evidence": evidence,
            "still_held_by": verdict.guard_reason if verdict else None,
        },
        "pr": entry.get("pr_number") if isinstance(entry.get("pr_number"), int) else None,
        "disarm": "skipped",
        "notified": (
            "skipped"
            if not notify
            else _notify_worker(
                node_id,
                f"merge hold released on {node_id}: evidence={evidence}",
            )
        ),
    }

