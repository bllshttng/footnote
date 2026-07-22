"""In-package port of ``scripts/lib/pr-merge.sh`` (ab-d4c98550, US1).

Skill-agnostic PR merge wrapper. Shells to ``gh``/``git`` and preserves the
caller-facing contract verbatim:

- Stdout: one JSON line ``{pr, outcome, reason, strategy}`` on
  merged/queued/skipped; failures print the same JSON shape to STDERR.
- Exit codes: 0 merged|queued, 1 failed (incl. bad args), 2 skipped
  (auto_merge disabled), 127 gh not installed.
- The footnote-canonical merge guard (config.auto_merge ``enabled`` + the
  CI-green / external-review / stub-manifest guards) and the worktree
  server-side-recovery fallback are preserved. The who-may-merge gate
  (``--invoker`` + ``auto_merge.allowed_invokers``) was removed (x-04ab): the
  caller context is derivable and megawalk is deprecated, so the flag was
  redundant ceremony. A legacy ``--invoker=...`` arg is silently accepted and
  ignored so old callers never break.
- Post-merge followups (memory-pass + triage sentinels, the session_satisfied
  event, the per-PR artifact consolidation) fire for merged AND queued,
  best-effort: a followup failure never changes the already-emitted outcome.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import sys
import time
from contextlib import contextmanager
from typing import Iterator, List, Literal, Optional, Sequence

from fno.pr._proc import ToolMissing, run

_PR_RE = re.compile(r"^[1-9][0-9]*$")

# Merge serialization (parallel mode, epic x-42d5 G4, Locked Decision #9):
# builds run parallel, merges run ONE AT A TIME. The lock is held across the
# gh merge call and its post-merge followups (typically seconds), so the wait
# is short and bounded - a peer still holding it past the window is reported
# as "held" (exit 2) for the caller to retry, never an indefinite block.
#
# Scope: the lock + freshness hold cover the IMMEDIATE merge path. A queued
# `--auto` merge (require_checks_pass) only ENQUEUES under the lock - GitHub
# performs the actual merge asynchronously once checks pass, outside any lock,
# so serialization there is delegated to GitHub. Operators running parallel
# lanes with --auto should pair it with branch protection requiring branches
# to be up to date before merging.
_MERGE_LOCK_WAIT_S = 120
_MERGE_LOCK_POLL_S = 5


def _emit(pr: int, outcome: str, reason: str, strategy: str, *, err: bool) -> None:
    """Print the JSON line. ``err`` routes to stderr (failure cases)."""
    obj = {
        "pr": pr,
        "outcome": outcome,
        "reason": reason,
        "strategy": strategy,
    }
    line = json.dumps(obj, separators=(",", ":")) + "\n"
    (sys.stderr if err else sys.stdout).write(line)
    (sys.stderr if err else sys.stdout).flush()


def _gh(args: Sequence[str], cwd: str):
    return run(["gh", *args], cwd=cwd)


def _git(args: Sequence[str], cwd: str):
    return run(["git", *args], cwd=cwd)


def _load_auto_merge():
    from fno.config import load_settings

    return load_settings().auto_merge


def _repo_state_dir(cwd: str) -> str:
    res = _git(["rev-parse", "--show-toplevel"], cwd)
    root = res.stdout.strip() if res.ok and res.stdout.strip() else cwd
    return os.path.join(root, ".fno")


def _read_state_field(state_file: str, field: str) -> str:
    """Read ``field:`` from frontmatter, dequoting a matched pair (parser parity).

    Strips only a MATCHED surrounding quote pair (not a naive unbalanced
    strip that could mangle a value starting/ending with a quote; gemini on
    PR #524).
    """
    try:
        with open(state_file, "r", encoding="utf-8") as fh:
            for ln in fh:
                if ln.startswith(field + ":"):
                    val = ln[len(field) + 1:].strip()
                    if len(val) >= 2 and (
                        (val[0] == '"' and val[-1] == '"')
                        or (val[0] == "'" and val[-1] == "'")
                    ):
                        return val[1:-1]
                    return val
    except OSError:
        pass
    return ""


# ---------------------------------------------------------------------------
# Post-merge side-effects (best-effort; never change the merge outcome)
# ---------------------------------------------------------------------------


def _sync_graph_merge_status(merge_status: str, pr_number: int, cwd: str = "") -> None:
    """Set merge_status on the graph node carrying this pr_number (best-effort)."""
    try:
        from fno.paths import graph_json
        from fno.graph.store import locked_mutate_graph

        path = graph_json()
        if not path.exists():
            return

        def _mut(entries: List[dict]) -> List[dict]:
            for e in entries:
                if e.get("pr_number") == pr_number:
                    e["merge_status"] = merge_status
                    break
            return entries

        locked_mutate_graph(path, _mut)
    except (Exception, SystemExit):
        # Silent no-op on ANY failure (no graph, store error): the bash
        # `|| true`-guarded this, and it must never block the merge outcome.
        # SystemExit is included because locked_mutate_graph calls sys.exit(1)
        # on a corrupt graph.json - a bare `except Exception` would let that
        # abort the merge outcome before the post-merge followups run (codex
        # P2 on PR #524). KeyboardInterrupt is deliberately NOT swallowed.
        pass

    # x-b6e4: stamp ship-phase lifecycle provenance on a REAL merge (not queued/
    # failed) -- the merge primitive is one of the plan's ship boundaries. Gated
    # here so all three merged code paths are covered in one place.
    if merge_status == "merged":
        _stamp_ship_provenance(pr_number, cwd)


def _find_pr_node_id(
    entries: List[dict], pr_number: int, pr_url: str = ""
) -> "Optional[str]":
    """The graph node linked to this PR, resolved robustly (baked-in, no memory).

    Match order: primary/additional ``pr_number``, then ``pr_url``. The url
    fallback is load-bearing: a node linked only by url - or created off a branch
    whose name does not carry the node id - is invisible to bare
    ``fno backlog reconcile`` (its forward scan needs an int ``pr_number`` and its
    reverse map needs the id in the branch name). ``fno pr merge`` knows the exact
    number and url, so it can always find and stamp its own node.
    """
    for e in entries:
        if e.get("pr_number") == pr_number:
            return e.get("id")
        for extra in e.get("additional_prs") or []:
            if isinstance(extra, dict) and extra.get("number") == pr_number:
                return e.get("id")
    url = (pr_url or "").strip()
    if url:
        for e in entries:
            if (e.get("pr_url") or "").strip() == url:
                return e.get("id")
            for extra in e.get("additional_prs") or []:
                if isinstance(extra, dict) and (extra.get("url") or "").strip() == url:
                    return e.get("id")
    return None


def _reconcile_merged_pr_node(pr_number: int, cwd: str = "") -> None:
    """Close the just-merged PR's backlog node synchronously (baked into merge).

    ``_run_post_merge_followups`` only drops a ``.triage-pending`` sentinel for a
    later stop-hook / ritual to consume; a standalone ``fno pr merge`` from a
    worktree or bg session never fires that hook, so the node stays open - the
    exact gap that made ``fno pr merge`` no better than ``gh pr merge``. Close it
    here so the merge always closes its own loop, with no memory/workaround:

      1. Resolve THIS PR's node by number, else url (a url-only / off-convention
         link is invisible to bare reconcile).
      2. Stamp ``pr_number`` on it (idempotent) so the canonical link exists for
         this and every later pass - reconcile's forward scan keys on it.
      3. Run ``fno backlog reconcile --node <id>`` (mark done, stamp the plan,
         drop the retro sentinel) - the full, tested close path, reused not
         duplicated.

    Best-effort: any failure is a non-fatal stderr note; never blocks the merge.
    """
    try:
        from fno.paths import graph_json
        from fno.graph.store import locked_mutate_graph, read_graph

        path = graph_json()
        if not path.exists():
            return
        pr_url = ""
        view = _gh(
            ["pr", "view", str(pr_number), "--json", "url", "-q", ".url"],
            cwd or os.getcwd(),
        )
        if view.ok:
            pr_url = view.stdout.strip()
        nid = _find_pr_node_id(read_graph(path), pr_number, pr_url)
        if not nid:
            return  # no node linked to this PR - nothing to close

        def _mut(entries: List[dict]) -> List[dict]:
            for e in entries:
                if e.get("id") == nid:
                    if e.get("pr_number") != pr_number:
                        e["pr_number"] = pr_number
                    if pr_url and not (e.get("pr_url") or "").strip():
                        e["pr_url"] = pr_url
                    break
            return entries

        locked_mutate_graph(path, _mut)

        from fno import _subprocess_util

        run(
            [*_subprocess_util.fno_py_cmd(), "backlog", "reconcile", "--node", nid],
            cwd=cwd or os.getcwd(),
        )
    except (Exception, SystemExit):
        # Never block the merge outcome on the node-close (mirrors
        # _sync_graph_merge_status: SystemExit covers locked_mutate_graph's
        # sys.exit on a corrupt graph).
        print(
            f"fno pr merge: post-merge node reconcile for PR #{pr_number} "
            "skipped (non-fatal)",
            file=sys.stderr,
        )


def _on_confirmed_merge(pr_number: int, cwd: str = "") -> None:
    """Every graph side-effect of a CONFIRMED (immediate) merge, in one place.

    Sync merge_status + stamp ship provenance (``_sync_graph_merge_status``), then
    close the node (``_reconcile_merged_pr_node``). The three merged code paths
    call this ONE function so the node-close can never be forgotten on one of
    them; the queued/failed paths keep calling ``_sync_graph_merge_status`` alone.
    """
    _sync_graph_merge_status("merged", pr_number, cwd)
    _reconcile_merged_pr_node(pr_number, cwd)


def _repo_slug(cwd: str) -> "Optional[str]":
    """The merge's ``<owner>/<repo>`` slug, or None if gh can't say (x-d5f9).

    Best-effort: a probe miss degrades to None, which reverts ship-stamping to
    the bare-``pr_number`` match - a safe skip in a multi-repo graph, never a
    wrong stamp (Failure Modes: Errors)."""
    try:
        res = _gh(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
                  cwd or os.getcwd())
        slug = res.stdout.strip() if res.ok else ""
        return slug or None
    except Exception:
        return None


def _stamp_ship_provenance(pr_number: int, cwd: str = "") -> None:
    """Append a `ship` lifecycle record to the PR's node for the merging session
    (x-b6e4). Ambient identity of whoever ran `fno pr merge`; resolves the unique
    PR-linked node in THIS repo (x-d5f9: scoped by the repo slug so a same-numbered
    PR in another repo is never stamped). Best-effort: any failure or a missing
    identity is a silent no-op and never blocks the merge outcome.

    When the repo slug cannot be resolved, SKIP rather than fall back to a bare
    pr_number match: in a cross-project graph a lone same-numbered PR in another
    repo would then be stamped on the wrong node (codex P2 on #403). A merge
    cannot have succeeded without `gh`, so an unresolved slug here is a rare
    flake; the node-id ship stamp from pr-creator already covers provenance."""
    try:
        from fno.harness_identity import resolve_harness_identity
        from fno.paths import graph_json
        from fno.graph.store import stamp_session_for_pr

        ident = resolve_harness_identity()
        if not ident.session_id or not ident.harness:
            return
        repo = _repo_slug(cwd)
        if not repo:
            sys.stderr.write(
                f"pr-merge: repo slug unresolved; skipping ship stamp for PR {pr_number} "
                "(a bare match could stamp a same-numbered PR in another repo)\n"
            )
            return
        path = graph_json()
        if not path.exists():
            return
        stamp_session_for_pr(
            path, pr_number, phase="ship",
            harness=ident.harness, session_id=ident.session_id,
            repo=repo,
        )
    except (Exception, SystemExit):
        pass


def _emit_session_satisfied(pr_url: str, state_dir: str) -> None:
    """Emit a session_satisfied{source:pr_merge} event (best-effort)."""
    state_file = os.path.join(state_dir, "target-state.md")
    if not os.path.isfile(state_file):
        return
    sid = _read_state_field(state_file, "session_id")
    if not sid or sid == "null":
        return
    try:
        with open(state_file, "rb") as fh:
            gate_hash = hashlib.md5(fh.read()).hexdigest()
    except OSError:
        return
    if not gate_hash:
        return
    try:
        from pathlib import Path

        from fno.events import append_event, session_satisfied

        event = session_satisfied(
            trigger="pr_merge",
            reason="pr_merged",
            session_id=sid,
            gate_state_hash=gate_hash,
            evidence_url=pr_url or None,
            source="target",
        )
        append_event(event, events_path=Path(state_dir) / "events.jsonl")
    except Exception as exc:  # noqa: BLE001 - best-effort, surface a diagnostic
        sys.stderr.write(
            f"pr-merge: session_satisfied emit failed ({exc}); merge outcome unaffected\n"
        )


def _emit_human_touch_merge(pr_number: int, state_dir: str) -> None:
    """Emit ``human_touch{source:merge}`` for a MANUAL merge (W4 telemetry).

    Only a human at a terminal counts: the autonomous loop's ship gate runs
    this same followup path with no tty and must not inflate the touch count,
    so the gate is stdin-isatty. Best-effort: a failure prints a diagnostic
    and never changes the merge outcome.
    """
    if not sys.stdin.isatty():
        return
    # The CLI already rejects non-positive PR args (_PR_RE); this keeps the
    # helper safe for any future caller (0/negative must never match a node).
    if not isinstance(pr_number, int) or pr_number <= 0:
        return
    node_id = None
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json, resolve_canonical_repo_root

        # The graph is global across projects, so bare PR numbers collide;
        # only nodes homed in THIS repo (node.cwd == canonical root) may
        # claim the touch, and only an UNAMBIGUOUS match does (two same-repo
        # nodes on one number -> resolution=failed, never an arbitrary pick).
        root = str(resolve_canonical_repo_root())
        hits = set()
        for e in read_graph(graph_json()):
            if e.get("cwd") != root:
                continue
            if e.get("pr_number") == pr_number or any(
                isinstance(p, dict) and p.get("number") == pr_number
                for p in e.get("additional_prs") or []
            ):
                hits.add(e.get("id"))
        node_id = hits.pop() if len(hits) == 1 else None
    except Exception:
        node_id = None
    try:
        from pathlib import Path

        from fno.events import _build, append_event

        event = _build(
            "human_touch",
            "target",
            {
                "graph_node_id": node_id,
                "source": "merge",
                "resolution": "ok" if node_id else "failed",
            },
        )
        append_event(event, events_path=Path(state_dir) / "events.jsonl")
    except Exception as exc:  # noqa: BLE001 - best-effort, surface a diagnostic
        sys.stderr.write(
            f"pr-merge: human_touch emit failed ({exc}); merge outcome unaffected\n"
        )


def _run_post_merge_followups(pr_number: int, strategy: str, cwd: str) -> None:
    state_dir = _repo_state_dir(cwd)
    state_file = os.path.join(state_dir, "target-state.md")

    # Memory-pass sentinel.
    try:
        with open(os.path.join(state_dir, ".memory-pass-pending"), "w", encoding="utf-8") as fh:
            fh.write(f"{pr_number}\n")
    except OSError:
        pass

    # Retro-triage fast-path sentinel.
    try:
        mode = "interactive"
        if os.path.isfile(os.path.join(state_dir, "megawalk-state.md")) or os.environ.get(
            "TARGET_MISSION_ID"
        ):
            mode = "autonomous"
        plan_path = _read_state_field(state_file, "plan_path")
        session_id = _read_state_field(state_file, "session_id")
        pr_url = ""
        res = _gh(["pr", "view", str(pr_number), "--json", "url", "-q", ".url"], cwd)
        if res.ok:
            pr_url = res.stdout.strip()
        sentinel = {
            "pr_number": pr_number,
            "pr_url": pr_url,
            "mode": mode,
            "plan_path": plan_path,
            "session_id": session_id,
        }
        with open(os.path.join(state_dir, ".triage-pending"), "w", encoding="utf-8") as fh:
            fh.write(json.dumps(sentinel, separators=(",", ":")))
    except Exception:
        pass

    # Auto-complete signal.
    try:
        pr_url = ""
        res = _gh(["pr", "view", str(pr_number), "--json", "url", "-q", ".url"], cwd)
        if res.ok:
            pr_url = res.stdout.strip()
        _emit_session_satisfied(pr_url, state_dir)
    except Exception:
        pass

    # W4 touch telemetry: a manual (tty) merge is a human steering action.
    try:
        _emit_human_touch_merge(pr_number, state_dir)
    except Exception as exc:
        sys.stderr.write(
            f"pr-merge: human_touch emit failed ({exc}); merge outcome unaffected\n"
        )

    # Per-PR artifact consolidation (best-effort; degrades cleanly when the
    # script is absent, e.g. a bare pip install). The consolidator lives in the
    # PLUGIN tree (`<plugin>/scripts/lib/consolidate-artifacts.sh`), not the
    # target repo, so resolve the plugin root first - else `fno pr merge` run in
    # a footnote-managed target repo would silently skip it every time (codex P2
    # on PR #524). CLAUDE_PLUGIN_ROOT / FNO_REPO_ROOT are read directly (a
    # PRIVATE resolution, not the shared resolve_repo_root/resolve_plugin_script
    # names), so this stays out of scope for the shellout-drift guard (flock-
    # pattern carveout cv-ca99e324 posture); os.path.dirname(state_dir) is the
    # last-resort fallback (correct in the dogfooded footnote repo itself).
    try:
        plugin_root = (
            os.environ.get("CLAUDE_PLUGIN_ROOT")
            or os.environ.get("FNO_REPO_ROOT")
            or os.path.dirname(state_dir)
        )
        script = os.path.join(plugin_root, "scripts", "lib", "consolidate-artifacts.sh")
        if os.path.isfile(script):
            env = dict(os.environ, PR_NUMBER=str(pr_number))
            run(["bash", script], cwd=cwd, env=env)
    except Exception:
        sys.stderr.write(
            "pr-merge: artifact consolidation failed; merge outcome unaffected\n"
        )


# ---------------------------------------------------------------------------
# Merge serialization + stale-base hold (parallel mode G4, LD#9)
# ---------------------------------------------------------------------------


_MergeLockState = Literal["acquired", "held", "unavailable"]


@contextmanager
def _merge_lock() -> Iterator[_MergeLockState]:
    """Serialize merges repo-wide; yield ``acquired`` | ``held`` | ``unavailable``.

    One ``merge:<canonical-root>`` claim per project (repo-local routing, so
    every worktree lane contends on the SAME lock - like ``walker:<root>``),
    pid-liveness anchored so a crashed merger frees it instantly. Acquisition
    polls for up to ``_MERGE_LOCK_WAIT_S`` (a merge holds it for seconds), then
    yields ``held``. A claims-layer error yields ``unavailable`` and the merge
    proceeds unserialized: the lock is coordination, GitHub stays the merge
    authority, and our own tooling failing must never block a merge.
    """
    state: Literal["acquired", "held", "unavailable"] = "acquired"
    key = holder = release = None
    # Acquisition happens fully BEFORE the yield: an exception the consumer
    # body throws into the generator must reach the finally-release, never an
    # except-then-yield-again (which would RuntimeError inside contextmanager).
    try:
        from fno.claims.core import ClaimHeldByOther, acquire_claim, release_claim
        from fno.paths import resolve_canonical_repo_root

        key = f"merge:{resolve_canonical_repo_root()}"
        holder = f"pr-merge:{os.getpid()}"
        deadline = time.monotonic() + _MERGE_LOCK_WAIT_S
        while True:
            try:
                acquire_claim(key, holder, reason="serialized PR merge (LD#9)")
                release = release_claim
                break
            except ClaimHeldByOther:
                if time.monotonic() >= deadline:
                    state = "held"
                    break
                time.sleep(_MERGE_LOCK_POLL_S)
    except Exception as exc:  # noqa: BLE001 - fail-open: lock is best-effort
        sys.stderr.write(f"pr-merge: merge lock unavailable ({exc}); proceeding\n")
        state = "unavailable"
    try:
        yield state
    finally:
        if release is not None and state == "acquired":
            assert key is not None and holder is not None  # set together before release
            try:
                release(key, holder)
            except Exception:  # noqa: BLE001 - pid-liveness frees it anyway
                pass


def _live_lane_count() -> int:
    """Live parallel-lane slots (0 on any probe miss, keeping sequential paths
    byte-identical: the stale-base hold below only arms while lanes run)."""
    try:
        from fno.claims.lanes import active_lane_count

        return active_lane_count()
    except Exception as exc:  # noqa: BLE001
        # A probe miss disarms the stale-base hold entirely - leave the audit
        # breadcrumb so an unguarded merge is distinguishable after the fact.
        sys.stderr.write(
            f"pr-merge: lane probe unavailable ({exc}); merging without freshness hold\n"
        )
        return 0


def _behind_by(pr_number: int, cwd: str) -> int:
    """Commits the PR head is behind its base branch. 0 on any probe miss:
    the hold must never block a merge because our own read failed, but each
    miss leaves a stderr breadcrumb - a gh outage is likeliest exactly when
    many lanes hammer gh, i.e. when the hold matters most."""

    def _miss(why: str) -> int:
        sys.stderr.write(
            f"pr-merge: stale-base probe unavailable ({why}); "
            "merging without freshness hold\n"
        )
        return 0

    try:
        view = _gh(
            ["pr", "view", str(pr_number), "--json", "baseRefName,headRefName"], cwd
        )
        if not view.ok:
            return _miss("gh pr view failed")
        try:
            refs = json.loads(view.stdout or "{}")
        except json.JSONDecodeError:
            return _miss("unparseable pr view output")
        base = refs.get("baseRefName") if isinstance(refs, dict) else None
        head = refs.get("headRefName") if isinstance(refs, dict) else None
        if not base or not head:
            return _miss("missing base/head ref")
        res = _gh(
            ["api", f"repos/{{owner}}/{{repo}}/compare/{base}...{head}", "-q", ".behind_by"],
            cwd,
        )
        if not res.ok:
            return _miss("gh compare failed")
        return int(res.stdout.strip())
    except Exception as exc:  # noqa: BLE001 - the hold must never BLOCK a merge
        return _miss(f"probe error: {exc}")


# ---------------------------------------------------------------------------
# Main flow
# ---------------------------------------------------------------------------


def run_merge(argv: Sequence[str], cwd: Optional[str] = None) -> int:
    repo = cwd or os.getcwd()
    pr_raw = ""
    for arg in argv:
        # A legacy ``--invoker=...`` is silently accepted and ignored (x-04ab
        # removed the flag + its gate). Never break a merge command on a stray
        # flag an un-updated caller still passes.
        if arg.startswith("--invoker="):
            continue
        elif arg[:1].isdigit():
            pr_raw = arg
        else:
            sys.stderr.write(f"Error: unknown arg '{arg}'\n")
            return 1

    if not pr_raw:
        sys.stderr.write("pr_number required\n")
        return 1

    # pr_number must be a positive integer.
    if not _PR_RE.match(pr_raw):
        _emit(0, "failed", f"invalid pr number: {pr_raw}", "none", err=True)
        return 1
    pr_number = int(pr_raw)

    # (0) Stub-manifest hold: a `contract`-tier dependent's PR must not merge
    # while it carries an unreconciled stub-manifest (mocks would ship). Checked
    # BEFORE the auto_merge gate so auto-merge cannot bypass it (AC7-EDGE), and
    # it no-ops for every non-contract PR so the default `hard` path is unchanged
    # (AC6-EDGE).
    try:
        from fno.stub_manifest import unreconciled_manifest_for_pr

        # Resolve the repo top-level: manifests are written under the PROJECT
        # root's `.fno/`, so a merge invoked from a subdirectory must not look
        # under that subdir (codex P2). Falls back to `repo` if git can't say.
        top = _git(["rev-parse", "--show-toplevel"], repo)
        root = top.stdout.strip() if top.ok and top.stdout.strip() else repo
        held = unreconciled_manifest_for_pr(pr_number, root)
    except Exception:
        held = None  # never let the guard's own failure block a normal merge
    if held:
        if held.get("_malformed"):
            detail = "malformed stub-manifest (cannot prove stubs are gone)"
        else:
            detail = f"unreconciled stub-manifest ({len(held.get('stubs', []))} stub(s))"
        _emit(
            pr_number,
            "held",
            f"contract dependent {held.get('_node')} carries a {detail}; "
            "reconcile before merge",
            "none",
            err=False,
        )
        return 2

    # (1) Short-circuit if auto-merge is disabled. The who-may-merge gate
    # (--invoker + allowed_invokers) was removed (x-04ab): auto-merge is gated
    # by `enabled` plus the CI-green / external-review / stub-manifest guards.
    auto_merge = _load_auto_merge()
    if not auto_merge.enabled:
        _emit(
            pr_number,
            "skipped",
            "auto_merge disabled",
            "none",
            err=False,
        )
        return 2

    # (1b) Honor THIS run's resolved decision, not just the project policy.
    # `auto_merge.enabled` is standing policy; the manifest's
    # `auto_merge_approved` is what init resolved after folding in the per-run
    # modifiers, and a per-run `no-merge` (which `/target bg` injects by
    # default) sets it false while `enabled` stays true. Without this the
    # sanctioned verb is a WEAKER gate than raw `gh pr merge`, which the
    # git-protection hook already guards on this same field.
    # Absent manifest or absent field -> proceed: a manual `fno pr merge`
    # outside a target session is legitimate and must not start refusing.
    approved = _read_state_field(
        os.path.join(_repo_state_dir(repo), "target-state.md"),
        "auto_merge_approved",
    )
    if approved and approved.strip().lower() not in ("true", "yes", "1"):
        _emit(
            pr_number,
            "skipped",
            "per-run no-merge (manifest auto_merge_approved is not true)",
            "none",
            err=False,
        )
        return 2

    # (2) gh must be installed.
    if shutil.which("gh") is None:
        _emit(pr_number, "failed", "gh CLI not installed", "none", err=True)
        return 127

    # (2b) Merge serialization + stale-base hold (parallel mode G4, LD#9).
    # Builds run parallel; merges run one at a time, and while lanes are live a
    # PR whose head is behind its base is held for `fno pr rebase` first, so a
    # lane never merges a stale base. Both checks run UNDER the lock: a peer
    # merge landing between the freshness read and our merge is exactly the
    # race the lock exists to close. Sequential runs (no live lanes) skip the
    # freshness hold and see only an uncontended lock - behavior unchanged.
    with _merge_lock() as lock:
        if lock == "held":
            _emit(
                pr_number,
                "held",
                "merge serialized: another merge holds the lock; retry",
                "none",
                err=False,
            )
            return 2
        if _live_lane_count() > 0:
            behind = _behind_by(pr_number, repo)
            if behind > 0:
                _emit(
                    pr_number,
                    "held",
                    f"stale base: head is {behind} commit(s) behind base with "
                    "parallel lanes live; run fno pr rebase, then retry",
                    "none",
                    err=False,
                )
                return 2
        return _do_merge(pr_number, auto_merge, repo)


def _do_merge(pr_number: int, auto_merge, repo: str) -> int:
    """Steps (3)-(4): build + run the gh merge and classify the outcome."""
    # (3) Build command.
    strategy = auto_merge.merge_strategy
    cmd: List[str] = ["pr", "merge", str(pr_number), f"--{strategy}"]
    if auto_merge.delete_branch_on_merge:
        cmd.append("--delete-branch")
    if auto_merge.require_checks_pass:
        cmd.append("--auto")

    # (4) Run + classify.
    try:
        res = _gh(cmd, repo)
    except ToolMissing:
        _emit(pr_number, "failed", "gh CLI not installed", "none", err=True)
        return 127

    output = (res.stdout or "") + (res.stderr or "")
    if res.ok:
        if re.search(r"will be automatically merged", output, re.IGNORECASE):
            _emit(pr_number, "queued", "awaiting required checks", strategy, err=False)
            _sync_graph_merge_status("queued", pr_number)
        else:
            _emit(pr_number, "merged", "merged immediately", strategy, err=False)
            _on_confirmed_merge(pr_number, repo)
        _run_post_merge_followups(pr_number, strategy, repo)
        return 0

    # Failure path. A worktree-local post-merge step can fail even though the
    # SERVER-SIDE merge already landed (recurring PR #393/#395 bite).
    first_line = output.splitlines()[0][:200] if output.strip() else ""
    if re.search(r"is already used by worktree|already checked out", output, re.IGNORECASE):
        # (a) Server-side merge already landed -> cosmetic local failure.
        merged_at = ""
        view = _gh(["pr", "view", str(pr_number), "--json", "mergedAt", "-q", ".mergedAt"], repo)
        if view.ok:
            merged_at = view.stdout.strip()
        if merged_at and merged_at != "null":
            _git(["fetch", "origin"], repo)
            _emit(
                pr_number,
                "merged",
                "merged server-side; local post-merge step skipped in worktree",
                strategy,
                err=False,
            )
            _on_confirmed_merge(pr_number, repo)
            _run_post_merge_followups(pr_number, strategy, repo)
            return 0
        # (b) Not merged yet -> merge SERVER-SIDE via the API (no local checkout).
        api = _gh(
            [
                "api",
                "--method",
                "PUT",
                f"repos/{{owner}}/{{repo}}/pulls/{pr_number}/merge",
                "-f",
                f"merge_method={strategy}",
            ],
            repo,
        )
        if api.ok:
            _git(["fetch", "origin"], repo)
            _emit(
                pr_number,
                "merged",
                "merged server-side (worktree fallback)",
                strategy,
                err=False,
            )
            _on_confirmed_merge(pr_number, repo)
            _run_post_merge_followups(pr_number, strategy, repo)
            return 0

    # Unrecovered failure: classify and report.
    reason = first_line
    if re.search(r"protected", output, re.IGNORECASE):
        reason = "branch protected"
    elif re.search(r"not mergeable", output, re.IGNORECASE):
        reason = "not mergeable (conflicts or base changed)"
    elif re.search(r"required review", output, re.IGNORECASE):
        reason = "required review pending"
    _emit(pr_number, "failed", reason, strategy, err=True)
    _sync_graph_merge_status("failed", pr_number)
    return 1
