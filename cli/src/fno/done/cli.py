"""`fno done` - mark a backlog node as done.

For `domain: code` with no explicit flags, auto-detects the node from the
current git branch and fills pr_number/pr_url/merge_status from `gh pr view`.
For any domain, populates user-supplied fields (--pr, --link, --note) and
always sets `status: done` + `completed_at`.

Beyond the direct flags, `fno done` ALSO rolls up ledger-sourced lifecycle
fields so a "done" graph entry reflects everything we know about the
completed feature, not just the PR trio:

    session_id      - from $CLAUDECODE_SESSION_ID if set, else latest ledger
    cost_usd        - sum of all matching ledger entries' cost_usd
    cost_sessions   - one row per (ledger entry, distinct session). A ledger
                      row's `sessions` list is an alias set for one run, so
                      members that merely rename that run are collapsed first.
    points          - from ledger.points if currently null in graph

A --backfill flag runs ONLY the rollup (skipping status / completed_at
changes), for sweeping already-done nodes that were marked done before
this logic existed. Pair with --force-overwrite for explicit re-reconciliation
of stale rollups (e.g. session_id or points changed since the node was marked done).

When a second `fno done` call races a node that is already done, the status
and completed_at are preserved. User-supplied --pr/--link/--note are still applied.
A done_race_collision event is emitted to events.jsonl for forensic audit.

Never hand-rolls graph writes - goes through locked_mutate_graph for safety.

Registered in fno.cli via `app.command(name="done")(done_command)` so
the main app treats `fno done <args>` as a single command, not a sub-app.
"""
from __future__ import annotations

import json
import os
import subprocess
from datetime import datetime, timezone
from typing import Optional

import typer

from fno.events import (
    SchemaUnavailableError as _SchemaUnavailableError,
    ValidationError as _ValidationError,
)
from fno.graph._reconcile import (
    node_pr_refs,
    pr_number_from_url,
    pr_url_for_repo,
    repo_slug_from_url,
    resolve_merge_evidence,
    resolve_promise_evidence,
)
from fno.graph.fuzzy import resolve_id
from fno.graph.store import locked_mutate_graph, normalize_plan_path, read_graph


def _path_graph():
    """Re-resolve GRAPH_JSON on each call so monkeypatches land in tests."""
    from fno.graph._constants import GRAPH_JSON
    return GRAPH_JSON


def _path_ledger():
    """Re-resolve LEDGER_JSON on each call so monkeypatches land in tests."""
    from fno.graph._constants import LEDGER_JSON
    return LEDGER_JSON


# -- subprocess helpers (subprocess.run is attribute-access so tests can stub) --


def _current_branch() -> Optional[str]:
    try:
        r = subprocess.run(
            ["git", "branch", "--show-current"],
            capture_output=True, text=True, check=False,
        )
    except OSError:
        return None
    b = (r.stdout or "").strip()
    return b or None


def _current_pr(announce_failure: bool = False) -> tuple[Optional[int], Optional[str]]:
    """Return (pr_number, pr_url) for the current branch, or (None, None).

    When `announce_failure=True` and `gh pr view` exits non-zero (or the
    subprocess raises), the captured stderr is printed to sys.stderr prefixed
    with ``fno done: gh pr view failed:``.  This surface is omitted when the
    caller has an explicit --pr/--link/--note (the caller gate short-circuits
    before reaching this function in that case).

    The rc=0 + unparseable-stdout path intentionally stays silent: that means
    "no PR for this branch", not a subprocess error.
    """
    import sys

    try:
        r = subprocess.run(
            [
                "gh", "pr", "view",
                "--json", "number,url",
                "--jq", r'"\(.number) \(.url)"',
            ],
            capture_output=True, text=True, check=False,
        )
    except OSError as exc:
        if announce_failure:
            print(
                f"fno done: gh pr view failed: {exc}",
                file=sys.stderr,
            )
        return None, None
    if r.returncode != 0:
        if announce_failure:
            diagnostic = (r.stderr or "").strip()
            # Truncate to first 4 KB so we don't flood the terminal.
            if len(diagnostic) > 4096:
                diagnostic = diagnostic[:4096] + " [truncated]"
            msg = f"fno done: gh pr view failed: {diagnostic}" if diagnostic else "fno done: gh pr view failed (no diagnostic available)"
            print(msg, file=sys.stderr)
        return None, None
    # rc=0: "no PR for this branch" if stdout is unparseable -- stay silent.
    parts = (r.stdout or "").strip().split(" ", 1)
    if len(parts) != 2:
        return None, None
    try:
        return int(parts[0]), parts[1]
    except ValueError:
        return None, None




