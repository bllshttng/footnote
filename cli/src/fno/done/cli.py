"""`fno done` - mark a backlog node as done.

For `domain: code` with no explicit flags, auto-detects the node from the
current git branch and fills pr_number/pr_url/merge_status from `gh pr view`.
For any domain, populates user-supplied fields (--pr, --link, --note) and
always sets `_status: done` + `completed_at`.

Beyond the direct flags, `fno done` ALSO rolls up ledger-sourced lifecycle
fields so a "done" graph entry reflects everything we know about the
completed feature, not just the PR trio:

    session_id      - from $CLAUDECODE_SESSION_ID if set, else latest ledger
    cost_usd        - sum of all matching ledger entries' cost_usd
    cost_sessions   - one row per (ledger entry, session UUID) combination
    points          - from ledger.points if currently null in graph

A --backfill flag runs ONLY the rollup (skipping _status / completed_at
changes), for sweeping already-done nodes that were marked done before
this logic existed. Pair with --force-overwrite for explicit re-reconciliation
of stale rollups (e.g. session_id or points changed since the node was marked done).

When a second `fno done` call races a node that is already done, the _status
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
from fno.graph.fuzzy import resolve_id
from fno.graph.store import locked_mutate_graph, read_graph


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


def _pr_url_from_gh(pr: int) -> Optional[str]:
    try:
        r = subprocess.run(
            ["gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
            capture_output=True, text=True, check=False,
        )
    except OSError:
        return None
    if r.returncode != 0 or not (r.stdout or "").strip():
        return None
    return f"https://github.com/{r.stdout.strip()}/pull/{pr}"


# -- ledger rollup --


def _norm(path: Optional[str]) -> Optional[str]:
    """Normalize a plan_path for comparison across graph / ledger / different
    trailing-slash / relative-vs-absolute conventions."""
    if not path:
        return None
    return os.path.normpath(path).rstrip(os.sep)


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


def _rollup_from_ledger(plan_path: Optional[str]) -> dict:
    """Aggregate session_id / cost_usd / cost_sessions / points from ledger.

    Returns a dict with keys: session_id, cost_usd, cost_sessions, points.
    Any field for which the ledger has no information is returned as None
    (or [] for cost_sessions) so the caller can preserve existing graph
    values instead of nulling them out.
    """
    if not plan_path:
        return {"session_id": None, "cost_usd": None, "cost_sessions": [], "points": None}

    target = _norm(plan_path)
    ledger_entries = _load_ledger_entries()
    matching = [
        le for le in ledger_entries
        if isinstance(le, dict) and _norm(le.get("plan_path")) == target
    ]
    if not matching:
        return {"session_id": None, "cost_usd": None, "cost_sessions": [], "points": None}

    # One cost_sessions row per (ledger entry, session UUID). Split the
    # ledger's aggregate cost_usd evenly across its session list so sums
    # stay faithful. If a ledger entry has no sessions, emit one row with
    # session_id=None so the cost still shows up.
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
        ts = le.get("completed") or le.get("started")
        if sessions:
            per = cost_f / len(sessions)
            for sid in sessions:
                cost_sessions.append({
                    "session_id": sid,
                    "cost_usd": round(per, 4),
                    "timestamp": ts,
                })
        else:
            cost_sessions.append({
                "session_id": None,
                "cost_usd": round(cost_f, 4),
                "timestamp": ts,
            })

    # Latest session: pick the most-recent-completed ledger entry's last UUID.
    def _sort_key(le: dict) -> str:
        return (le.get("completed") or le.get("started") or "") or ""

    latest = max(matching, key=_sort_key)
    latest_sessions = latest.get("sessions")
    if not isinstance(latest_sessions, list):
        latest_sessions = []
    session_id = latest_sessions[-1] if latest_sessions else None

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
    Cost dedup logic (cost_sessions de-dupe by session_id + timestamp) still
    applies regardless of force_overwrite so double-counting cannot occur.
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


def done_command(
    query: Optional[str] = typer.Argument(
        None,
        help="Graph node id (ab-xxx) or title substring. Omit to auto-detect from git branch.",
    ),
    pr: Optional[int] = typer.Option(
        None, "--pr-number", "--pr", "-p", help="PR number (for code-domain completions)."
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
            "points). Does not flip _status or completed_at. With no QUERY, "
            "sweeps every node with _status=done."
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
    graph_path = _path_graph()
    env_session = os.environ.get("CLAUDECODE_SESSION_ID") or None

    # -- backfill mode (no status change, may be batch) --
    if backfill:
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
                        f"  {c.get('id'):<14} {c.get('_status', '?'):<9} "
                        f"{c.get('title', '')}",
                        err=True,
                    )
                raise typer.Exit(code=2)
            target_ids = {match.id}
        else:
            target_ids = {
                e.get("id") for e in entries
                if e.get("_status") == "done" and e.get("id")
            }

        if not target_ids:
            typer.echo("fno done --backfill: no done nodes to backfill")
            return

        # Pre-compute rollups outside the mutator (ledger I/O stays unlocked).
        rollups: dict[str, dict] = {}
        for e in entries:
            if e.get("id") in target_ids:
                rollups[e["id"]] = _rollup_from_ledger(e.get("plan_path"))

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
                f"  {c.get('id'):<14} {c.get('_status', '?'):<9} "
                f"{c.get('title', '')}"
            )
            typer.echo(line, err=True)
        raise typer.Exit(code=2)

    node_id = match.id
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

    # Resolve pr_url + ledger rollup outside the mutator so subprocess / disk
    # I/O stays out of the graph lock.
    pr_url_to_write: Optional[str] = None
    if pr is not None:
        pr_url_to_write = auto_url or _pr_url_from_gh(pr)
    rollup = _rollup_from_ledger(node.get("plan_path"))
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
                e["merge_status"] = "merged"
                # Only overwrite pr_url if we resolved one; otherwise leave prior
                # value intact. None is acceptable when gh auth is missing.
                if pr_url_to_write is not None:
                    e["pr_url"] = pr_url_to_write
            if link is not None:
                e["artifact_url"] = link
            if note is not None:
                e["completion_note"] = note

            if e.get("_status") == "done":
                # Second writer sees node already done - collision path.
                # Skip _status / completed_at overwrites. Apply rollup only when
                # force_overwrite is explicit: a bare `fno done <id>` on a done
                # node must not silently re-reconcile rollup, but
                # `fno done <id> --force-overwrite` honors the flag's promise of
                # explicit re-reconciliation even on collision.
                collision_state["detected"] = True
                collision_state["first_completed_at"] = e.get("completed_at")
                if force_overwrite:
                    rollup_tags.extend(_apply_rollup(e, rollup, env_session=env_session, force_overwrite=True))
            else:
                e["_status"] = "done"
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
        from fno.agents.drive_authority import (
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
    done_command(query=query, pr=pr, link=link, note=note, backfill=backfill, force_overwrite=force_overwrite)