# -- ledger rollup --
# plan_path normalization lives in fno.graph.store (normalize_plan_path) so the
# ledger rollup, the second-binding refusal, and the maintain check all compare
# one way.


def _load_ledger_entries() -> list[dict]:
    ledger_path = _path_ledger()
    try:
        if not ledger_path.exists():
            return []
        data = json.loads(ledger_path.read_text())
    except (OSError, json.JSONDecodeError):
        return []
    # Tolerate both the canonical `{"entries": [...]}` envelope and a legacy
    # flat-list ledger. cost._append_to_ledger now writes the envelope, but
    # older checkouts on disk may still carry the bare list shape.
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        entries = data.get("entries", [])
        return entries if isinstance(entries, list) else []
    return []


def _rollup_from_ledger(node: Optional[dict]) -> dict:
    """Aggregate session_id / cost_usd / cost_sessions / points from ledger.

    Returns a dict with keys: session_id, cost_usd, cost_sessions, points.
    Any field for which the ledger has no information is returned as None
    (or [] for cost_sessions) so the caller can preserve existing graph
    values instead of nulling them out.

    Takes the whole NODE, not its ``plan_path``. The rollup matches ledger rows
    on plan_path, so N nodes sharing one plan each claim the same cost, points,
    and session_id - the flat project sum at ``fno backlog`` project-cost then
    counts one run N times. A contained node must therefore be suppressed here
    (x-e957 task 1.4), and a plan_path parameter cannot see that. Passing the
    node makes the containment read structurally unskippable: there is no
    signature a caller can satisfy while bypassing the guard.

    Suppression is empty-handed, not zeroed: ``cost_usd`` stays None,
    ``cost_sessions`` [], ``points`` None. The delivery unit carries the whole
    figure, and contained nodes contributing NOTHING is what keeps the flat
    project sum correct with no dedup logic of its own.
    """
    if not isinstance(node, dict):
        return {"session_id": None, "cost_usd": None, "cost_sessions": [], "points": None}

    # Session id and points ride the same return as cost, so they must be
    # suppressed together - a contained node granted the session id would claim
    # a run it never made, and the metrics backfills cross-reference that.
    contained = node.get("contained_in")
    if isinstance(contained, str) and contained:
        return {"session_id": None, "cost_usd": None, "cost_sessions": [], "points": None}

    plan_path = node.get("plan_path")
    if not plan_path:
        return {"session_id": None, "cost_usd": None, "cost_sessions": [], "points": None}

    target = normalize_plan_path(plan_path)
    ledger_entries = _load_ledger_entries()
    matching = [
        le for le in ledger_entries
        if isinstance(le, dict) and normalize_plan_path(le.get("plan_path")) == target
    ]
    if not matching:
        return {"session_id": None, "cost_usd": None, "cost_sessions": [], "points": None}

    # One cost_sessions row per ledger row, carrying that row's whole cost.
    # A row's `sessions` is an ALIAS SET for ONE run, not a list of sessions:
    # cost/_register.py records up to five identifier forms of the same
    # session (the minted run id, the harness id, a claude transcript uuid, a
    # codex thread id), and its only other writer emits a single-element list.
    # So len(sessions) counts NAMES and dividing by it splits one run's cost
    # across its own aliases. Two sessions on a node means two ledger rows,
    # which `matching` already iterates.
    # Key the row by the scalar run id - the identifier every other footnote
    # surface joins on - falling back to the first alias, then to None so a
    # row with no identity at all still reports its cost.
    cost_sessions: list[dict] = []
    for le in matching:
        sessions = le.get("sessions") or []
        if not isinstance(sessions, list):
            sessions = []
        cost = le.get("cost_usd")
        try:
            cost_f = float(cost) if cost is not None else 0.0
        except (TypeError, ValueError):
            cost_f = 0.0
        sid = le.get("fno_id") or le.get("session_id") or (sessions[0] if sessions else None)
        cost_sessions.append({
            "session_id": sid,
            "cost_usd": round(cost_f, 4),
            "timestamp": le.get("completed") or le.get("started"),
        })

    # Latest session: the most-recent-completed ledger entry, resolved to the
    # SAME scalar the cost row above keys on. Taking the last alias instead put
    # a session id on the node that owns no cost row whenever the alias order
    # does not end on the scalar, which the metrics backfills cross-reference.
    def _sort_key(le: dict) -> str:
        return (le.get("completed") or le.get("started") or "") or ""

    latest = max(matching, key=_sort_key)
    latest_sessions = latest.get("sessions")
    if not isinstance(latest_sessions, list):
        latest_sessions = []
    session_id = (
        latest.get("fno_id")
        or latest.get("session_id")
        or (latest_sessions[-1] if latest_sessions else None)
    )

    # Points: first non-null points field across matching entries. Sometimes
    # ledger has points and graph doesn't (intake-time lookup missed the ledger).
    points = None
    for le in matching:
        p = le.get("points")
        if p is not None:
            points = p
            break

    cost_usd_total = round(sum(s["cost_usd"] for s in cost_sessions), 4)
    return {
        "session_id": session_id,
        "cost_usd": cost_usd_total if cost_sessions else None,
        "cost_sessions": cost_sessions,
        "points": points,
    }


# -- command --


def _apply_rollup(
    entry: dict,
    rollup: dict,
    *,
    env_session: Optional[str] = None,
    force_overwrite: bool = False,
) -> list[str]:
    """Apply ledger rollup + env session to `entry` in place.

    Returns a list of human-readable tags describing what was filled in.
    Default: "fill if null" semantics - pre-existing values are preserved.
    When force_overwrite=True: overwrites session_id and points unconditionally
    (use with --backfill for explicit re-reconciliation of stale rollups).
    Cost rows de-dupe on (session_id, timestamp) regardless of force_overwrite.
    That key catches a re-run of the same rollup, which is what it is for. It
    does NOT make double-counting impossible: two rows for one run recorded
    under different identifier aliases pass as distinct, and one session
    re-stamped at a later recording time passes as distinct too. The alias half
    is handled upstream in `_rollup_from_ledger`; the timestamp-drift half is
    still guarded ad hoc on the reconcile path in graph/cli.py.

    That upstream fix reaches NEW rows only. A node already stamped under the
    old divided-alias model keeps its split breakdown forever, including under
    `--force-overwrite`: the scalar run id was one of the aliases and carries
    the same timestamp, so the corrected full-cost row collides with the row
    already there and is dropped as a duplicate. Totals stay correct either way
    (an even split sums back), which is why this ships without a migration -
    but do not read `--force-overwrite` as a repair for those breakdowns.
    """
    tags: list[str] = []

    # session_id source:
    #   - force_overwrite=False (normal): env_session takes precedence over
    #     ledger so first-time marking captures the current session id even
    #     before the ledger rolls up.
    #   - force_overwrite=True (explicit re-reconciliation): trust the ledger
    #     exclusively. Otherwise `fno done --backfill --force-overwrite` from
    #     an active session (CLAUDECODE_SESSION_ID set) would mass-rewrite
    #     every reconciled node's session_id to the CURRENT session id instead
    #     of the historical ledger attribution, defeating the flag's purpose.
    if force_overwrite:
        new_sid = rollup.get("session_id")
    else:
        new_sid = env_session or rollup.get("session_id")
    if new_sid and (not entry.get("session_id") or force_overwrite):
        entry["session_id"] = new_sid
        tags.append(f"session={new_sid[:8]}")

    # cost_sessions: merge ledger-derived rows with any existing loop-recorded
    # rows, de-duplicated by session_id + timestamp pair.
    rollup_sessions = rollup.get("cost_sessions") or []
    if rollup_sessions:
        existing = entry.get("cost_sessions") or []
        seen = {(s.get("session_id"), s.get("timestamp")) for s in existing}
        added = 0
        for row in rollup_sessions:
            key = (row.get("session_id"), row.get("timestamp"))
            if key not in seen:
                existing.append(row)
                seen.add(key)
                added += 1
        if added:
            entry["cost_sessions"] = existing
            entry["cost_usd"] = round(
                sum(float(s.get("cost_usd") or 0) for s in existing), 4
            )
            tags.append(f"${entry['cost_usd']:.2f}")

    # points: fill only if null, or overwrite when forced
    if rollup.get("points") is not None and (entry.get("points") is None or force_overwrite):
        entry["points"] = rollup["points"]
        tags.append(f"points={rollup['points']}")

    return tags


def _gh_query(pr_number, **kwargs):
    """Query REST for PR merge state. Injectable test seam, mirroring graph.cli."""
    from fno.graph._reconcile import PrMergeState, ReconcileError
    from fno.pr._rest import fetch_pr_info_rest

    info, reason = fetch_pr_info_rest(
        str(pr_number), cwd=kwargs.get("cwd"), repo=kwargs.get("repo")
    )
    if info is None:
        raise ReconcileError(reason)
    return PrMergeState(
        number=info["pr"],
        state=info["state"],
        url=info["url"],
        merged_at=info["merged_at"],
    )


def done_command(
    query: Optional[str] = typer.Argument(
        None,
        help="Graph node id (ab-xxx) or title substring. Omit to auto-detect from git branch.",
    ),
    pr: Optional[int] = typer.Option(
        None, "--pr-number", "--pr", "-p", help="PR number (for code-domain completions)."
    ),
    pr_url: Optional[str] = typer.Option(
        None, "--pr-url",
        help="PR URL. Derived from the repo when omitted; supply it when the repo slug cannot be resolved.",
    ),
    link: Optional[str] = typer.Option(
        None, "--link", "--url", "-l",
        help="Artifact URL (Figma/Canva/Obsidian/any) - sets artifact_url.",
    ),
    note: Optional[str] = typer.Option(
        None, "--note", "-m", help="Free-text completion note - sets completion_note.",
    ),
    backfill: bool = typer.Option(
        False, "--backfill",
        help=(
            "Run ONLY the ledger-rollup (session_id, cost_usd, cost_sessions, "
            "points). Does not flip status or completed_at. With no QUERY, "
            "sweeps every node with status=done."
        ),
    ),
    force_overwrite: bool = typer.Option(
        False, "--force-overwrite",
        help="Overwrite existing rollup fields instead of fill-if-null. Use with --backfill for explicit re-reconciliation of stale rollups.",
    ),
) -> None:
    """Mark a backlog node as done.

    For code-domain nodes with no explicit flags, auto-detects the node from
    the current git branch and fills PR metadata via `gh pr view`. Also rolls
    up session_id / cost_usd / cost_sessions / points from `ledger.json`.

    With `--backfill`, runs the rollup without flipping status - useful for
    reconciling nodes that were marked done before the rollup logic existed.
    """
    # Deprecation shim (unit 6): `fno backlog done` is the canonical closing
    # verb, but it is NOT this command. It takes a node id only, and its whole
    # flag surface is --skip-stamp / -F --force / -R --reason. Six options and
    # the title-substring / branch-autodetect query live only here, so the
    # notice names the gap instead of promising a drop-in swap. Removing this
    # spelling means porting them first. The list is canonical spellings only:
    # --pr and --url are themselves hidden deprecated aliases, so naming them
    # here would point a caller at a second doomed spelling. Only this stderr
    # line is added, because callers parse stdout.
    typer.echo(
        "fno done is deprecated; use `fno backlog done <node-id>` to close a "
        "node. The flag surfaces differ: --backfill, --force-overwrite, "
        "--pr-number, --pr-url, --link, --note and the title/branch query "
        "exist only here and have no destination yet, so this spelling stays "
        "until they are ported.",
        err=True,
    )
    graph_path = _path_graph()
    env_session = os.environ.get("CLAUDECODE_SESSION_ID") or None

    # -- backfill mode (no status change, may be batch) --
    if backfill:
        # The sweep enumerates done nodes from the graph store and writes
        # rollup fields back into it. Under an external backend the done
        # roster lives in the tracker and the rollup lives in the footnote
        # sidecar, so both halves target the wrong store; the per-node door
        # above already fills sidecar rollups at completion time.
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            typer.echo(
                "fno done --backfill: refused - the sweep reads and writes "
                "the graph store, which is not selected (rollups live in the "
                "footnote sidecar under an external backend)",
                err=True,
            )
            raise typer.Exit(code=1)
        entries = read_graph(graph_path)
        if query:
            branch = _current_branch()
            match = resolve_id(query, entries, git_branch=branch)
            if match.kind == "none":
                typer.echo(
                    f"fno done --backfill: no match for {query!r}", err=True,
                )
                raise typer.Exit(code=2)
            if match.kind == "ambiguous":
                typer.echo(
                    f"fno done --backfill: {len(match.candidates)} candidates "
                    f"for {query!r}:",
                    err=True,
                )
                for c in match.candidates:
                    typer.echo(
                        f"  {c.get('id'):<14} {c.get('status', '?'):<9} "
                        f"{c.get('title', '')}",
                        err=True,
                    )
                raise typer.Exit(code=2)
            target_ids = {match.id}
        else:
            target_ids = {
                e.get("id") for e in entries
                if e.get("status") == "done" and e.get("id")
            }

        if not target_ids:
            typer.echo("fno done --backfill: no done nodes to backfill")
            return

        # Pre-compute rollups outside the mutator (ledger I/O stays unlocked).
        rollups: dict[str, dict] = {}
        for e in entries:
            if e.get("id") in target_ids:
                rollups[e["id"]] = _rollup_from_ledger(e)

        touched: list[tuple[str, list[str]]] = []

        def _backfill_mutator(entries_inner):
            for e in entries_inner:
                eid = e.get("id")
                if eid not in target_ids:
                    continue
                tags = _apply_rollup(e, rollups.get(eid, {}), env_session=env_session, force_overwrite=force_overwrite)
                if tags:
                    touched.append((eid, tags))
            return entries_inner

        locked_mutate_graph(graph_path, _backfill_mutator)

        if not touched:
            typer.echo(
                f"fno done --backfill: scanned {len(target_ids)} node(s); "
                "nothing to fill (all fields already set or ledger silent)."
            )
        else:
            for eid, tags in touched:
                typer.echo(f"  {eid}: {'  '.join(tags)}")
            typer.echo(
                f"fno done --backfill: updated {len(touched)} / "
                f"{len(target_ids)} node(s)"
            )
        return

    # -- normal flow: resolve + flip status + rollup --
    # External backend (task 4.1): this front door routes through the SAME
    # shared terminal as `backlog done` - identical gates, sidecar rollups
    # before the close, exactly one tracker.close - instead of the local
    # graph resolution below. Requires an explicit node id: the branch-based
    # auto-detect resolves through footnote-minted slug metadata.
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        from fno.graph.cli import _done_via_seam

        # Graph-backend completions the seam cannot record: the sidecar has no
        # artifact_url/completion_note fields and the gates run on the
        # sidecar's stored refs. Refuse rather than silently drop them.
        if pr or pr_url or link or note:
            typer.echo(
                "fno done: --pr/--pr-url/--link/--note record graph-backend "
                "completion metadata and cannot be recorded under an external "
                "tracker backend",
                err=True,
            )
            raise typer.Exit(code=2)
        if not query:
            typer.echo(
                "fno done: an external tracker backend needs an explicit node "
                "id (branch auto-detect is footnote-metadata-based)",
                err=True,
            )
            raise typer.Exit(code=2)
        _done_via_seam(query, skip_stamp=False, force=False, reason=None)
        return

    entries = read_graph(graph_path)
    branch = _current_branch()
    match = resolve_id(query, entries, git_branch=branch)

    if match.kind == "none":
        msg_target = query or branch or "<no input>"
        typer.echo(f"fno done: no match for {msg_target!r}", err=True)
        if match.note:
            typer.echo(f"  ({match.note})", err=True)
        raise typer.Exit(code=2)

    if match.kind == "ambiguous":
        typer.echo(
            f"fno done: {len(match.candidates)} candidates for {query!r}:",
            err=True,
        )
        for c in match.candidates:
            line = (
                f"  {c.get('id'):<14} {c.get('status', '?'):<9} "
                f"{c.get('title', '')}"
            )
            typer.echo(line, err=True)
        raise typer.Exit(code=2)

    # A url with no number is not a PR link, and silently dropping it would
    # mark the node done while discarding the operator's only evidence.
    if pr_url is not None and pr is None:
        typer.echo("fno done: --pr-url requires --pr", err=True)
        raise typer.Exit(code=2)

    node_id = match.id
    assert node_id is not None  # the exact-match guard above ensures a resolved id
    node = next(e for e in entries if e.get("id") == node_id)
    domain = node.get("domain") or "code"

    # Auto-detect PR only for code domain AND only when the user passed no
    # explicit artifact signal. Non-code domains never auto-detect a PR.
    auto_url: Optional[str] = None
    if domain == "code" and pr is None and link is None and note is None:
        pr, auto_url = _current_pr(announce_failure=True)

    # Require some completion signal for non-code when nothing resolves.
    if (
        domain != "code"
        and pr is None
        and link is None
        and note is None
    ):
        typer.echo(
            f"fno done: {node_id} is domain={domain}; "
            "pass --link, --note, or --pr to mark it done.",
            err=True,
        )
        raise typer.Exit(code=2)

    now = datetime.now(timezone.utc).isoformat()

    # -- merge gate --
    # An explicit --pr REPLACES the node's refs as the evidence: it is the PR
    # the operator is closing on, so a stale merged ref must not authorize a
    # close whose new PR is still open. It is scoped to the NODE's repo (its
    # stored url, else its cwd), never the caller's checkout, so running from
    # another checkout cannot close the node on a same-numbered stranger.
    # With no --pr the node's own refs are the evidence, exactly as cmd_done
    # reads them - keying on the argument alone would reopen the original
    # bypass whenever gh auto-detect fails, and let --note close an open PR.
    # A node with no ref anywhere has nothing to gate (--link/--note).
    #
    # ASYMMETRY with the promise gate below (~:586), intentional - do not align:
    # this gate REPLACES the refs with [(pr, node.pr_url)] and asks "is THIS pr
    # merged" (one ship, scoped to the node's repo). The promise gate UNIONS the
    # explicit --pr onto the stored refs and uses the pr's OWN url, because it
    # asks "how many ships landed" and a multi-repo split may put the new PR in
    # a different repo than the node's prior ship. Replace-vs-union and
    # node-url-vs-pr-url differ because the two questions differ.
    gate_refs = [(pr, node.get("pr_url"))] if pr is not None else node_pr_refs(node)

    # An already-done node is a metadata update, not a close. The close was
    # gated when it happened; re-gating here would let a gh outage block a
    # --note from landing on a node that closed months ago.
    if node.get("status") == "done" or node.get("completed_at"):
        gate_refs = []

    merge_status_to_write: Optional[str] = None
    if gate_refs:
        evidence = resolve_merge_evidence(
            gate_refs, cwd=node.get("cwd"), query=_gh_query
        )
        if evidence.outcome == "awaiting_merge":
            typer.echo(
                f"awaiting merge: PR #{evidence.open_pr_number} is OPEN, not merged. "
                f"{node_id} stays in_review and closes on merge "
                f"(reconcile / merge-triggered advance)."
                + (f" (note: {evidence.error})" if evidence.error else ""),
                err=True,
            )
            raise typer.Exit(code=evidence.exit_code)
        if evidence.outcome == "outage":
            typer.echo(
                f"Error: gh cross-check failed for {node_id}: {evidence.error}\n"
                f"The check is retryable once gh is available again. Node stays open.",
                err=True,
            )
            raise typer.Exit(code=evidence.exit_code)
        if evidence.outcome == "refused":
            typer.echo(
                f"Refused: {node_id} cross-check failed: {evidence.reason}",
                err=True,
            )
            raise typer.Exit(code=evidence.exit_code)
        merge_status_to_write = "merged"

    # -- promise gate (x-5d34) --
    # Only on the close path (node not already done): a metadata update on a
    # done node was gated when it first closed, and re-gating here would let a
    # gh outage block a --note on a node that closed months ago. fno done has no
    # --force escape; a deliberate half-ship closes via
    # `fno backlog done <id> --force --reason`.
    if not (node.get("status") == "done" or node.get("completed_at")):
        # An explicit --pr is a ship the merge gate just confirmed but the node
        # has not stored yet; condition C must count it, else a final multi-PR
        # close reports its own new PR as missing (P2).
        #
        # ASYMMETRY with the merge gate above (~:535), intentional: that gate
        # REPLACES the refs with the explicit pr scoped to node.pr_url (one ship,
        # node-scoped); this gate UNIONS the explicit pr onto the stored refs
        # (all ships) and uses the pr's OWN url, since a multi-repo split may put
        # the new PR in a different repo than the node's prior ship. The two
        # resolve the ref url differently because they ask different questions.
        # The url that belongs to THIS pr number, when the operator named one:
        # the ref's url is only a repo hint, and the node's stored pr_url points
        # at the PREVIOUS ship, which in the multi-repo split that
        # expected_url_count exists for is a different repo entirely.
        _extra_url = pr_url or auto_url or node.get("pr_url")
        _extra_refs = [(pr, _extra_url)] if pr is not None else None
        promise = resolve_promise_evidence(
            node, cwd=node.get("cwd"), query=_gh_query, extra_refs=_extra_refs
        )
        if promise.outcome == "promise_unmet":
            typer.echo(promise.reason, err=True)
            raise typer.Exit(code=promise.exit_code)
        if promise.warning:
            typer.echo(f"warning: {promise.warning}", err=True)

    # Resolve pr_url + ledger rollup outside the mutator so subprocess / disk
    # I/O stays out of the graph lock.
    pr_url_to_write: Optional[str] = None
    if pr is not None:
        if pr_url is not None:
            if repo_slug_from_url(pr_url) is None:
                typer.echo(
                    f"fno done: --pr-url {pr_url!r} is not a GitHub PR url "
                    "(expected https://github.com/<owner>/<repo>/pull/<n>)",
                    err=True,
                )
                raise typer.Exit(code=2)
            named = pr_number_from_url(pr_url)
            if named != pr:
                typer.echo(
                    f"fno done: --pr-url names PR #{named}, not #{pr} - a row "
                    "pointing at two different PRs matches neither.",
                    err=True,
                )
                raise typer.Exit(code=2)
            pr_url_to_write = pr_url
        else:
            pr_url_to_write = auto_url or pr_url_for_repo(pr, node.get("cwd"))
        # Fail closed: a url-less pr_number names no repo, and PR numbers
        # collide across repos, so a bare number can attribute a foreign PR.
        if pr_url_to_write is None:
            typer.echo(
                f"fno done: cannot resolve the repo for PR #{pr} - refusing to "
                "stamp an unattributable pr_number. Fix with either "
                "`gh auth login` or `--pr-url https://github.com/<owner>/<repo>/pull/"
                f"{pr}`.",
                err=True,
            )
            raise typer.Exit(code=2)
    rollup = _rollup_from_ledger(node)
    rollup_tags: list[str] = []

    # Collision tracking: set inside the mutator, read after locked_mutate_graph.
    collision_state: dict = {"detected": False, "first_completed_at": None}

    def _mutator(entries_inner):
        for e in entries_inner:
            if e.get("id") != node_id:
                continue
            # User-supplied metadata applies on both paths (collision + normal).
            # Hoisted out of the if/else so the two branches don't duplicate the
            # same six assignments.
            if pr is not None:
                e["pr_number"] = pr
                # Only when the gate above resolved MERGED from gh. Left alone
                # otherwise so a metadata-only update to an already-done node
                # cannot erase the evidence its own close recorded.
                if merge_status_to_write is not None:
                    e["merge_status"] = merge_status_to_write
                e["pr_url"] = pr_url_to_write
            if link is not None:
                e["artifact_url"] = link
            if note is not None:
                e["completion_note"] = note

            if e.get("status") == "done":
                # Second writer sees node already done - collision path.
                # Skip status / completed_at overwrites. Apply rollup only when
                # force_overwrite is explicit: a bare `fno done <id>` on a done
                # node must not silently re-reconcile rollup, but
                # `fno done <id> --force-overwrite` honors the flag's promise of
                # explicit re-reconciliation even on collision.
                collision_state["detected"] = True
                collision_state["first_completed_at"] = e.get("completed_at")
                if force_overwrite:
                    rollup_tags.extend(_apply_rollup(e, rollup, env_session=env_session, force_overwrite=True))
            else:
                e["status"] = "done"
                e["completed_at"] = now
                rollup_tags.extend(_apply_rollup(e, rollup, env_session=env_session, force_overwrite=force_overwrite))
            break
        return entries_inner

    locked_mutate_graph(graph_path, _mutator)

    # Emit done_race_collision AFTER the lock releases (telemetry fires after
    # the op so the diagnostic line reflects the actual emit outcome - per
    # memory feedback_forward_promise_telemetry_lies).
    if collision_state["detected"]:
        first_completed_at = collision_state["first_completed_at"] or ""
        emit_outcome = "emitted"
        try:
            import fno.events as _ev
            _ev.append_event(
                _ev.done_race_collision(
                    node_id=node_id,
                    first_completed_at=first_completed_at,
                    second_attempt_at=now,
                )
            )
        except (
            _ValidationError,
            _SchemaUnavailableError,
            TimeoutError,
            OSError,
        ) as exc:
            emit_outcome = f"emit failed: {exc!r}"
        # On corrupt graph entries first_completed_at can be empty; conditional
        # to avoid an awkward "already done at ;" diagnostic.
        at_msg = f" at {first_completed_at}" if first_completed_at else ""
        typer.echo(
            f"fno done: {node_id} already done{at_msg}; "
            f"metadata updates applied; collision event {emit_outcome}",
            err=True,
        )
        typer.echo(f"fno done: {node_id} -> already done (metadata updated)")
        return

    tag_bits: list[str] = [f"domain={domain}"]
    if pr is not None:
        tag_bits.append(f"PR #{pr}")
        tag_bits.append(f"pr_url={pr_url_to_write}")
    if link is not None:
        tag_bits.append(f"link={link}")
    if note is not None:
        tag_bits.append(f"note={note!r}")
    tag_bits.extend(rollup_tags)
    typer.echo(f"fno done: {node_id} -> done  " + "  ".join(tag_bits))

    # Operator-authority matrix (LD3/LD29): the top-level `fno done` verb is an
    # allowed action during a drive window, but audit-tag it so the trail
    # attributes the completion to the operator rather than the LLM. Mirrors
    # `graph/cli.py::cmd_done` exactly so both done verbs emit the identical
    # event kind, source, and envelope -- the audit trail must not fork by verb.
    # Reached only on a fresh completion: the collision path returns above and
    # `--backfill` returns before the normal flow, so no guard on
    # `collision_state["detected"]` is needed here. Best-effort: a write failure
    # warns to stderr inside the helper and never breaks `fno done`.
    try:
        from fno.drive_authority import (
            emit_operator_initiated,
            is_drive_authority_active,
        )

        if is_drive_authority_active():
            emit_operator_initiated(
                "backlog_done_operator_initiated",
                source="backlog",
                task_id=node_id,
            )
    except Exception:
        pass


# Back-compat: some tests and older imports may reach for `cli` attribute.
# Provide a Typer app that exposes done_command as its default callback so
# `app.add_typer(done.cli, name="...")` keeps working if anyone uses that form.
cli = typer.Typer(
    name="done",
    help="Mark a backlog node as done. Auto-detects for code-domain nodes.",
    no_args_is_help=False,
    invoke_without_command=True,
)


@cli.callback(invoke_without_command=True)
def _cli_callback(
    ctx: typer.Context,
    query: Optional[str] = typer.Argument(None),
    pr: Optional[int] = typer.Option(None, "--pr-number", "--pr", "-p"),
    pr_url: Optional[str] = typer.Option(None, "--pr-url"),
    link: Optional[str] = typer.Option(None, "--link", "--url", "-l"),
    note: Optional[str] = typer.Option(None, "--note", "-m"),
    backfill: bool = typer.Option(False, "--backfill"),
    force_overwrite: bool = typer.Option(
        False,
        "--force-overwrite",
        help="Overwrite existing rollup fields instead of fill-if-null. Use with --backfill for explicit re-reconciliation of stale rollups.",
    ),
) -> None:
    if ctx.invoked_subcommand is not None:
        return
    done_command(query=query, pr=pr, pr_url=pr_url, link=link, note=note, backfill=backfill, force_overwrite=force_overwrite)
