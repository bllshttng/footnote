"""fno graph CLI - typer subcommands for feature graph management.

Each subcommand delegates to fno.graph.{store,statuses,render,depends}
and preserves identical behavior to scripts/roadmap-tasks.py.

Exit codes:
    0  success
    1  user error (invalid input)
    2  runtime error (bad state, cycle detected)
    3  not found
    4  nothing to intake
"""
from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace
from typing import Any, List, Literal, Optional, Union

import typer

from fno.tombstones import tombstone_group_cls

cli = typer.Typer(
    name="graph",
    help="Feature graph management",
    no_args_is_help=True,
    # Removed verbs under `backlog` refuse by name and say what replaced them,
    # instead of failing with the same message a typo gets.
    cls=tombstone_group_cls("backlog"),
    # The curated menu below is nouns; this line is the answer to "can I take
    # that back". It sits on the group help because that is the surface someone
    # deciding what is possible actually reads - a correction verb nobody can
    # find is, for decision-making purposes, a correction that does not exist.
    epilog=(
        "Corrections: reopen (undo done) | remove (hard delete) | unarchive | "
        "undefer | unqueue | unsupersede | unclaim. All hidden; "
        "`fno help backlog --all` lists every verb."
    ),
)

# Nested triage sub-app: `fno backlog triage <verb>`.
from fno.graph.triage import cli as _triage_cli  # noqa: E402

cli.add_typer(_triage_cli, name="triage")

# Nested capture sub-app: `fno backlog capture <verb>`. The capture tier below
# idea nodes (markdown fu-* items, NOT graph nodes). Distinct from
# `fno agents mail` (cross-project messaging).
#
# `inbox` was a SECOND registration of this same app, so all nine of its
# subcommands were duplicates and the surface paid for them twice. It is gone;
# `fno.tombstones` keeps the name reachable as a signpost.
from fno.backlog.capture import cli as _capture_cli  # noqa: E402

cli.add_typer(_capture_cli, name="capture", hidden=True)

# Nested batch sub-app: `fno backlog batch <verb>`. Batch-lane state
# (.fno/batches/<domain>.json) — coalesce same-domain nodes into one PR.
from fno.backlog.batch import cli as _batch_cli  # noqa: E402

cli.add_typer(_batch_cli, name="batch", hidden=True)

# Decision records are node/PR metadata, so their three leaves live directly
# under backlog. The old top-level spelling remains a lazy shim.
from fno.decide.cli import (  # noqa: E402
    backlog_decide,
    backlog_decide_retract,
    backlog_decide_reindex,
    backlog_decisions,
)

cli.command("decide", hidden=True)(backlog_decide)
cli.command("decisions", hidden=True)(backlog_decisions)
cli.command("decide-retract", hidden=True)(backlog_decide_retract)
cli.command("decide-reindex", hidden=True)(backlog_decide_reindex)


# Node-lifecycle sub-apps folded under backlog (unit 6 of the x-9d6c reorg):
# annotate findings, carve-out records, and the retro harvest are all node
# metadata operations. The old top-level spellings stay one-release shims
# (fno.verb_moves); the mounts below are the canonical homes.
from fno.annotate.cli import annotate_app as _annotate_app  # noqa: E402
from fno.carveout.cli import carveout_app as _carveout_app  # noqa: E402
from fno.retro.cli import retro_app as _retro_app  # noqa: E402

cli.add_typer(_annotate_app, name="annotate", hidden=True)
cli.add_typer(_carveout_app, name="carveout", hidden=True)
cli.add_typer(_retro_app, name="retro", hidden=True)


# Selection-time enforcement (ab-fcf9cec5): a node another session is actively
# driving holds a live ``node:<id>`` claim and must be skipped so two sessions
# never pick up the same node. The implementation is homed in graph/statuses.py
# (so the board renderers can share it without a cli<->render cycle); re-exported
# under the original module-global name that existing tests monkeypatch.
from fno.graph.statuses import _LEGACY_DEFER_PREFIX, live_claimed_node_ids as _live_claimed_node_ids  # noqa: E402


def _require_live_claimed_node_ids(operation: str) -> set[str]:
    """Read live claims for a dispatch or mutation path, failing closed."""
    try:
        return _live_claimed_node_ids(strict=True)
    except Exception as exc:
        typer.echo(
            f"Error: live claim state is unavailable; {operation} refused.",
            err=True,
        )
        raise typer.Exit(code=1) from exc


def _has_unmerged_open_pr(e: dict) -> bool:
    """True when a node already carries a PR but is not yet closed (done) -
    i.e. work is in flight / in review, so it must NOT be re-selected for
    dispatch (ab-372130f6).

    A node only leaves the ready pool at merge-and-close (completed_at set ->
    recompute_statuses derives status "done"). During the whole PR window
    pr_number is set but completed_at is None, so the status derivation still
    yields "ready"; this predicate is the missing selection-time guard,
    mirroring _live_claimed_node_ids() - the PID-based node claim dies when the
    builder session exits, leaving no in-flight signal behind.

    Excludes every not-done node that carries a pr_number, regardless of
    merge_status: an open unmerged PR (the originally-observed ab-58645f63
    case), a merged-but-reconcile-pending PR (do not re-dispatch already-merged
    work in the close gap), and a closed-without-merge PR (un-dispatchable until
    pr_number is cleared - see the plan's known edges) all mean "not fresh
    ready work".
    """
    if e.get("completed_at"):
        return False  # already done; status derivation bucketed it out of ready
    return bool(e.get("pr_number"))


def _is_batched_member(e: dict) -> bool:
    """True when a node is already committed to an open batch (batch-lane Wave 2).

    A batched member has its atomic commits on a shared batch branch and ships as
    part of the batch PR, not its own. It must NOT be re-selected for dispatch
    (else the daemon would spawn a second worker for work already on the branch).
    The mark is the graph `batch` field, set by `/target batched` via
    `fno backlog update --batch <id>` and cleared (`--batch null`) on abandon so
    the node resurfaces for an individual ship. Mirrors `_has_unmerged_open_pr`:
    an in-flight signal that survives the builder session's PID-claim dying.
    """
    return bool(e.get("batch"))


def _needs_design(e: dict) -> bool:
    """True when a node still needs a design pass before it can be blueprinted.

    Reads the rung rather than `plan_path` presence. The presence check was a
    PROXY for "not ready": with `stub` in no vocabulary, a linked scaffold
    derived `ready`, so withholding the link was the only lever that could say
    "undesigned". `plan_rung` says it directly, which means a child linked to an
    `idea`-rung scaffold is correctly still a design candidate instead of being
    skipped as done.
    """
    from fno.graph.ladder import Rung, plan_rung

    return plan_rung(e) in (Rung.NONE, Rung.IDEA)


def _container_ids(entries: list[dict]) -> set[str]:
    """Ids of nodes that are some other node's ``parent`` - i.e. epics/containers.

    A container is never directly buildable: its work lives in its decomposed
    children, and it carries no PR of its own. So every work-SELECTION surface
    drops it from the candidate pool - `next`/`ready`/`--all-ready` build the
    leaves, never the box (x-33b2). Shared by `next` (_pick_ready) and `ready`
    (cmd_ready) so the two surfaces cannot drift; advance_dependents applies the
    same rule on the merge edge-following path.

    No "keep the all-done epic selectable" exception is needed: an epic closes
    automatically via ``_cascade_close_parents`` on the merge that finishes its
    last child (uniform across projects), so it is already ``done`` - never a
    lingering ``ready`` container that selection would have to surface for
    closure. That replaces the old "walker closes the epic via next" path,
    which conflicted with never building a container.
    """
    return {
        p for e in entries
        if isinstance(e, dict) and isinstance((p := e.get("parent")), str)
    }


@cli.callback()
def _graph_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag
    merge_json_flag(ctx, json_output)


def _graph_path() -> Path:
    """Return the active graph.json path (monkeypatch-friendly)."""
    from fno.graph._constants import GRAPH_JSON
    return GRAPH_JSON


def _display_entries(reader: str) -> list[dict]:
    """Entries for read-only display/search surfaces (view, find, roadmap,
    relatedness, provenance walks, the status summary).

    These renders visualize the LOCAL store's full records - status, tags,
    details, slug - which the five-field read contract does not carry, so they
    read through the guarded metadata reader: byte-identical on the default
    backend, an honest named refusal under an external selection (an external
    tracker has its own UI; a stale local render is the leak the seam closes).
    Mutation paths keep ``read_graph`` and get the shared external refusal
    from task 4.2 instead.
    """
    from fno.tracker.metadata import ExternalMetadataUnavailable, read_entries

    try:
        return read_entries(reader)
    except ExternalMetadataUnavailable as exc:
        typer.echo(f"fno backlog: {exc}", err=True)
        raise typer.Exit(code=2)


def _safe_stderr_warn(msg: str) -> None:
    """Write ``msg`` to stderr, swallowing a closed/broken stream.

    The post-write dedup fallback runs AFTER the node already committed, so a
    secondary stderr failure (closed fd, broken pipe) must never escape and
    fail a filing whose mutation already landed (codex P2)."""
    try:
        sys.stderr.write(msg)
    except Exception:  # noqa: BLE001 - a dead stderr must not break the filing
        pass


# Distinct from exit 1 ("graph read cleanly, node absent"): the graph itself
# could not be read. click reserves 2 for usage errors, so 3 is the first free
# code. A resolution caller that today treats any non-zero as "absent" keeps
# failing closed; one that cares can tell a wedged graph from a typo.
GRAPH_UNREADABLE_EXIT = 3


def _resolve_entries_or_exit(id: str):
    """Read the graph strictly for a resolution verb.

    Returns the entries on a clean read (populated or empty). On an unreadable
    graph, prints a message that names the read failure and the path -- never
    "No node matching", which would assert the node is absent -- and exits with
    the distinct GRAPH_UNREADABLE_EXIT instead of 1.
    """
    from fno.graph.store import read_graph_strict, GraphUnreadableError

    try:
        return read_graph_strict(_graph_path())
    except GraphUnreadableError as e:
        typer.echo(
            f"Could not read the graph cleanly, so '{id}' cannot be resolved: {e}",
            err=True,
        )
        raise typer.Exit(code=GRAPH_UNREADABLE_EXIT)


def _archive_path() -> Path:
    from fno.graph._constants import GRAPH_ARCHIVE_JSON
    return GRAPH_ARCHIVE_JSON


def _briefs_dir() -> Path:
    from fno.graph._constants import BRIEFS_DIR
    return BRIEFS_DIR


# -- relatedness sidecar (`fno backlog relatedness build|get`) --
# A node-to-node relatedness map read by x-9ed6's offer path and /triage.
# Sidecar, not a graph mutation, so `build` writes unconditionally.

_relatedness_cli = typer.Typer(
    name="relatedness",
    help="Node-to-node relatedness map (sidecar next to graph.json).",
    no_args_is_help=True,
)


def _relatedness_path() -> Path:
    from fno.paths import relatedness_json
    return relatedness_json()


@_relatedness_cli.command("build")
def cmd_relatedness_build(
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Restrict the corpus to this project."),
    judge: bool = typer.Option(False, "--judge", help="Haiku pairwise refinement (v2, opt-in); v1 is deterministic-only."),
    top_k: int = typer.Option(5, "--top-k", "-K", help="Edges persisted per node."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the built map as JSON."),
) -> None:
    """Build the relatedness sidecar from graph signals (read-only on the graph)."""
    from fno.graph import relatedness as _r

    entries = _display_entries("relatedness.build")
    if project is not None:
        entries = [e for e in entries if e.get("project") == project]
    mapping = _r.build_map(entries, k=top_k)
    if judge:
        # Degrade, never abort: v1 has no judge layer, so note and write the
        # deterministic map (AC6 posture - LLM absence never blocks the write).
        typer.echo("note: --judge (haiku refinement) not implemented in v1; wrote deterministic map.", err=True)
    path = _relatedness_path()
    _r.write_map(path, mapping)
    if json_output:
        typer.echo(json.dumps(mapping, indent=2))
    else:
        edges = sum(len(v) for v in mapping.values())
        typer.echo(f"relatedness: {len(mapping)} nodes, {edges} edges -> {path}")


@_relatedness_cli.command("get")
def cmd_relatedness_get(
    node_id: str = typer.Argument(..., help="Node id to fetch related nodes for."),
    top_k: int = typer.Option(5, "--top-k", "-K", help="Max related nodes to return."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit a JSON array."),
) -> None:
    """Print the top related nodes for one node (the x-9ed6 consumer API).

    No map -> exit non-zero, empty stdout (the caller's fallback signal).
    Present map, no edges -> exit 0, empty list. The two are distinct (AC3).
    """
    from fno.graph import relatedness as _r

    try:
        edges = _r.get_related(_relatedness_path(), node_id, k=top_k)
    except _r.NoMapError:
        raise typer.Exit(code=1)
    if json_output:
        typer.echo(json.dumps(edges, indent=2))
    else:
        for r in edges:
            typer.echo(f"{r['id']}\t{r['score']}\t{r['reason']}")


cli.add_typer(_relatedness_cli, name="relatedness", hidden=True)


# -- epic status (`fno backlog epic status <id>`) --
# One cross-project table over an epic's children: id/slug, project, status,
# live worker (node:<id> claim holder), PR (node stamp). A `ready` child with no
# live worker prints its most recent dispatch/skip/failure receipt inline -
# never a blank cell (the silent failure this verb exists to kill). A `deferred`
# child prints its consecutive-failure breaker streak so a tripped breaker is
# diagnosable from this one screen. Reads only; no graph mutation.

_epic_cli = typer.Typer(
    name="epic",
    help="Epic (container) status across projects.",
    no_args_is_help=True,
)

# Node-keyed dispatch receipts (all carry data.node_id). termination keys on
# session_id, not node_id, so it never matches a child row and is left out.
_RECEIPT_TYPES = {
    "advance_dispatched",
    "advance_skipped",
    "advance_failed",
    "quota_deferred",
    "dispatch_deferred",
    "quota_rotation_declined",
}


def _live_worker(node_id: str) -> Optional[str]:
    """The holder of a live/suspect ``node:<id>`` claim, else None.

    A suspect claim (TTL-unexpired, pid dead) still belongs to its session, so
    it counts as a worker here (x-ba4b). Routes through the global claims root
    that node ids key on, so it reads the same lockfile the dispatcher wrote.
    """
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    key = f"node:{node_id}"
    try:
        info = claim_status(key, root=claims_root_for(key))
    except Exception:  # noqa: BLE001 - a status read must never crash the table
        return None
    if info.get("state") in ("live", "suspect"):
        return info.get("holder")
    return None


def _epic_events(children: list[dict]) -> list[dict]:
    """Union the events logs that can carry a child's receipt, deduped + ts-sorted.

    Reads the global mirror (walker node_* events) plus each child project's
    ``<root>/.fno/events.jsonl`` (where advance/reconcile emit their dispatch
    receipts). The child root is resolved through the workspace map
    (``project_root_from_settings``) so a moved checkout whose recorded ``cwd``
    is stale still finds the live journal, falling back to the recorded cwd.

    The loop runtime writes each node_* envelope byte-identically to BOTH the
    project journal and the global mirror, so envelopes are deduped by content -
    otherwise a deferred child's breaker streak would double-count. Sorted by
    ``ts`` (None-safe) so newest-wins holds across files.
    """
    from fno.graph import failure
    from fno.graph._intake import project_root_from_settings

    seen_paths: set[str] = set()
    seen_env: set[str] = set()
    out: list[dict] = []

    def _ingest(path: Path) -> None:
        if str(path) in seen_paths:
            return
        seen_paths.add(str(path))
        for e in failure.read_events(path):
            key = json.dumps(e, sort_keys=True, default=str) if isinstance(e, dict) else repr(e)
            if key in seen_env:
                continue
            seen_env.add(key)
            out.append(e)

    _ingest(failure.events_path())

    for c in children:
        proj = c.get("project")
        root = (project_root_from_settings(proj) if proj else None) \
            or c.get("_resolved_cwd") or c.get("cwd")
        if not root:
            continue
        _ingest(Path(root) / ".fno" / "events.jsonl")

    out.sort(key=lambda e: (e.get("ts") or "") if isinstance(e, dict) else "")
    return out


def _format_receipt(etype: str, data: dict) -> str:
    if etype == "advance_dispatched":
        who = data.get("agent_name") or data.get("short_id") or "worker"
        return f"dispatched {who}"
    if etype == "advance_skipped":
        return f"skipped: {data.get('reason', '?')}"
    if etype == "advance_failed":
        err = (data.get("error") or "").strip()
        return f"failed: {err[:80]}" if err else "failed"
    if etype == "quota_rotation_declined":
        prov = data.get("provider") or ""
        return f"declined: {prov}" if prov else "declined"
    # quota_deferred / dispatch_deferred
    prov = data.get("provider") or data.get("owner_harness") or ""
    return f"deferred: {prov}" if prov else "deferred"


def _latest_receipt(node_id: str, events: list[dict]) -> Optional[str]:
    """The most recent dispatch/skip/failure receipt for ``node_id``, or None.

    ``events`` is ts-sorted (oldest -> newest) by ``_epic_events``, so the last
    matching envelope is the newest receipt.
    """
    latest: Optional[dict] = None
    for e in events:
        if not isinstance(e, dict) or e.get("type") not in _RECEIPT_TYPES:
            continue
        data = e.get("data")
        if not isinstance(data, dict) or data.get("node_id") != node_id:
            continue
        latest = e
    if latest is None:
        return None
    return _format_receipt(latest["type"], latest["data"])


def _child_note(child: dict, events: list[dict], worker: Optional[str]) -> str:
    """The inline note for a child row: streak (deferred), receipt (idle ready),
    or ``-`` (working / done). Never blank for an idle ready child."""
    from fno.graph import failure

    node_id = child["id"]
    status = child.get("status")
    if status == "deferred":
        return f"streak {failure.consecutive_failures(node_id, events)}"
    if status == "ready" and not worker:
        return _latest_receipt(node_id, events) or "no receipt found"
    return "-"


def _scope_growth_line(growth) -> str:
    """One line: the growth figure with its coverage, or why it is withheld.

    The coverage clause is never dropped - a bare count reads as measured.
    """
    from fno.graph.rollup import SCOPE_GROWTH_COVERAGE_FLOOR

    pct = f"{growth.coverage:.0%} of {growth.window_total} window nodes"
    if growth.window_dangling:
        pct += f", {growth.window_dangling} dangling"
    cost = (
        f"realized {growth.realized_nodes} nodes / {growth.realized_prs} PRs"
        f"{f' vs size {growth.declared_size}' if growth.declared_size else ''}"
    )
    if not growth.reportable or growth.follow_up_ids is None:
        return (
            f"scope growth: withheld (origin capture {pct}, below the "
            f"{SCOPE_GROWTH_COVERAGE_FLOOR:.0%} floor)  |  {cost}"
        )
    return (
        f"scope growth: {len(growth.follow_up_ids)} follow-ups "
        f"(origin capture {pct})  |  {cost}"
    )


@_epic_cli.command("status")
def cmd_epic_status(
    ctx: typer.Context,
    epic: str = typer.Argument(..., help="Epic node id or slug."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON."),
) -> None:
    """One table over an epic's children: status, worker, PR, and an inline
    dispatch receipt (or breaker streak) so an idle/deferred child is never a
    silent blank. Refuses a non-container node by name."""
    from fno.graph.fuzzy import resolve_node
    from fno.handoff.output import merge_json_flag, json_mode

    # Honor --json wherever it appears (top-level, subtyper, or this leaf) - the
    # parent callbacks merge theirs into ctx.obj; merge this leaf's too.
    merge_json_flag(ctx, json_output)

    entries = _display_entries("epic.status")
    match = resolve_node(epic, entries)
    if match.kind != "exact" or not match.id:
        typer.echo(f"epic status: no node matches '{epic}'", err=True)
        raise typer.Exit(code=1)
    epic_id = match.id
    epic_node = match.candidates[0]

    # A container is a node with children OR an `epic`-typed node not yet
    # decomposed (childless but legitimately queryable -> shows "(no children)").
    # A genuine leaf (feature/bug/... with no children) is refused by name.
    if epic_id not in _container_ids(entries) and epic_node.get("type") != "epic":
        typer.echo(
            f"epic status: {epic_id} is a leaf, not a container "
            f"(an epic's work lives in its children).",
            err=True,
        )
        raise typer.Exit(code=1)

    children = [e for e in entries if isinstance(e, dict) and e.get("parent") == epic_id]
    children.sort(key=lambda c: c.get("id", ""))
    events = _epic_events(children)

    def _status_of(c: dict) -> Optional[str]:
        return c.get("status")

    total = len(children)
    done = sum(1 for c in children if _status_of(c) == "done")

    rows = []
    for c in children:
        node_id = c["id"]
        worker = _live_worker(node_id)
        pr = c.get("pr_number")
        rows.append({
            "id": node_id,
            "slug": c.get("slug") or "",
            "project": c.get("project") or "",
            "status": _status_of(c) or "",
            "worker": worker,
            "pr_number": pr,
            "receipt": _child_note(c, events, worker),
        })

    from fno.graph.rollup import scope_growth
    from fno.graph.store import entries_with_archive

    # Read through the archive for the METRIC only (the same read-only fallback
    # `get` uses). Without it a swept child stops counting and the epic's
    # realized cost and follow-up set shrink as grooming runs - a number that
    # quietly changes with unrelated maintenance is the failure this metric is
    # supposed to be immune to. The children table above stays working-graph
    # only, as it was before.
    growth = scope_growth(entries_with_archive(entries), epic_id)

    if json_mode(ctx):
        typer.echo(json.dumps({
            "epic": epic_id,
            "slug": epic_node.get("slug"),
            "children_total": total,
            "children_done": done,
            "children": rows,
            # follow_ups is reported only when coverage clears the floor; the
            # coverage block ships regardless so a suppressed figure explains
            # itself instead of just being absent.
            "scope_growth": {
                "follow_ups": len(growth.follow_up_ids or ()) if growth.reportable else None,
                "follow_up_ids": list(growth.follow_up_ids or ()),
                "reportable": growth.reportable,
                "coverage": round(growth.coverage, 4),
                "window_total": growth.window_total,
                "window_with_origin": growth.window_with_origin,
                # Origins naming a node the graph no longer has. Excluded from
                # coverage (they can join nothing) and reported so the gap
                # between "stamped" and "joinable" stays visible.
                "window_dangling": growth.window_dangling,
                "realized_nodes": growth.realized_nodes,
                "realized_prs": growth.realized_prs,
                "declared_size": growth.declared_size,
            },
        }, indent=2))
        return

    typer.echo(f"epic: {epic_id} ({epic_node.get('slug') or ''})  {done}/{total} done")
    typer.echo("  " + _scope_growth_line(growth))
    if not rows:
        typer.echo("  (no children)")
        return
    headers = ("child", "project", "status", "worker", "PR", "note")

    def _cells(r: dict) -> tuple[str, ...]:
        ident = r["slug"] or r["id"]
        return (
            f"{ident} ({r['id']})" if r["slug"] else r["id"],
            r["project"],
            r["status"],
            r["worker"] or "-",
            f"#{r['pr_number']}" if r["pr_number"] else "-",
            r["receipt"],
        )

    table = [headers] + [_cells(r) for r in rows]
    widths = [max(len(row[i]) for row in table) for i in range(len(headers))]
    for row in table:
        typer.echo("  " + "  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)))


cli.add_typer(_epic_cli, name="epic", hidden=True)


# -- shared node construction --

_NodeFields = dict


def _scan_md_field(text: str, key: str) -> Optional[str]:
    """First ``<key>: <value>`` value in a target-state.md, matched-quote-stripped.

    Local mirror of ``fno.agents.whoami._scan_field`` so ``graph`` does not import
    ``agents`` (avoids an import cycle). ``None`` if the key is absent.
    """
    import re

    # ^\s* tolerates indentation; (.+) captures the whole value so a path/title
    # containing spaces is not truncated at the first space (\S+ would).
    pattern = re.compile(rf"^\s*{re.escape(key)}:\s*(.+)")
    for line in text.splitlines():
        match = pattern.match(line)
        if match:
            value = match.group(1).strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
                value = value[1:-1]
            return value
    return None


def _stamp_ship_on_pr_link(node_id: str) -> None:
    """Stamp the ship lifecycle row when a node is first PR-linked.

    The PR link is ship's START (the PR is open, awaiting review/merge), so the
    row carries started_at only - no ended_at, since merge is recorded elsewhere
    or not at all, and the roster renders the row 'in progress' rather than
    guessing an end. Every shipped node passes through a PR-link site regardless
    of which worker or skill opened the PR, so the
    row records the implementer's identity, not the merger's. ``fno do pr
    bind-created`` is the second such site and calls this too. Best-effort: an
    unresolvable identity or a graph failure skips with a named stderr reason and
    never fails the update. Idempotent: append_session_record collapses a
    re-stamp of the same (phase, harness, session_id).
    """
    from datetime import datetime, timezone

    from fno.graph.store import append_session_record

    from fno.claims.self_identity import resolve_self_identity

    ident = resolve_self_identity()
    harness = (ident.harness or "").strip()
    session_id = (ident.session_id or "").strip()
    if not harness or not session_id:
        typer.echo(
            f"update: no ambient identity to stamp ship provenance for {node_id} "
            f"(missing {'harness' if not harness else 'session_id'}); "
            "run the link inside a session. Skipped.",
            err=True,
        )
        return
    try:
        append_session_record(
            _graph_path(), node_id, phase="ship",
            harness=harness, session_id=session_id,
            started_at=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
    except (Exception, SystemExit) as exc:
        typer.echo(
            f"update: ship provenance stamp skipped for {node_id}: {exc}",
            err=True,
        )


def _stamp_blueprint_on_plan_link(node_id: str) -> None:
    """Stamp the blueprint lifecycle row when a plan is first bound to a node.

    `backlog update --plan-path` is the choke point every blueprinted node
    passes through, so this replaces the skill-prose stamp a direct CLI call or
    non-Claude worker would skip. The plan-bind is blueprint's END, so the row
    carries ended_at at the bind instant and no started_at (think has no writer,
    so there is no clean blueprint start); the roster renders it 'end only'.
    Best-effort: an unresolvable identity or a graph failure skips with a named
    stderr reason and never fails the update. Idempotent.

    KNOWN LABEL APPROXIMATION (owned by node x-8824): this stamps the DESIGN
    BOUNDARY - the moment a plan is bound - not strictly /blueprint. On the
    normal path /think runs `backlog update --plan-path` (think SKILL), and
    /blueprint only binds CHILD plans during epic decomposition; so on the
    normal path this row fires at /think, wearing the 'blueprint' label. The
    row is honest about WHEN the plan was bound and WHO bound it; the phase
    label is the approximation. x-8824 tests whether plan-bind IS the think
    boundary (which would make think a mislabeled writer, not a missing one).
    """
    from datetime import datetime, timezone

    from fno.graph.store import append_session_record

    from fno.claims.self_identity import resolve_self_identity

    ident = resolve_self_identity()
    harness = (ident.harness or "").strip()
    session_id = (ident.session_id or "").strip()
    if not harness or not session_id:
        typer.echo(
            f"update: no ambient identity to stamp blueprint provenance for {node_id} "
            f"(missing {'harness' if not harness else 'session_id'}); "
            "run the bind inside a session. Skipped.",
            err=True,
        )
        return
    try:
        append_session_record(
            _graph_path(), node_id, phase="blueprint",
            harness=harness, session_id=session_id,
            ended_at=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
    except (Exception, SystemExit) as exc:
        typer.echo(
            f"update: blueprint provenance stamp skipped for {node_id}: {exc}",
            err=True,
        )


def _resolve_asserted_id(
    token: str,
    entries: list,
    *,
    flag: str,
    self_id: Optional[str] = None,
) -> str:
    """Resolve a caller-asserted node reference to a canonical id, or refuse.

    The counterpart to ambient capture's degrade-to-null: a caller who names an
    origin or a peer has asserted something, and silently dropping an assertion
    that does not resolve would leave a node looking organically filed. So this
    FAILS CLOSED - non-zero exit, the unresolved
    token named, no write.

    Accepts anything the graph resolver accepts (id, slug, bare hex) rather than
    refusing on shape; passing a slug where an id is expected is the likely
    mistake and the resolver already handles it.
    """
    from fno.graph.fuzzy import resolve_node

    match = resolve_node(token, entries)
    if match.kind != "exact":
        typer.echo(f"Error: {flag} '{token}' does not resolve to a node", err=True)
        raise typer.Exit(code=1)
    resolved = match.candidates[0]["id"]
    if self_id is not None and resolved == self_id:
        typer.echo(f"Error: {flag} cannot reference the node itself ({self_id})", err=True)
        raise typer.Exit(code=1)
    return resolved


def _session_provenance(
    running_cwd: Optional[str] = None,
    *,
    source_node: Optional[str] = None,
    known_ids: Optional[set] = None,
) -> dict:
    """Parent-edge provenance for a node born inside a live session.

    Reads the running session's env + ``.fno/target-state.md`` and returns
    ``source_session_id`` / ``source_harness`` / ``source_cwd`` /
    ``source_node_id`` / ``source_plan_path``. Every key degrades to ``None``
    and the function NEVER raises (AC-EDGE).

    The origin resolves through three branches in strict precedence:

    1. ``source_node`` - an explicit ``--source-node``, already resolved and
       validated by the CLI verb. Taken as given: re-judging it would need a
       raise this function has promised not to make.
    2. The owned manifest (below, claude-only and ownership-proven).
    3. ``FNO_NODE`` - written at spawn time by the spawner. NOT gated on
       harness: it is the only origin signal a codex/opencode worker has.

    ``known_ids`` is the caller's live snapshot. When supplied, an ambiently
    resolved id absent from it degrades to ``None`` rather than stamping an edge
    that dangles, and the dropped token comes back in ``source_node_dropped``.
    Every filing path resolves it inside its locked mutator, so the check costs
    no extra read; omitting it skips the check.

    ``source_cwd`` is the originating SESSION's cwd, which is the key claude
    transcript dirs are slugged by -- distinct from the node's durable ``cwd``
    (the canonical project root). The read-back resolver needs the session cwd,
    so it is persisted separately rather than reusing the node's ``cwd``.

    Ownership of the manifest is proven exactly as ``whoami.find_held_node``
    does it: the manifest's ``claude_transcript_id`` must equal this process's
    ``CLAUDE_CODE_SESSION_ID``, so a stale / reused / foreign worktree manifest
    never leaks a node this session does not hold. Node + plan resolution is
    claude-only (the only proven transcript-resolver lane); codex/gemini stamp
    session + harness and degrade the rest.
    """
    cwd = running_cwd if running_cwd is not None else os.getcwd()

    from fno.claims.self_identity import resolve_self_identity

    identity = resolve_self_identity()
    session = identity.session_id
    harness = identity.harness

    source_node_id: Optional[str] = None
    source_plan_path: Optional[str] = None
    if session and harness == "claude":
        try:
            text = (Path(cwd) / ".fno" / "target-state.md").read_text(encoding="utf-8")
            # Current key is claude_session_id; old-key fallback for one release.
            manifest_claude_sid = _scan_md_field(text, "claude_session_id") or _scan_md_field(
                text, "claude_transcript_id"
            )
            if manifest_claude_sid == session:
                nid = _scan_md_field(text, "graph_node_id")
                if nid and nid.lower() != "null":
                    source_node_id = nid
                plan = _scan_md_field(text, "plan_path")
                if plan and plan.lower() != "null":
                    source_plan_path = plan
        except (OSError, ValueError):
            pass

    if source_node_id is None:
        source_node_id = (os.environ.get("FNO_NODE") or "").strip() or None

    dropped: Optional[str] = None
    if source_node_id is not None and known_ids is not None and source_node_id not in known_ids:
        dropped, source_node_id = source_node_id, None

    if source_node:
        # The explicit flag wins, so nothing was dropped: an ambient candidate
        # that lost a precedence contest is not a capture failure to report.
        source_node_id, dropped = source_node, None

    return {
        "source_session_id": session,
        "source_harness": harness,
        # session cwd is the transcript-resolver key; only meaningful with a session.
        "source_cwd": cwd if session else None,
        "source_node_id": source_node_id,
        "source_plan_path": source_plan_path,
        # Not a node field. An ambient signal naming a node the graph no longer
        # has is the one case worth telling the operator about: capture silently
        # regressing to nothing is what this feature exists to catch.
        "source_node_dropped": dropped,
    }


def _build_backlog_node(
    *,
    title: str,
    type_: str = "feature",
    parent: Optional[str] = None,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    priority: str = "p2",
    difficulty: Optional[str] = None,
    domain: str = "code",
    blocked_by: Optional[list[str]] = None,
    roadmap_id: Optional[str] = None,
    vision_path: Optional[str] = None,
    details: Optional[str] = None,
    size: Optional[str] = None,
    batch: Optional[str] = None,
    plan_path: Optional[str] = None,
    tags: Optional[list[str]] = None,
    source_node: Optional[str] = None,
    known_ids: Optional[set] = None,
    out: Optional[dict] = None,
) -> _NodeFields:
    """Build a backlog node dict shared by ``cmd_add`` and ``cmd_idea``.

    ``out``, when given, receives metadata ABOUT the capture that is not itself
    a node field (currently ``source_node_dropped``). A separate channel rather
    than a transient key on the returned dict, so a caller that does not know to
    strip it cannot persist it into the graph.

    Centralizes the field set so a schema addition (e.g. a new graph
    field) shows up in every entry-creating verb at once. The returned
    dict has no ``id`` - the caller assigns one inside its locked mutator
    so duplicate-ID checks happen against the live snapshot.
    """
    from fno.graph._constants import ID_PREFIX  # noqa: F401 (kept for symmetry)
    # Parent-edge provenance (x-30f6): stamped from the running session's env +
    # manifest, or from an explicit --source-node. Centralized here so
    # every creator verb (add/idea/decompose) self-describes its origin.
    prov = _session_provenance(source_node=source_node, known_ids=known_ids)
    if out is not None:
        out["source_node_dropped"] = prov["source_node_dropped"]
    return {
        "id": None,  # caller fills inside locked mutator
        "parent": parent,
        "tags": list(tags or []),
        "title": title,
        "type": type_,
        "project": project,
        "cwd": cwd,
        "priority": priority,
        "difficulty": difficulty,
        "difficulty_history": (
            [{"value": difficulty, "source": "filed", "ts": datetime.now(timezone.utc).isoformat()}]
            if difficulty is not None
            else []
        ),
        "domain": domain,
        "blocked_by": list(blocked_by or []),
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "has_brief": False,
        "roadmap_id": roadmap_id,
        "vision_path": vision_path,
        "details": details,
        "size": size,
        "batch": batch,
        "cost_usd": None,
        "cost_sessions": [],
        "plan_path": plan_path,
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "source_session_id": prov["source_session_id"],
        "source_harness": prov["source_harness"],
        "source_cwd": prov["source_cwd"],
        "source_node_id": prov["source_node_id"],
        "source_plan_path": prov["source_plan_path"],
    }


def _refuse_create_on_external_backend() -> None:
    """Refuse node-creation verbs on a non-default backend.

    On an external backend, item creation lives in the tracker (GitHub Issues,
    Linear, ...); minting an ab- entry in graph.json would create a phantom item
    the tracker has no record of. Called from EVERY creation entry point
    (_create_node_impl for add/idea, plus cmd_new, cmd_decompose, cmd_intake, and
    cmd_tree) so the guard is not decorative. A guard on only some reachable
    paths is the pitfall this exists to prevent; the parametrized test exercises
    each path so a future creation verb that bypasses it fails loudly.
    """
    from fno.tracker import active_backend_name

    backend = active_backend_name()
    if backend != "graph":
        typer.echo(
            f"fno backlog: creating work belongs to the {backend} tracker. "
            f"Create the item there; footnote tracks it by its id "
            f"(e.g. /fno:target owner/repo#N).",
            err=True,
        )
        raise typer.Exit(code=1)


def _create_node_impl(
    *,
    title: str,
    type_: str = "feature",
    parent: Optional[str] = None,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    priority: str = "p2",
    difficulty: Optional[str] = None,
    domain: str = "code",
    blocked_by: Optional[str] = None,
    roadmap_id: Optional[str] = None,
    vision_path: Optional[str] = None,
    details: Optional[str] = None,
    description: Optional[str] = None,
    size: Optional[str] = None,
    batch: Optional[str] = None,
    tags: Optional[list[str]] = None,
    source_node: Optional[str] = None,
    related: Optional[list[str]] = None,
    require_difficulty: bool = False,
) -> None:
    """Shared create-a-backlog-node body for ``cmd_add`` and ``cmd_idea``.

    Both verbs create a plan-less node (which derives to ``status: idea``);
    ``idea`` is just sugar for ``add``. Centralizing the body keeps their flag
    sets and behavior from drifting - the divergence that used to force a second
    ``fno backlog update`` just to set parent/size/domain on a fresh idea.
    """
    _refuse_create_on_external_backend()
    from fno.graph._constants import (
        DIFFICULTY_HELP,
        PRIORITY_ORDER,
        mint_node_id,
        normalize_difficulty,
    )
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import (
        VALID_NODE_TYPES,
        detect_project_from_settings,
        project_root_from_settings,
        repo_root,
    )

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    if difficulty is None and require_difficulty:
        if not sys.stdin.isatty():
            typer.echo(
                "Error: non-interactive filing requires --difficulty "
                f"({', '.join(('low', 'medium', 'high'))}). {DIFFICULTY_HELP}",
                err=True,
            )
            raise typer.Exit(code=2)
        difficulty = typer.prompt(
            "Difficulty (low|medium|high)",
            value_proc=lambda value: normalize_difficulty(value) or "",
        )
    try:
        difficulty = normalize_difficulty(difficulty)
    except ValueError as exc:
        typer.echo(f"Error: {exc}. {DIFFICULTY_HELP}", err=True)
        raise typer.Exit(code=2)

    # `update --type` has always validated; these birth paths never did, so
    # `--type task` wrote an out-of-vocabulary value straight into the graph.
    # Same set, same message - one vocabulary is the point.
    if type_ not in VALID_NODE_TYPES:
        typer.echo(
            f"Error: invalid type '{type_}'. Must be one of: "
            f"{', '.join(sorted(VALID_NODE_TYPES))}",
            err=True,
        )
        raise typer.Exit(code=1)

    if details is not None and description is not None:
        typer.echo("Error: pass --details or --description, not both", err=True)
        raise typer.Exit(code=1)
    resolved_details = details if details is not None else description

    from fno.graph._constants import normalize_tag
    try:
        resolved_tags = list(dict.fromkeys(normalize_tag(t) for t in (tags or [])))
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1)

    # Store an absolute path so downstream `detect_project()` (which compares
    # against `repo_root()` via normpath) finds matches. No explicit --cwd:
    # record the canonical main checkout (repo_root()), not os.getcwd() - a
    # backlog node outlives the worktree it was filed from.
    if cwd is not None:
        resolved_cwd = os.path.abspath(os.path.expanduser(cwd))
    elif project is not None:
        resolved_cwd = project_root_from_settings(project) or repo_root()
    else:
        resolved_cwd = repo_root()
    resolved_project = project
    if resolved_project is None:
        resolved_project = detect_project_from_settings(resolved_cwd)

    blockers: list[str] = []
    if blocked_by:
        blockers = [b.strip() for b in blocked_by.split(",") if b.strip()]

    new_id_holder: list[Optional[str]] = [None]
    node_holder: list[Optional[dict]] = [None]
    capture_meta: dict = {}
    rollup_lines: list[str] = []
    rollup_error: list[Optional[str]] = [None]

    def mutator(entries):
        live_ids = {e.get("id") for e in entries}
        # Fail closed BEFORE minting: an unresolvable assertion must leave the
        # graph untouched, and raising here aborts the locked write.
        resolved_source_node = (
            _resolve_asserted_id(source_node, entries, flag="--source-node")
            if source_node
            else None
        )
        new_id = mint_node_id(live_ids)
        new_id_holder[0] = new_id
        node = _build_backlog_node(
            title=title,
            type_=type_,
            parent=parent,
            project=resolved_project,
            cwd=resolved_cwd,
            priority=priority,
            difficulty=difficulty,
            domain=domain,
            blocked_by=blockers,
            roadmap_id=roadmap_id,
            vision_path=vision_path,
            details=resolved_details,
            size=size,
            batch=batch,
            tags=resolved_tags,
            source_node=resolved_source_node,
            known_ids=live_ids,
            out=capture_meta,
        )
        node["id"] = new_id
        # Enforce the epic-nesting cap on the create path too, or `add --type
        # epic --parent <nested-epic>` would slip a 3rd epic level past the same
        # guard cmd_update applies (x-6c2b). Scoped to a real cap violation: a
        # non-epic, or a parent that does not resolve, keeps the existing lenient
        # pass-through (add/idea has never hard-validated --parent).
        if parent:
            from fno.graph._intake import _find_node, _would_exceed_epic_depth
            from fno.graph._constants import EPIC_NEST_MAX_DEPTH

            target = _find_node(entries, parent)
            if target is not None and _would_exceed_epic_depth(entries, node, target):
                typer.echo(
                    f"Error: parenting epic {new_id} under {target['id']} would "
                    f"exceed the {EPIC_NEST_MAX_DEPTH}-level cap (mission -> epic "
                    f"-> leaf); an epic may nest only under a top-level mission",
                    err=True,
                )
                raise typer.Exit(code=1)
        entries.append(node)
        node_holder[0] = node
        # After the append so the new node is in the snapshot the mirror writes
        # against; before the rollup block, whose broad except would swallow a
        # refusal.
        if related:
            from fno.graph._intake import _parse_blocker_list
            from fno.graph.store import set_related

            set_related(
                entries,
                new_id,
                [
                    _resolve_asserted_id(t, entries, flag="--related", self_id=new_id)
                    for t in _parse_blocker_list(related)
                ],
            )
        # Rollup resolution runs INSIDE the mutator: it reads the same locked
        # snapshot the node was born into and applies an auto-link in the same
        # write, so no second lock and no window where the node exists unlinked.
        # Strictly non-fatal - any failure degrades to the orphan line (AC4).
        try:
            from fno.graph import rollup as _rollup
            from fno.graph._intake import (
                _find_node,
                _would_create_cycle,
                _would_exceed_epic_depth,
            )

            resolution = _rollup.resolve(node, entries)
            link_to: Optional[str] = None
            if resolution.kind == "linked":
                target = _find_node(entries, resolution.epic_id or "")
                # A brand-new leaf can neither cycle nor deepen epic nesting,
                # but honor the same guards the update path applies rather than
                # trusting that; a refusal falls back to suggest.
                if target is not None and not _would_exceed_epic_depth(
                    entries, node, target
                ) and not _would_create_cycle(entries, node["id"], target["id"]):
                    link_to = target["id"]
                else:
                    resolution = resolution._replace(kind="suggest")
            index = {
                e["id"]: e for e in entries
                if isinstance(e, dict) and isinstance(e.get("id"), str)
            }
            # Receipt FIRST, then the edge: an auto-link is only safe because a
            # human reads the receipt and can undo it, so a link must never land
            # without one. Anything that raises above leaves the node unlinked.
            rollup_lines[:] = _rollup.receipt_lines(resolution, node["id"], index)
            if link_to is not None:
                node["parent"] = link_to
        except Exception as exc:  # noqa: BLE001 - rollup never breaks intake
            rollup_error[0] = str(exc)
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    if rollup_error[0] is not None:
        typer.echo(f"warning: rollup skipped ({rollup_error[0]})", err=True)
    # stderr, not stdout: this verb's stdout is a machine-readable JSON payload
    # that callers pipe through `json.loads` / `jq`. The receipt is advisory
    # human output and must not corrupt that contract.
    for line in rollup_lines:
        typer.echo(line, err=True)

    # Name a captured origin on stderr, for the same reason as the rollup lines:
    # stdout is the machine-readable payload. Silent when no signal existed at
    # all, but never silent when one was found and rejected - that is the case
    # where capture regresses to nothing with no other trace.
    dropped = capture_meta.get("source_node_dropped")
    if dropped:
        typer.echo(
            f"origin: dropped '{dropped}' (not in graph); filed with no origin",
            err=True,
        )
    elif node_holder[0] is not None and node_holder[0].get("source_node_id"):
        typer.echo(f"origin: {node_holder[0]['source_node_id']}", err=True)

    # Filing-time dedup net (plan x-6ac7): warn if the just-born node resembles
    # an existing one across all live states. Post-write, fresh read, non-fatal -
    # the mutation already committed, so a failure anywhere degrades to one
    # warning and never fails the filing or touches its exit code.
    if new_id_holder[0] is not None:
        try:
            from fno.graph.store import read_graph
            from fno.graph._intake import _find_node, _warn_similar_nodes

            post_entries = read_graph(_graph_path())
            node = _find_node(post_entries, new_id_holder[0] or "")
            if node is not None:
                _warn_similar_nodes(node, post_entries, intake_hint=False)
        except Exception as e:  # noqa: BLE001 - dedup never breaks a filing
            _safe_stderr_warn(f"warning: post-file dedup check skipped: {e}\n")

    # Born-with-why: route births through the shared birth hook. Gate-first and
    # strictly non-fatal, so a gate-OFF install is a no-op and a dispatch
    # failure never wedges the filing of the node above.
    if node_holder[0] is not None:
        try:
            from fno.provenance.spawn_think import on_node_born

            on_node_born(node_holder[0])
        except Exception:  # noqa: BLE001 - born-with-why is additive; never block birth
            pass

    # Repaint the new child's ancestors so a parent epic/mission rollup reflects
    # the birth immediately (a plan-less idea still counts toward children_total).
    # The converger walks up from the child id to the epic + mission (codex P2).
    new_child_id = new_id_holder[0]
    if new_child_id is not None and node_holder[0] is not None and node_holder[0].get("parent"):
        _project_plans_from_graph([new_child_id])

    typer.echo(json.dumps({"id": new_id_holder[0], "title": title}, indent=2))


# -- add --

@cli.command(
    "add",
    epilog="Paired verb: `fno backlog remove <id>` deletes it (hidden; run its own --help).",
)
def cmd_add(
    title: str = typer.Argument(..., help="Feature title"),
    domain: str = typer.Option("code", help="Domain profile"),
    priority: str = typer.Option("p2", "--priority", "-p", help="p0|p1|p2|p3"),
    difficulty: Optional[str] = typer.Option(None, "--difficulty", help="Intrinsic work difficulty: low|medium|high."),
    blocked_by: Optional[str] = typer.Option(None, "--blocked-by", help="Comma-separated ab-IDs"),
    parent: Optional[str] = typer.Option(None, help="Parent node ab-ID"),
    type_: str = typer.Option("feature", "--type", "-t", help="Node type: feature|epic|bug|roadmap"),
    project: Optional[str] = typer.Option(
        None,
        help=(
            "Project name. Defaults to the project whose `path:` in "
            "settings.yaml matches the current working directory."
        ),
    ),
    cwd: Optional[str] = typer.Option(
        None,
        "--cwd",
        "-c",
        help="Project working directory. Defaults to the current working directory.",
    ),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id", help="Roadmap group ID"),
    vision_path: Optional[str] = typer.Option(None, "--vision-path", help="Source vision doc path"),
    details: Optional[str] = typer.Option(None, "--details", "-d", help="Implementation guidance"),
    description: Optional[str] = typer.Option(
        None,
        "--description",
        help=(
            "Alias for --details. Reads more naturally for an idea-stage "
            "row. Mutually exclusive with --details."
        ),
    ),
    size: Optional[str] = typer.Option(None, help="Size estimate: S|M|L"),
    batch: Optional[str] = typer.Option(None, help="Execution batch group"),
    tag: Optional[List[str]] = typer.Option(
        None, "--tag", hidden=True, help="Tag (repeatable, lowercase-kebab)."
    ),
    source_node: Optional[str] = typer.Option(
        None,
        "--source-node",
        help=(
            "Origin node this filing came out of (id, slug, or bare hex). Overrides "
            "ambient capture. Refuses if it does not resolve."
        ),
    ),
    related: Optional[List[str]] = typer.Option(
        None,
        "--related",
        help=(
            "Related node ids/slugs (asserted, symmetric, non-blocking). Repeat or "
            "comma-separate. Refuses an id that does not resolve."
        ),
    ),
) -> None:
    _create_node_impl(
        title=title,
        type_=type_,
        parent=parent,
        project=project,
        cwd=cwd,
        priority=priority,
        difficulty=difficulty,
        domain=domain,
        blocked_by=blocked_by,
        roadmap_id=roadmap_id,
        vision_path=vision_path,
        details=details,
        description=description,
        size=size,
        batch=batch,
        tags=tag,
        source_node=source_node,
        related=related,
    )


# -- idea (sugar verb) --

@cli.command(
    "idea",
    epilog="Paired verb: `fno backlog remove <id>` deletes it (hidden; run its own --help).",
)
def cmd_idea(
    title: str = typer.Argument(..., help="Idea title - what is this?"),
    domain: str = typer.Option("code", help="Domain profile"),
    priority: str = typer.Option("p2", "--priority", "-p", help="p0|p1|p2|p3"),
    difficulty: Optional[str] = typer.Option(None, "--difficulty", help="Intrinsic work difficulty: low|medium|high."),
    blocked_by: Optional[str] = typer.Option(None, "--blocked-by", help="Comma-separated ab-IDs"),
    parent: Optional[str] = typer.Option(None, help="Parent node ab-ID"),
    type_: str = typer.Option("feature", "--type", "-t", help="Node type: feature|epic|bug|roadmap"),
    project: Optional[str] = typer.Option(
        None,
        help=(
            "Project name. Defaults to the project whose `path:` in "
            "settings.yaml matches the current working directory."
        ),
    ),
    cwd: Optional[str] = typer.Option(
        None,
        "--cwd",
        "-c",
        help="Working directory. Defaults to the current working directory.",
    ),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id", help="Roadmap group ID"),
    vision_path: Optional[str] = typer.Option(None, "--vision-path", help="Source vision doc path"),
    details: Optional[str] = typer.Option(None, "--details", "-d", help="Implementation guidance"),
    description: Optional[str] = typer.Option(
        None,
        "--description",
        help=(
            "Alias for --details. Reads more naturally for an idea-stage "
            "row. Mutually exclusive with --details."
        ),
    ),
    size: Optional[str] = typer.Option(None, help="Size estimate: S|M|L"),
    batch: Optional[str] = typer.Option(None, help="Execution batch group"),
    tag: Optional[List[str]] = typer.Option(
        None, "--tag", hidden=True, help="Tag (repeatable, lowercase-kebab)."
    ),
    source_node: Optional[str] = typer.Option(
        None,
        "--source-node",
        help=(
            "Origin node this filing came out of (id, slug, or bare hex). Overrides "
            "ambient capture. Refuses if it does not resolve."
        ),
    ),
    related: Optional[List[str]] = typer.Option(
        None,
        "--related",
        help=(
            "Related node ids/slugs (asserted, symmetric, non-blocking). Repeat or "
            "comma-separate. Refuses an id that does not resolve."
        ),
    ),
) -> None:
    """Capture an idea (a plan-less backlog node) with minimal ceremony.

    Equivalent to `fno backlog add <title>` but signals intent to skip the
    spec/plan ceremony for now. The new node has no ``plan_path`` and so
    derives to ``status: idea`` until a plan is associated (via
    ``fno backlog intake`` or by setting ``--plan-path`` on
    ``fno backlog update``). Shares ``add``'s full option set so a fresh idea
    can carry parent/size/domain without a follow-up ``fno backlog update``.
    """
    _create_node_impl(
        title=title,
        type_=type_,
        parent=parent,
        project=project,
        cwd=cwd,
        priority=priority,
        difficulty=difficulty,
        domain=domain,
        blocked_by=blocked_by,
        roadmap_id=roadmap_id,
        vision_path=vision_path,
        details=details,
        description=description,
        size=size,
        batch=batch,
        tags=tag,
        source_node=source_node,
        related=related,
        require_difficulty=True,
    )


# -- decompose (bounded epic -> group child nodes) --

@cli.command("decompose", hidden=True)
def cmd_decompose(
    ctx: typer.Context,
    epic_id: str = typer.Argument(..., help="Epic node ab-ID to decompose into group children"),
    groups: str = typer.Option(
        ...,
        "--groups",
        help=(
            "JSON array of {slug,title,waves,blocked_by_groups[,project][,cwd]"
            "[,adopt]} group specs. Optional per-group project/cwd route a child "
            "into a different repo (multi-repo decomposition): project resolves "
            "its cwd from the settings work-map; an explicit cwd overrides; "
            "absent -> inherit the epic's repo. "
            "Optional `adopt: [<node-id>...]` re-parents EXISTING nodes under the "
            "group child instead of minting new ones - use it to package an epic "
            "already populated by `fno backlog idea --parent` rather than "
            "doubling it. Epic children no group adopts are named on stderr. "
            "Prefix '@' to read a file (--groups @groups.json) or pass '-' to read stdin."
        ),
    ),
    max_prs: Optional[int] = typer.Option(
        None,
        "--max-prs",
        help=(
            "Ceiling on group/PR count. Rejects when groups exceed it (N is a "
            "ceiling, not a quota). Defaults to config.blueprint.max_prs_per_epic. "
            "An epic doc's `max_children:` frontmatter overrides that default; "
            "--max-prs may then only tighten it (never loosen the author's cap)."
        ),
    ),
    force: bool = typer.Option(
        False,
        "--force", "-F",
        help="Allow a re-decomposition that orphans an already-shipped group child node.",
    ),
    plans: str = typer.Option(
        "separate",
        "--plans",
        help=(
            "Per-child plan packaging. Only 'separate' is supported: scaffold a "
            "self-contained quick-plan stub per child and repoint its plan_path "
            "to that file (one plan == one PR == one node). The former 'fragment' "
            "packaging (a <epic-doc>#group-<slug> section of a shared doc) was "
            "removed - it is still recognized on existing children for idempotent "
            "re-decompose, but never authored."
        ),
    ),
) -> None:
    """Upsert group child nodes under an epic (atomic + idempotent).

    Each group becomes one child node (parent=epic) bundling 1+ execution waves
    into a single shippable PR, with its own self-contained
    <stem>.group-<slug>.md quick-plan (the only packaging). Re-running with the
    same slugs updates the existing children in place rather than duplicating,
    keyed on the slug - and a child still on the legacy <epic-doc>#group-<slug>
    fragment form is repointed to its separate file. The whole decomposition
    lands in one locked graph mutation, so a bad spec leaves the graph exactly
    as it was (AC1-FR).
    """
    _refuse_create_on_external_backend()
    import sys as _sys
    from fno.graph._constants import mint_node_id
    from fno.graph.store import locked_mutate_graph, read_graph, GraphUnreadableError
    from fno.graph._intake import _find_node, _would_create_cycle
    from fno.graph._decompose import (
        _UNSET,
        DecomposeError,
        canonical_child_plan_path,
        child_plan_path,
        classify_group_dep,
        extract_contract_versions,
        extract_why_digest,
        find_orphans,
        group_child_slug,
        is_group_child,
        is_shipped,
        plan_base,
        resolve_effective_cap,
        scaffold_separate_plan,
        separate_plan_path,
        validate_groups,
    )
    from fno.graph._intake import _read_plan_frontmatter
    from fno.handoff.output import emit_error, json_mode

    if plans == "fragment":
        emit_error(
            ctx,
            "--plans fragment was removed; 'separate' is now the only packaging "
            "(one plan == one PR == one node). Drop the flag or pass --plans separate.",
        )
        raise typer.Exit(code=1)
    if plans != "separate":
        emit_error(ctx, f"--plans must be 'separate' (got {plans!r})")
        raise typer.Exit(code=1)
    separate = True

    # 1. Read the --groups source ('@file', '-' stdin, or a JSON literal),
    #    keeping read vs parse failures distinct so the message names the cause.
    try:
        if groups == "-":
            raw = _sys.stdin.read()
        elif groups.startswith("@"):
            raw = Path(groups[1:]).expanduser().read_text(encoding="utf-8")
        else:
            raw = groups
    except OSError as e:
        emit_error(ctx, f"could not read --groups file {groups[1:]!r}: {e}")
        raise typer.Exit(code=1)
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as e:
        emit_error(ctx, f"--groups is not valid JSON: {e}")
        raise typer.Exit(code=1)

    # 2. Resolve the ceiling. Precedence: a `max_children` in the epic doc's
    #    frontmatter is the author's durable per-epic cap (overrides the config
    #    default upward; an explicit --max-prs may only tighten it). With no
    #    max_children, resolution is byte-identical to before: explicit --max-prs
    #    else config.blueprint.max_prs_per_epic.
    explicit_max_prs = max_prs  # captured before any fallback overwrites the None sentinel

    #    Read the epic's max_children read-only, pre-lock (advisory; the locked
    #    mutator re-resolves the epic). Any read failure -> absent cap -> current
    #    behavior (_read_plan_frontmatter fails safe to {}). A present-but-invalid
    #    value (incl. explicit null) is rejected by resolve_effective_cap below.
    #    _UNSET (not None) marks "no key", so an explicit `max_children: null`
    #    fails closed instead of masquerading as absent.
    max_children: object = _UNSET
    epic_doc_rel: Optional[str] = None
    try:
        epic_node = _find_node(read_graph(_graph_path()), epic_id)
        epic_plan_path = epic_node.get("plan_path") if epic_node else None
        if epic_node is not None and epic_plan_path:
            epic_doc = plan_base(epic_plan_path)
            # Resolve a relative plan_path against the epic's stored cwd, mirroring
            # the in-lock base resolution - reading it against the process cwd would
            # miss the doc when decompose runs elsewhere and silently drop the cap.
            if not os.path.isabs(epic_doc):
                epic_doc = os.path.join(epic_node.get("cwd") or os.getcwd(), epic_doc)
            max_children = _read_plan_frontmatter(epic_doc).get("max_children", _UNSET)
            try:
                epic_doc_rel = os.path.relpath(
                    epic_doc, epic_node.get("cwd") or os.getcwd()
                )
            except ValueError:
                epic_doc_rel = os.path.basename(epic_doc)
    except (DecomposeError, GraphUnreadableError, OSError):
        max_children = _UNSET  # fail-safe: no cap, current behavior

    #    Read config ONLY on the true fallback path (no max_children, no explicit
    #    flag). A valid max_children or an explicit --max-prs makes config
    #    irrelevant, so an unrelated config error must not abort decompose there.
    config_default: Optional[int] = None
    if max_children is _UNSET and explicit_max_prs is None:
        from fno.config import load_settings
        try:
            config_default = load_settings().blueprint.max_prs_per_epic
        except Exception as e:
            emit_error(ctx, f"could not read config.blueprint.max_prs_per_epic: {e}")
            raise typer.Exit(code=1)

    try:
        effective_cap, cap_source = resolve_effective_cap(
            max_children, explicit_max_prs, config_default, epic_doc_rel
        )
    except DecomposeError as e:
        emit_error(ctx, str(e))
        raise typer.Exit(code=e.exit_code)

    # 3. Validate the spec entirely before touching the graph (atomicity).
    try:
        norm = validate_groups(parsed, effective_cap, cap_source, epic_id)
    except DecomposeError as e:
        emit_error(ctx, str(e))
        raise typer.Exit(code=e.exit_code)

    # 3b. Resolve per-group repo routing OUTSIDE the graph lock (settings reads
    #     never happen under the lock, mirroring `update`). A group with an
    #     explicit cwd uses it as-is; a group with only a project derives its
    #     cwd from the work-map and is REFUSED (atomically, before any write) if
    #     that project is unmapped - guessing a cwd would silently record foreign
    #     work under the wrong repo and break spawn-into-project. No project/cwd
    #     -> (None, None) = inherit the epic's repo (the single-repo default).
    from fno.graph._intake import project_root_from_settings
    slug_route: dict[str, tuple[Optional[str], Optional[str]]] = {}
    for grp in norm:
        gproj, gcwd = grp["project"], grp["cwd"]
        if gcwd is not None:
            slug_route[grp["slug"]] = (gproj, os.path.abspath(os.path.expanduser(gcwd)))
        elif gproj is not None:
            root = project_root_from_settings(gproj)
            if root is None:
                emit_error(
                    ctx,
                    f"group {grp['slug']!r} project {gproj!r} is not in any "
                    "settings.yaml work-map; add it there or pass an explicit cwd",
                )
                raise typer.Exit(code=1)
            slug_route[grp["slug"]] = (gproj, root)
        else:
            slug_route[grp["slug"]] = (None, None)

    keep_slugs = {g["slug"] for g in norm}
    results: list[dict] = []
    epic_id_box: list[str] = [epic_id]

    def mutator(graph_entries):
        # Resolve the epic inside the locked snapshot so a corrupt graph
        # surfaces as exit 1 (via locked_mutate_graph) rather than masquerading
        # as "epic not found".
        live_epic = _find_node(graph_entries, epic_id)
        if live_epic is None:
            raise DecomposeError(f"epic node {epic_id} not found", exit_code=3)
        epic_resolved_id = live_epic["id"]
        epic_id_box[0] = epic_resolved_id
        base = plan_base(live_epic.get("plan_path"))
        verbatim_base_box[0] = base  # the relative base, for the source_doc seed
        # `base` (verbatim, possibly relative) is the node-identity key used by
        # child_plan_path below - DO NOT mutate it. For the set-expected
        # shell-out only, resolve a relative base against the epic's project
        # root (its stored cwd) so a decompose run from a subdirectory still
        # locates the doc on disk; fno.plan._stamp resolves relative paths against
        # the process cwd, which would otherwise false-"missing" and skip
        # writing the count (reintroducing early graduation).
        if base and not os.path.isabs(base):
            base_box[0] = os.path.join(live_epic.get("cwd") or os.getcwd(), base)
        else:
            base_box[0] = base
        # Stash the epic's cwd (US4) so the post-lock scaffold step can resolve an
        # inherited child's child_root outside the lock (mirrors base_box).
        epic_cwd_box[0] = live_epic.get("cwd")

        # Read the epic doc's pinned interface-contract version(s). The doc is
        # the single source of truth: a `contract`-tier group is eligible only
        # when the doc pins a `## Interface Contract` (G1); with no pin every
        # `contract` request falls back to `hard` (AC2-HP). A missing/unreadable
        # doc -> no pin -> all hard (fail-safe; the downgrade is reported, never
        # silent). Local file read under the lock is trivial (the doc is small).
        pinned_versions: set[int] = set()
        if base_box[0]:
            try:
                pinned_versions = extract_contract_versions(
                    Path(base_box[0]).read_text(encoding="utf-8")
                )
            except (OSError, UnicodeDecodeError):
                # No readable doc -> no pin -> contract falls back to hard. Never
                # hard-fail decompose on a doc-read issue (mirrors the stamp path).
                pinned_versions = set()

        # Refuse to orphan an already-shipped group child unless --force.
        orphans = find_orphans(graph_entries, epic_resolved_id, base, keep_slugs)
        shipped_orphans = [o for o in orphans if is_shipped(o)]
        if shipped_orphans and not force:
            ids = ", ".join(o["id"] for o in shipped_orphans)
            raise DecomposeError(
                f"re-decomposition would orphan already-shipped group node(s): {ids}. "
                "Re-run with --force to proceed, or keep their #group slugs.",
                exit_code=2,
            )

        # Pass 1: resolve each group to an existing or new child node.
        slug_to_id: dict[str, str] = {}
        # Resolved adopt claims, keyed on the node id `_find_node` returns
        # rather than the spelling the spec used. validate_groups can only
        # compare raw strings, and a 4-7 hex `ab-` prefix resolves to the same
        # entry as the full id - so `ab-abcd` in one group and `ab-abcd0001` in
        # another read as two claims there and are one node here.
        adopt_claim: dict[str, str] = {}
        plan_to_group: list[tuple[dict, dict]] = []  # (node, normalized group)
        for grp in norm:
            frag_path = child_plan_path(base, grp["slug"])
            sep_path = separate_plan_path(base, grp["slug"])
            # Tolerant lookup: identity is the durable group_slug (x-edf7 US2), so
            # a child born unlinked (no plan_path yet) is still found; the legacy
            # plan_path match (fragment or separate form) upserts a pre-field child
            # in place instead of duplicating (idempotent on slug across migration).
            existing = next(
                (
                    e
                    for e in graph_entries
                    if e.get("parent") == epic_resolved_id
                    and (
                        e.get("group_slug") == grp["slug"]
                        or e.get("plan_path") in (frag_path, sep_path)
                    )
                ),
                None,
            )
            route_proj, route_cwd = slug_route[grp["slug"]]
            if existing is not None:
                action = "updated"
                node = existing
                node["group_slug"] = grp["slug"]  # backfill identity on legacy children
                # Preserve a designed child's plan_path; NEVER link an unlinked
                # child here (linking is the inline-fill / fan-out step's job, US2).
                # The one exception is the documented legacy-fragment repoint:
                # a child still on `<doc>#group-<slug>` moves to its separate file
                # (staying linked/ready), never unset.
                if node.get("plan_path") == frag_path:
                    node["plan_path"] = sep_path
                # Re-running with an explicit route reprojects an existing child
                # (e.g. a first pass inherited the epic's repo, a later pass adds
                # per-group routing). No route leaves the child's repo untouched.
                if route_proj is not None:
                    node["project"] = route_proj
                if route_cwd is not None:
                    node["cwd"] = route_cwd
            else:
                action = "created"
                # Born UNLINKED (plan_path=None -> derives `idea`): linking the
                # filled plan is the design-completion signal that flips the child
                # `ready` (x-edf7 US2, Locked Decision 4). group_slug is the durable
                # identity that survives the unlinked window.
                node = _build_backlog_node(
                    title=grp["title"],
                    parent=epic_resolved_id,
                    project=route_proj if route_proj is not None else live_epic.get("project"),
                    cwd=route_cwd if route_cwd is not None else live_epic.get("cwd"),
                    priority=live_epic.get("priority", "p2"),
                    domain=live_epic.get("domain", "code"),
                    plan_path=None,
                    known_ids={e.get("id") for e in graph_entries},
                )
                node["group_slug"] = grp["slug"]
                node["id"] = mint_node_id({e.get("id") for e in graph_entries})
                # Reuse the parent-setter's cycle detection on this path too
                # (plan Invariants, line 88). A freshly minted id cannot be an
                # ancestor of the epic, so this never trips for new nodes today;
                # it guards future paths that re-parent an existing node.
                if _would_create_cycle(graph_entries, node["id"], epic_resolved_id):
                    raise DecomposeError(
                        f"parenting group {grp['slug']!r} to {epic_resolved_id} would create a cycle",
                        exit_code=2,
                    )
                graph_entries.append(node)
            slug_to_id[grp["slug"]] = node["id"]
            plan_to_group.append((node, grp))

            # Adoption (x-b9d7 US2): re-parent each named node under this group
            # child, inside the same locked mutation. Nothing is minted for it
            # and nothing is deleted; its plan_path, details, priority, and
            # evidence are untouched, because membership is carried by the
            # `parent` pointer alone. Why adopted nodes never get `group_slug`
            # is on the NormalizedGroup field in _decompose.py.
            adopted: list[str] = []
            # A DEAD delivery unit cannot own containment. Once per
            # group, before the adopt loop, because deadness is a property of
            # the owner rather than of each target - and it has to run BEFORE
            # the stamp's back-fill convergence leg at :1739, which is exactly
            # what re-applies containment that the death transition released.
            #
            # Unreachable by every repair path if it lands:
            # _release_contained_children fires once, at the moment the unit
            # dies, and nothing re-runs it (cmd_undefer deliberately does not
            # re-contain); _strandable_contained_ids keys on the owner's
            # completed_at, which a deferred or superseded owner does not have;
            # and both dispatch halves then refuse the adoptee forever
            # (selection_guards' `contained:` guard, _redirect_if_contained).
            # The only escape is a manual `--parent null` per node.
            #
            # Fields, not the derived `status` string: recompute_statuses
            # re-persists status after every locked mutation, so it is a
            # snapshot rather than truth (selection_guards' dead-ancestor guard
            # documents the same reasoning). The legacy `completed_at:
            # "deferred:<ts>"` workaround (pre-feature deferral overloaded
            # completed_at) is folded into the effective deferred_at here:
            # recompute_statuses migrates it to deferred_at only AFTER this
            # mutator returns, so reading completed_at raw would treat a
            # deferred owner as done, skip this guard, and stamp exactly the
            # permanently-contained state it exists to prevent. A genuinely-done
            # owner (a real completed_at timestamp, not the legacy marker) is
            # exempt because the merge cascade can heal it, reproducing the
            # done > superseded > deferred precedence so a unit that was
            # superseded and later closed does not trip a false refusal. Both
            # death fields are read even though every writer sets deferred_at
            # today, so a future supersede path that forgets it cannot slip
            # through.
            #
            # Gated on a non-empty adopt list: a dead group child with nothing
            # to adopt still updates its title, waves, and blocked_by as it does
            # today. Refusing there would deadlock an epic doc whose group was
            # deferred for unrelated reasons.
            _completed = node.get("completed_at")
            _legacy_defer = isinstance(_completed, str) and _completed.startswith(
                _LEGACY_DEFER_PREFIX
            )
            if grp["adopt"] and not (bool(_completed) and not _legacy_defer) and (
                node.get("deferred_at") or node.get("superseded_by") or _legacy_defer
            ):
                _superseder = node.get("superseded_by")
                # The remedy branches on the cause: `fno backlog undefer` clears
                # deferred_at only and never superseded_by (:6046), so offering
                # it to a superseded owner sends the operator through a command
                # that exits 0 and changes nothing this refusal reads.
                if _superseder:
                    _how = f"was superseded by {_superseder}"
                    _remedy = (
                        f"Run `fno backlog unsupersede {node['id']}` to revive it "
                        "(clears superseded_by; `undefer` does not), or point the "
                        "adopt list at the superseding node, or give the group a "
                        "new slug so it mints a live delivery unit"
                    )
                else:
                    _how = "is deferred"
                    _remedy = (
                        f"Run `fno backlog undefer {node['id']}` first, or drop "
                        "the adopt list from this group"
                    )
                raise DecomposeError(
                    f"group {grp['slug']!r} resolves to {node['id']}, which "
                    f"{_how}; its death already released the nodes it contained "
                    "and nothing re-runs that release, so stamping containment "
                    "here would leave every adoptee undispatchable with no verb "
                    f"to free it. {_remedy}",
                    exit_code=2,
                )
            for adopt_id in grp["adopt"]:
                target = _find_node(graph_entries, adopt_id)
                if target is None:
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopts {adopt_id}, which resolves "
                        "to no node",
                        exit_code=3,
                    )
                if target["id"] == epic_resolved_id:
                    # validate_groups makes the same check against the RAW epic
                    # argument, so an aliasable `ab-` prefix of the epic slips
                    # past it and would otherwise land on the generic cycle
                    # refusal (exit 2) instead of this one (exit 1).
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopt names the epic "
                        f"{epic_resolved_id} itself",
                        exit_code=1,
                    )
                prior_slug = adopt_claim.get(target["id"])
                if prior_slug is not None:
                    raise DecomposeError(
                        f"node {target['id']} is claimed by more than one adopt "
                        f"entry (group {prior_slug!r} and group {grp['slug']!r}); "
                        "two spellings of one id resolve to the same node",
                        exit_code=1,
                    )
                adopt_claim[target["id"]] = grp["slug"]
                # Unscoped on purpose: group_child_slug answers "group child of
                # THIS doc", which misses a legacy child of ANOTHER epic and
                # lets adoption steal it. Also covers self-adoption, since a
                # group naming its own resolved id is a group child by
                # construction.
                if is_group_child(target):
                    owner = group_child_slug(target, base)
                    if owner:
                        whose = f"already the group child for slug {owner!r}"
                    elif target.get("parent") == epic_resolved_id:
                        # This epic's own group child on a plan path that no
                        # longer matches the doc (a rename). Saying "another
                        # epic" here would be a plain lie about the operator's
                        # own node.
                        whose = (
                            "already this epic's group child on a legacy plan "
                            f"path ({target.get('plan_path')}) that no longer "
                            "matches the epic doc"
                        )
                    else:
                        whose = (
                            "already a group child of another epic "
                            f"({target.get('plan_path')})"
                        )
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopts {target['id']}, which is "
                        f"{whose}; demoting a group into a task "
                        "reshapes the epic",
                        exit_code=2,
                    )
                # Refuse to adopt a node someone is actively building (codex
                # P1). Every dispatch gate reads containment BEFORE the claim -
                # the in-process redirect earliest of all - so adoption landing
                # in that window produced a worker holding a claim on a node
                # that is now contained: it writes its manifest and builds the
                # second PR for one plan anyway.
                #
                # Closed from THIS side rather than by re-validating after the
                # claim, because it is the single boundary: it covers every
                # dispatch path at once instead of one script, it prevents the
                # contradictory state rather than killing a worker that already
                # started, and it reports to the person running decompose, who
                # has the context to fix the adopt list. A live claim also means
                # the node is a delivery unit in practice right now, which is
                # the thing adoption asserts it is not.
                #
                # Reads the LIVE lockfile, not the graph's `locked_by` mirror:
                # the manifest claim fields are an init-time snapshot and can
                # lie after a respawn. Suspect counts as held (x-ba4b).
                _holder = _live_worker(target["id"])
                if _holder:
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopts {target['id']}, which is "
                        f"being built right now by {_holder}; adopting it would "
                        "leave that session holding a claim on a node that no "
                        "longer dispatches, and it would still open its own PR. "
                        "Wait for it to land, or stop it first",
                        exit_code=2,
                    )
                # Cycle + descendants BOTH run before the stamp and before the
                # already-adopted `continue` below. Placing the descendants
                # refusal after that short-circuit made it unreachable on the
                # BACK-FILL path (sigma): re-running a spec against a legacy
                # adopted node that has since gained children stamped it anyway,
                # producing the exact half-closed subtree the refusal exists to
                # prevent. Cycle stays first so an ancestor-of-the-epic adoptee
                # still gets its specific message rather than the vaguer one.
                if _would_create_cycle(graph_entries, target["id"], node["id"]):
                    raise DecomposeError(
                        f"adopting {target['id']} into group {grp['slug']!r} "
                        "would create a cycle",
                        exit_code=2,
                    )
                # An adoptee with descendants (codex P1). Containment is ONE
                # level by design, and selection_guards does not treat a
                # contained ANCESTOR as a guard - so the children would stay
                # independently dispatchable while the merge cascade closed only
                # this parent, leaving them open to build separate PRs. Refusing
                # is right rather than propagating: a subtree is a decomposition
                # of its own, and folding it wholesale into another unit is a
                # reshape the operator should state explicitly.
                _kids = [
                    e.get("id") for e in graph_entries
                    if isinstance(e, dict) and e.get("parent") == target["id"]
                ]
                if _kids:
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopts {target['id']}, which has "
                        f"{len(_kids)} child(ren) ({', '.join(str(k) for k in _kids[:3])}"
                        f"{'...' if len(_kids) > 3 else ''}); containment is one "
                        "level, so they would stay dispatchable and open their own "
                        "PRs while their parent closed. Adopt the children "
                        "individually, or re-parent them out first",
                        exit_code=2,
                    )
                # Containment (x-e957), stamped BEFORE the already-adopted
                # short-circuit so re-running the spec CONVERGES: a node adopted
                # by an older fno (parent set, no contained_in) is back-filled
                # rather than left half-adopted forever. Writing the value it
                # already holds is a no-op, so re-running an up-to-date spec
                # still serializes byte-identically.
                #
                # Same locked mutation as the re-parent below, deliberately: two
                # writes would open a window where the node is re-parented but
                # still armed for dispatch, which is the exact state this field
                # exists to make impossible.
                # Containment says "this node has no PR of its own". A node that
                # already carries one, or that already carries cost, HAS
                # independent delivery evidence, so the field simply does not
                # apply to it (codex P1/P2) - and stamping it anyway would hide
                # an open PR's node from dispatch, auto-close it under someone
                # else's merge while its own PR is still open, and report a
                # finished one as having "shipped inside" a unit it predates.
                # Its cost also stays in the flat project sum regardless, because
                # _apply_rollup reads an empty rollup as "preserve existing", so
                # the double-count the rollup guard prevents for NEW attribution
                # would simply persist for old.
                #
                # Adoption itself still proceeds: re-parenting changes rollup
                # membership, not delivery state, which is exactly what
                # test_adopt_a_shipped_node_is_permitted pins. Only the
                # containment stamp is withheld, and loudly.
                _own_pr = target.get("pr_number")
                _own_cost = target.get("cost_usd")
                _is_done = bool(target.get("completed_at")) or target.get("status") == "done"
                if (_own_pr or _own_cost is not None) and not _is_done:
                    # An UNFINISHED delivery unit cannot be adopted at all
                    # (codex P1). Withholding the stamp keeps it dispatchable,
                    # but re-parenting still hangs it under the group child -
                    # and _cascade_close_parents only asks whether the EPIC's
                    # direct children are complete, never its grandchildren. So
                    # the group's merge would close the epic (and dispatch its
                    # dependents) over work that is still open one level down.
                    # Refuse: a node mid-flight with its own PR or cost is a
                    # delivery unit, and folding one into another is a reshape
                    # the operator has to state, not something adopt infers.
                    _what = f"has an open PR (#{_own_pr})" if _own_pr else "has accrued cost"
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopts {target['id']}, which "
                        f"{_what} and has not landed; it is its own delivery "
                        "unit mid-flight. Adopting it would hang open work under "
                        "the group, and the epic would close over it when the "
                        "group merges. Let it land first, or drop it from the "
                        "adopt list",
                        exit_code=2,
                    )
                if _own_pr or _own_cost is not None:
                    _why = "carries PR #%s" % _own_pr if _own_pr else "carries cost"
                    uncontained_box[0].append(
                        f"warning: adopted {target['id']} into group "
                        f"{grp['slug']!r} but did NOT mark it contained: it "
                        f"{_why}, so it is its own delivery unit. It stays "
                        "separately dispatchable, separately costed, and is not "
                        "closed by the group's merge."
                    )
                else:
                    target["contained_in"] = node["id"]
                if target.get("parent") == node["id"]:
                    continue  # already adopted - re-running the spec is a no-op
                target["parent"] = node["id"]
                adopted.append(target["id"])

            results.append(
                {
                    "id": node["id"],
                    "slug": grp["slug"],
                    "waves": grp["waves"],
                    "action": action,
                    "adopted": adopted,
                }
            )

        # Pass 2: set titles + inter-group blocked_by now that all ids exist.
        for (node, grp), r in zip(plan_to_group, results):
            node["title"] = grp["title"]
            # Set details unconditionally so a re-decompose that clears a
            # group's waves does not leave stale wave metadata behind.
            node["details"] = (
                f"Waves {grp['waves']} of epic {epic_resolved_id}" if grp["waves"] else None
            )
            node["blocked_by"] = [slug_to_id[d] for d in grp["blocked_by_groups"]]
            r["blocked_by"] = list(node["blocked_by"])

            # Classify the dependency tier against the doc's pin. Stamp the
            # contract fields ONLY on a `contract` dep; pop them on `hard` so a
            # re-decompose downgrade (contract -> hard) cleans up stale stub
            # metadata and the pure-hard path serializes byte-for-byte unchanged
            # (Invariant). The downgrade reason, if any, is surfaced after the lock.
            dep, stub_against, cversion, downgrade = classify_group_dep(
                grp, pinned_versions, base
            )
            if dep == "contract":
                node["dep"] = "contract"
                node["stub_against"] = stub_against
                node["contract_version"] = cversion
                r["dep"] = "contract"
            else:
                node.pop("dep", None)
                node.pop("stub_against", None)
                node.pop("contract_version", None)
                r["dep"] = "hard"
            if downgrade:
                downgrade_box[0].append(downgrade)

        # Surface any unshipped orphans (slug dropped from the spec). They are
        # left in place, not deleted - deleting graph nodes is destructive.
        orphan_box[0] = [o["id"] for o in orphans]

        # Epic children that no group adopted (x-b9d7 US3). Uses the SAME
        # predicate as the adoption refusal above, deliberately: keying this on
        # the base-scoped group_child_slug instead would list a group child
        # whose plan path no longer matches a renamed epic doc, and the warning
        # would then tell the operator to adopt a node the refusal blocks.
        # Collected after the re-parenting above, so
        # adopted nodes have already left the epic and exclude themselves; no
        # cross-reference against the adopt lists is needed. Warning, not
        # refusal: refusing deadlocks, because re-parenting needs the group
        # nodes a refusal prevents creating, and a parked child is a legitimate
        # steady state.
        unadopted_box[0] = [
            e.get("id")
            for e in graph_entries
            if e.get("id")
            and e.get("parent") == epic_resolved_id
            and not is_group_child(e)
        ]
        return graph_entries

    orphan_box: list[list[str]] = [[]]
    unadopted_box: list[list[str]] = [[]]
    # Adoptees that were re-parented but deliberately NOT marked contained
    # (they carry their own PR or cost). Same box-then-emit shape as the
    # unadopted warning: collected inside the locked mutator, printed after.
    uncontained_box: list[list[str]] = [[]]
    base_box: list = [None]
    verbatim_base_box: list = [None]
    epic_cwd_box: list = [None]
    downgrade_box: list[list[str]] = [[]]
    try:
        locked_mutate_graph(_graph_path(), mutator)
    except DecomposeError as e:
        emit_error(ctx, str(e))
        raise typer.Exit(code=e.exit_code)

    epic_resolved_id = epic_id_box[0]
    orphan_ids = orphan_box[0]
    unadopted_ids = unadopted_box[0]
    # Deduped: locked_mutate_graph may re-enter the mutator, and this box appends
    # where the sibling boxes assign.
    for _uc in sorted(set(uncontained_box[0])):
        typer.echo(_uc, err=True)
    downgrades = downgrade_box[0]

    # Shared post-mutation graph re-read: 3c reads each child's created_at +
    # plan_path from it, and fan-out 4a reuses it. One read, not two. A read
    # failure degrades to an empty map (scaffold falls back to today's date, the
    # fan-out step is a no-op) rather than wedging the already-committed mutation.
    from fno.graph.store import read_graph as _read_graph
    try:
        by_id = {e.get("id"): e for e in _read_graph(_graph_path())}
    except Exception:  # noqa: BLE001 - never wedge the report on a re-read failure
        by_id = {}

    # 3c. Scaffold per-child quick-plan files (--plans separate). Runs OUTSIDE
    #     the graph lock (mirrors the _set_expected_count doc write below), because
    #     it reads settings (plans_content_dir walks .claude/settings) and settings
    #     reads never happen under the lock. Each child is born at its CANONICAL
    #     `fno do plan path` name, routed into the CHILD project's plans dir - not the
    #     epic's dir, not the legacy `.group-<slug>.md` name (x-d6a6).
    # US4: transcribe the epic's why (intent + Locked Decisions) once - every child
    # scaffold is born grounded, AND a fan-out seed carries it so the /think worker
    # stays grounded even when its origin transcript is unresolved. A missing
    # Locked-Decisions block degrades to intent-only + a warning; an unreadable doc
    # yields an empty digest (the scaffold then seeds the validator-rejected stub).
    why_digest = ""
    if separate and base_box[0]:
        try:
            why_digest, why_warn = extract_why_digest(
                Path(base_box[0]).read_text(encoding="utf-8")
            )
            if why_warn:
                typer.echo(f"warning: {why_warn}", err=True)
        except (OSError, UnicodeDecodeError):
            pass

    scaffolded: list[str] = []
    if separate and base_box[0]:
        from fno.graph._intake import repo_root
        source_doc = verbatim_base_box[0] or base_box[0]
        id_by_slug = {r["slug"]: r["id"] for r in results}
        for grp in norm:
            slug = grp["slug"]
            child_id = id_by_slug.get(slug)
            if not child_id:
                continue
            child = by_id.get(child_id)
            # Skip 1: already linked - never spawn a stub beside a filled plan
            # (Locked Decision 6; also grandfathers a repointed legacy fragment).
            if child and child.get("plan_path"):
                continue
            # Skip 2: a legacy `.group-<slug>.md` file exists - grandfather it in
            # place, no rename, no canonical duplicate (Locked Decision 4).
            if Path(separate_plan_path(base_box[0], slug)).exists():
                continue
            # Route the stub into the CHILD project's plans dir. The child node's
            # own cwd is the authoritative per-child root: the mutator set it to
            # the routed cwd (or inherited the epic's) at mint, so it already
            # reflects a route made WITHOUT an explicit re-route - a routed child
            # re-decomposed with no route keeps its own repo, not the epic's.
            # created_at sources the filename date, so a later-day re-decompose
            # recomputes the SAME path (idempotent). Fall back to the epic cwd
            # then repo_root() only if the re-read lost the node.
            child_root = (
                (child.get("cwd") if child else None)
                or epic_cwd_box[0]
                or repo_root()
            )
            canonical = Path(
                canonical_child_plan_path(
                    slug, child_id, str(child_root),
                    child.get("created_at") if child else None,
                )
            )
            # Skip 3: canonical already on disk - idempotent re-run.
            if canonical.exists():
                continue
            try:
                canonical.parent.mkdir(parents=True, exist_ok=True)
                # Seed the folded nodes (discovery children adopted into this one
                # PR) as a coverage checklist, so a fresh-context builder sees
                # every commitment the plan must address (x-d9a4 task 1.7).
                adopted_nodes = [
                    (aid, (by_id.get(aid) or {}).get("title") or aid)
                    for aid in (grp.get("adopt") or [])
                ]
                canonical.write_text(
                    scaffold_separate_plan(
                        grp, epic_resolved_id, source_doc,
                        why_digest=why_digest,
                        adopted=adopted_nodes,
                    ),
                    encoding="utf-8",
                )
                scaffolded.append(str(canonical))
            except OSError as e:
                # Non-fatal: the graph is already the source of truth. Warn loudly
                # so the missing stub is visible, never silently swallowed.
                typer.echo(
                    f"warning: could not scaffold separate plan {canonical}: {e}",
                    err=True,
                )

    # 4a. Per-child design pass (x-edf7 US3) + born-with-why (v2 A1). Runs BEFORE
    #     the report so a flagged fan-out's outcome rides in the --json payload
    #     (a machine caller must see when a child was left an unlinked idea, not a
    #     silent success). Two lanes, one shared RunState bounding the batch's blast
    #     radius (AC1-EDGE):
    #       - `needs_think` group -> FORCE a fan-out /think+/blueprint design pass.
    #         The decompose invocation IS the operator consent (Locked Decision 3),
    #         so the gate + attended-offer are overridden (mirrors the
    #         dispatch_conversational env-forcing); the caps still bound it. A spawn
    #         that does not fire leaves the child `idea` with its stub on disk.
    #       - unflagged group -> nothing. `needs_think` is the SOLE consent for a
    #         decompose-time /think: the born-with-why lane that used to
    #         run here spawned unconditionally on any autonomous decompose, because
    #         its OFFER branch needs presence == attended and an autonomous session
    #         always classifies `away`. The epic doc is a group child's design
    #         authority; scaffold_separate_plan + why_digest already carry it.
    #     Only UNLINKED children are candidates (a re-decompose never re-designs a
    #     child that already has a plan). Strictly non-fatal: never wedge decompose.
    fanout: list[dict] = []
    flagged_slugs = {g["slug"] for g in norm if g["needs_think"]}
    slug_by_id = {r["id"]: r["slug"] for r in results}
    created_ids = {r["id"] for r in results if r["action"] == "created"}
    spec_ids = [r["id"] for r in results]
    if spec_ids:
        try:
            from fno.provenance.spawn_think import (
                RunState,
                maybe_spawn_think,
                think_spawn_on_decompose_wave0,
            )

            # Reuse the shared post-mutation re-read from 3c (by_id).
            born_rs = RunState()
            # Force the gate + spawn (over the default-OFF / attended-offer) for the
            # flagged fan-out only; reuses the exact env seams dispatch_conversational
            # uses, so no new maybe_spawn branch.
            forced_env = {
                **os.environ,
                "FNO_THINK_SPAWN": "1",
                "FNO_THINK_SPAWN_ATTENDED": "spawn",
            }
            # x-3571 wave-2 lane: opt-in, default OFF. Wave 0 means "no
            # intra-epic blocker", which `compute_waves` already derives (and
            # already projects as `wave:`), so there is nothing new to compute -
            # only a second reason to spawn beside the `needs_think` flag.
            # Restricted to wave 0 deliberately: those children are genuinely
            # independent, which is the only case where handing design to a cold
            # worker beats one warm context writing several coherent siblings.
            wave0_ids: set[str] = set()
            if think_spawn_on_decompose_wave0(project_root=Path(epic_cwd_box[0])
                                              if epic_cwd_box[0] else None):
                from fno.plan._rollup import compute_waves

                wave_by_id, _ = compute_waves(epic_resolved_id, list(by_id.values()))
                wave0_ids = {cid for cid, w in wave_by_id.items() if w == 0}
            for cid in spec_ids:
                child = by_id.get(cid)
                if child is None or not _needs_design(child):
                    continue  # already designed; nothing for a /think to add
                if slug_by_id.get(cid) in flagged_slugs or cid in wave0_ids:
                    # chain_blueprint: the worker must continue /think -> /blueprint
                    # -> link, else the flagged child stays designless/idea forever
                    # (a bare /think never links plan_path). why_digest keeps it
                    # grounded when the transcript is unresolved; project_root scopes
                    # the /think doc to the CHILD's repo (cross-repo routing).
                    child_root = child.get("_resolved_cwd") or child.get("cwd")
                    res = maybe_spawn_think(
                        child, run_state=born_rs, env=forced_env,
                        quiet=json_mode(ctx), chain_blueprint=True,
                        why_digest=why_digest,
                        project_root=Path(child_root) if child_root else None,
                    )
                    # `owned` resolves the inline-fill handoff (Open Question 3)
                    # and is the field step 7 reads. Ownership is keyed on the
                    # OBSERVED spawn receipt, never on predicted wave
                    # membership: if the spawn did not fire, the child is still
                    # inline-fill's, so a spawn failure degrades to today's
                    # behavior instead of leaving an orphan nobody fills.
                    # That is also what makes double-writing impossible (AC9-CON)
                    # - exactly one lane can see `owned: true` for a child.
                    lane = "wave0" if cid in wave0_ids else "needs_think"
                    fanout.append({"id": cid, "decision": res.decision,
                                   "reason": res.reason, "lane": lane,
                                   "owned": res.decision == "spawned"})
                    if res.decision != "spawned" and not json_mode(ctx):
                        typer.echo(
                            f"fan-out /think for {cid} did not spawn "
                            f"({res.reason}); it stays yours to inline-fill "
                            f"(or run `/think {cid}` then `/blueprint`)",
                            err=True,
                        )
        except Exception as exc:  # noqa: BLE001 - additive; never wedge the decompose
            # Non-fatal by design (the graph mutation already committed), but
            # NOT silent: an empty `fanout` is indistinguishable from "no
            # children needed designing", so a crash here would read as a clean
            # no-op to both the operator and a --json consumer. Name it and say
            # which children fell back to inline-fill.
            _unowned = [c for c in spec_ids if not any(
                f["id"] == c and f.get("owned") for f in fanout
            )]
            if not json_mode(ctx):
                typer.echo(
                    f"warning: design fan-out failed ({exc}); "
                    f"{len(_unowned)} child(ren) stay yours to inline-fill"
                    + (f": {', '.join(_unowned)}" if _unowned else ""),
                    err=True,
                )

    # 4b. Report what happened (AC1-UI).
    if json_mode(ctx):
        typer.echo(json.dumps(
            {
                "epic": epic_resolved_id,
                "groups": results,
                "orphaned": orphan_ids,
                "downgrades": downgrades,
                "packaging": plans,
                "scaffolded": scaffolded,
                "fanout": fanout,
            },
            default=str,
        ))
    else:
        typer.echo(f"epic: {epic_resolved_id}")
        typer.echo(
            f"decomposed into {len(results)} group child node(s) "
            f"(packaging: {plans}):"
        )
        for r in results:
            waves = f" waves {r['waves']}" if r["waves"] else ""
            blk = f" blocked_by={r['blocked_by']}" if r["blocked_by"] else ""
            tier = " dep=contract" if r.get("dep") == "contract" else ""
            marker = r["slug"]
            # Adoption re-parents PRE-EXISTING nodes, so it must reach the human
            # receipt and not only `--json` - same reason the fan-out ownership
            # line below does. Empty stays silent, so a spec with no adopt key
            # prints byte-for-byte what it printed before.
            adopted = f" adopted={r['adopted']}" if r.get("adopted") else ""
            typer.echo(
                f"  {r['action']}: {r['id']} ({marker}){waves}{blk}{tier}{adopted}"
            )
        for f in scaffolded:
            typer.echo(f"  scaffolded plan: {f}")
        # Ownership must reach the HUMAN receipt, not just `--json`. The
        # blueprint session is told to skip children the fan-out owns
        # (epic-decomposition.md step 7), and step 6 invokes decompose without
        # `--json` - so a contract carried only in the JSON shape is a contract
        # the reader never sees, and AC9-CON's no-double-write property would
        # rest on a field the default invocation does not emit.
        _owned = [fo["id"] for fo in fanout if fo.get("owned")]
        for fo in fanout:
            if fo["decision"] == "spawned":
                typer.echo(f"  fan-out design pass dispatched: {fo['id']}")
        if _owned:
            typer.echo(
                f"  fan-out OWNS (do NOT inline-fill): {', '.join(_owned)}"
            )
        _unowned_attempts = [
            fo["id"] for fo in fanout if not fo.get("owned")
        ]
        if _unowned_attempts:
            typer.echo(
                f"  fan-out did NOT claim (inline-fill these): "
                f"{', '.join(_unowned_attempts)}"
            )
        if orphan_ids:
            typer.echo(
                f"warning: {len(orphan_ids)} group child node(s) no longer in the spec, "
                f"left in place: {', '.join(orphan_ids)}",
                err=True,
            )
        for msg in downgrades:
            typer.echo(f"warning: {msg}", err=True)

    # 4c. Name the epic children no group adopted (x-b9d7 US3). Emitted on BOTH
    #     report paths, not just the human one: a --json caller decomposing a
    #     populated epic needs this as much as an operator does, and stderr
    #     never pollutes the JSON on stdout.
    if unadopted_ids:
        typer.echo(
            f"warning: {len(unadopted_ids)} epic child(ren) adopted by no group, "
            f"left parented to the epic: {', '.join(unadopted_ids)}. "
            "Add them to a group's `adopt` list to package them into that PR.",
            err=True,
        )

    # 5. Record the group count N on the shared epic doc so it graduates only
    #    after all N group PRs ship (not after the first). The graph mutation
    #    above is the source of truth and is NEVER rolled back; decompose also
    #    never exits non-zero on a stamp problem, because that would break
    #    pipelines (e.g. /blueprint group) for a best-effort stamp. A genuine
    #    write failure (the doc exists but could not be written) is surfaced as
    #    a loud, actionable stderr warning so it is not silent; environment
    #    skips (absent doc/script - which also can't be stamped at ship, so no
    #    early graduation) stay quiet.
    base = base_box[0]
    expected_count = len(results)
    if base and expected_count >= 1:
        status, detail = _set_expected_count(base, expected_count)
        if status == "failed":
            typer.echo(
                f"warning: could not record expected_url_count={expected_count} on "
                f"{base}: {detail}. The shared doc will graduate after the FIRST "
                f"group ships unless you run: fno do plan set-expected --plan-path "
                f"{base} --count {expected_count}",
                err=True,
            )
        # status == "skipped": the doc or script is absent (an environment
        # condition that cannot cause early graduation - target can't stamp it
        # either). Proceed silently; the graph mutation already succeeded.

    # Repaint the epic and every child this decompose CREATED so a decomposed
    # epic's children carry correct blocked_by/parent mirrors from birth (US5).
    # Scoped to created children (not already-linked ones): an existing child's
    # hand-filled plan is left untouched here and its drift rides the sweep.
    _project_plans_from_graph([epic_resolved_id, *created_ids])


# -- intake --

def _intake_impl(
    plan_paths: Optional[List[str]] = None,
    from_list: Optional[str] = None,
    roadmap_id: Optional[str] = None,
    title: Optional[str] = None,
    priority: Optional[str] = None,
    deps: Optional[str] = None,
    points: Optional[int] = None,
    project: Optional[str] = None,
    force_new_roadmap: bool = False,
    batch: bool = False,
    dry_run: bool = False,
    claims: Optional[str] = None,
) -> None:
    """Implementation for the intake verb.

    Pulls an existing plan file into the backlog as a new node. Typer-parameter
    defaults are intentionally plain Python values here so the thin command
    wrapper can pass through already-parsed arguments. Kept as a separate
    `_intake_impl` (rather than inlined into `cmd_intake`) so the underlying
    `_intake.py` helpers can be exercised by tests without going through Typer.
    """
    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import (
        _prepare_intake, _build_intake_node,
        _validate_cli_deps,
    )

    # Reject removed --batch flag
    if batch:
        typer.echo(
            "Error: `--batch` was removed. Use multi-path intake instead:\n"
            "  fno backlog intake plans/a.md plans/b.md plans/c.md\n"
            "  fno backlog intake plans/folder/*.md  # shell glob",
            err=True,
        )
        raise typer.Exit(code=1)

    # Build args-like namespace for reuse of shared intake logic
    args = SimpleNamespace(
        roadmap_id=roadmap_id,
        title=title,
        priority=priority,
        deps=deps,
        points=points,
        force_new_roadmap=force_new_roadmap,
        dry_run=dry_run,
        from_list=from_list,
        plan_paths=plan_paths or [],
        project=project,
    )

    if project is not None and (not isinstance(project, str) or not project.strip()):
        typer.echo("Error: --project must be a non-empty string", err=True)
        raise typer.Exit(code=1)

    if args.priority and args.priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{args.priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    all_paths = _collect_intake_paths_typer(plan_paths or [], from_list)
    if not all_paths:
        if from_list:
            label = "stdin" if from_list == "-" else from_list
            typer.echo(
                f"Error: --from {label} produced 0 usable paths "
                "(blank lines and '#' comments are skipped).",
                err=True,
            )
        else:
            typer.echo(
                "Error: no plan paths provided. Pass one or more positional "
                "arguments, or use --from FILE (or --from -).",
                err=True,
            )
        raise typer.Exit(code=1)

    if len(all_paths) > 1:
        _do_intake_multi(
            args, all_paths,
            roadmap_id=roadmap_id, dry_run=dry_run,
        )
        return

    # Single-path flow
    plan_path = all_paths[0]

    cli_deps: list[str] = (
        [d.strip() for d in deps.split(",") if d.strip()] if deps else []
    )

    # Creation path: the same external-backend refusal every birth path
    # carries (an intake mints new nodes).
    _refuse_create_on_external_backend()

    entries = read_graph(_graph_path())

    if roadmap_id and not force_new_roadmap:
        has_roadmap = any(e.get("roadmap_id") == roadmap_id for e in entries)
        if not has_roadmap:
            typer.echo(
                f"unknown roadmap_id: {roadmap_id} "
                "(use /megawalk vision.md to create a roadmap first, "
                "pass --force-new-roadmap, or omit --roadmap-id to intake to the backlog)",
                err=True,
            )
            raise typer.Exit(code=2)

    _validate_cli_deps(cli_deps, entries)

    try:
        prep = _prepare_intake(
            plan_path, entries,
            roadmap_id=roadmap_id, cli_title=title,
            cli_priority=priority, cli_deps=cli_deps, cli_points=points,
            cli_project=project,
            cli_claim=claims,
        )
    except ValueError as e:
        typer.echo(f"Error: {e}", err=True)
        raise typer.Exit(code=1)
    if prep["status"] == "already":
        typer.echo(f'already intaked: {prep["id"]}')
        return

    spec = prep["node_spec"]

    if dry_run:
        verb = "claim" if prep["status"] == "claim" else "intake"
        typer.echo(f"{verb.capitalize()} preview (dry-run, no changes):")
        target = f' (claims {prep["id"]})' if prep["status"] == "claim" else ""
        typer.echo(f'  would {verb}: "{spec["title"]}"  (plan: {plan_path}){target}')
        if spec["deps"]:
            typer.echo(f'  blocked_by: {", ".join(spec["deps"])}')
        return

    # Emit "not in ledger" warning before mutating
    from fno.graph._intake import _lookup_ledger_entry
    if _lookup_ledger_entry(plan_path) is None:
        typer.echo("plan_path not in ledger.json - intake will continue anyway", err=True)

    if prep["status"] == "claim":
        claim_id = prep["id"]
        claim_source = prep["claim_source"]

        def claim_mutator(es):
            from fno.graph._intake import (
                DEFAULT_NODE_TYPE,
                _find_node,
                _read_plan_frontmatter,
                _would_exceed_epic_depth,
                normalize_type,
                resolve_node_project_and_cwd,
            )

            # Intake has TWO lanes; the create lane reads `type` doc->graph in
            # _build_intake_node, so this one must too or the flow is a guard on
            # one of N paths. Same rule as priority below: the doc only speaks
            # when it declares a real, non-default value.
            frontmatter = _read_plan_frontmatter(plan_path) or {}
            claimed_type = normalize_type(frontmatter.get("type"))
            from fno.graph._constants import normalize_difficulty
            raw_difficulty = frontmatter.get("difficulty")
            if raw_difficulty is None:
                raw_difficulty = frontmatter.get("model_tier")
            for entry in es:
                if entry.get("id") != claim_id:
                    continue
                entry["plan_path"] = plan_path
                entry["title"] = spec["title"]
                if raw_difficulty is not None:
                    try:
                        revised_difficulty = normalize_difficulty(raw_difficulty)
                    except ValueError as exc:
                        typer.echo(f"warning: {exc}; difficulty left unchanged", err=True)
                    else:
                        entry["difficulty"] = revised_difficulty
                        entry.setdefault("difficulty_history", []).append(
                            {
                                "value": revised_difficulty,
                                "source": "blueprint",
                                "ts": datetime.now(timezone.utc).isoformat(),
                            }
                        )
                if (
                    claimed_type != DEFAULT_NODE_TYPE
                    and entry.get("type") != claimed_type
                ):
                    # `add` and `update` both refuse a write that would make a
                    # third epic level; a doc-frontmatter lane that skips the cap
                    # would be the decorative guard this whole change is about.
                    # It SKIPS rather than refuses, unlike those two: there the
                    # operator typed `--type` and deserves a hard error, here the
                    # doc is advisory and the claim itself is still valid.
                    parent_node = (
                        _find_node(es, entry["parent"]) if entry.get("parent") else None
                    )
                    if (
                        claimed_type == "epic"
                        and parent_node is not None
                        and _would_exceed_epic_depth(
                            es, {**entry, "type": "epic"}, parent_node
                        )
                    ):
                        typer.echo(
                            f"warning: plan declares type: epic but {claim_id} sits "
                            f"under {parent_node['id']}; promoting it would exceed "
                            f"the epic-nesting cap - type left as "
                            f"{entry.get('type')!r}",
                            err=True,
                        )
                    else:
                        entry["type"] = claimed_type
                if spec["deps"]:
                    merged = list(
                        dict.fromkeys([*entry.get("blocked_by", []), *spec["deps"]])
                    )
                    entry["blocked_by"] = merged
                # Only override priority if the plan supplied a non-default one.
                if spec.get("priority") and spec["priority"] != "p2":
                    entry["priority"] = spec["priority"]
                if spec.get("points") is not None:
                    entry["points"] = spec["points"]
                # Backfill project/cwd when the node was created via
                # `fno backlog new` (no plan path -> no auto-scope) and is
                # now being claimed by a plan that lives in a project repo.
                # Only fills nulls; never overwrites existing values.
                if entry.get("project") is None or entry.get("cwd") is None:
                    resolved_project, resolved_cwd, _ = resolve_node_project_and_cwd(
                        plan_path, project, es,
                    )
                    if entry.get("project") is None and resolved_project:
                        entry["project"] = resolved_project
                    if entry.get("cwd") is None and resolved_cwd:
                        entry["cwd"] = resolved_cwd
                # Promote idea -> ready by clearing any stale claimed_at.
                # status is recomputed by recompute_statuses on the next read.
                entry["claimed_at"] = None
                break
            return es

        locked_mutate_graph(_graph_path(), claim_mutator)
        typer.echo(
            f'claimed {claim_id} via {claim_source}: "{spec["title"]}"'
        )
        # Mirror nav fields onto the just-linked plan of the CLAIMED node too -
        # this branch returns early, so the append-path projection never runs.
        # Routed through the converger so parent_slug is injected consistently.
        try:
            from fno.plan._project import project_graph_nodes

            project_graph_nodes(read_graph(_graph_path()), [claim_id])
        except Exception as e:  # noqa: BLE001 - additive; never wedge the claim
            sys.stderr.write(f"warning: post-claim plan projection failed: {e}\n")
        return

    new_id_holder: list[Optional[str]] = [None]

    def mutator(es):
        node = _build_intake_node(spec, es)
        new_id_holder[0] = node["id"]
        es.append(node)
        return es

    locked_mutate_graph(_graph_path(), mutator)
    destination = roadmap_id if roadmap_id else "backlog"
    typer.echo(f'intake {new_id_holder[0]} -> {destination}: "{spec["title"]}"')

    try:
        from fno.graph._intake import _warn_unknown_project, _find_node
        post_entries = read_graph(_graph_path())
        node = _find_node(post_entries, new_id_holder[0] or "")
        landed_project = node.get("project") if node else None
        _warn_unknown_project(landed_project)
    except Exception as e:
        # The mutation already committed; a stray failure in the warning
        # path must not surface as if the intake itself failed.
        sys.stderr.write(f"warning: post-intake project check failed: {e}\n")

    # Filing-time dedup net (plan x-6ac7): warn if the just-born node resembles
    # an existing one across all live states. Own try/except so a dedup failure
    # is reported as itself, not conflated with the project check above.
    try:
        from fno.graph._intake import _find_node, _warn_similar_nodes
        post_entries = read_graph(_graph_path())
        node = _find_node(post_entries, new_id_holder[0] or "")
        if node is not None:
            _warn_similar_nodes(node, post_entries, intake_hint=True)
    except Exception as e:  # noqa: BLE001 - dedup never breaks the intake
        _safe_stderr_warn(f"warning: post-intake dedup check skipped: {e}\n")

    # Mirror the graph-authoritative navigation fields onto the plan doc the
    # node just linked. Non-fatal: a missing/unreadable plan never fails intake.
    # Routed through the converger so parent_slug is injected consistently.
    if new_id_holder[0]:
        try:
            from fno.plan._project import project_graph_nodes

            project_graph_nodes(read_graph(_graph_path()), [new_id_holder[0]])
        except Exception as e:  # noqa: BLE001 - additive; never wedge the intake
            sys.stderr.write(f"warning: post-intake plan projection failed: {e}\n")

    # Born-with-why (v2 A1): route the intaked node through the shared birth hook
    # for uniformity across birth paths. Independent of the project-warning block
    # above so a warn failure never drops the dispatch. Most intake nodes are
    # built by _build_intake_node (no ambient provenance stamp) and self-skip
    # with 'no-origin'; this keeps every birth path consistent. Non-fatal +
    # opt-in (gate-OFF default => complete no-op).
    if new_id_holder[0]:
        try:
            from fno.graph._intake import _find_node
            from fno.provenance.spawn_think import on_node_born

            born_node = _find_node(read_graph(_graph_path()), new_id_holder[0])
            if born_node is not None:
                # Already the persisted, slugged node -> skip the re-read.
                on_node_born(born_node, persisted=True)
        except Exception:  # noqa: BLE001 - additive; never wedge the intake
            pass


@cli.command(
    "intake",
    hidden=True,
    epilog="Paired verb: `fno backlog remove <id>` deletes the node this creates.",
)
def cmd_intake(
    plan_paths: Optional[List[str]] = typer.Argument(default=None, help="Plan paths"),
    from_list: Optional[str] = typer.Option(None, "--from", help="Read paths from FILE or '-' for stdin"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id", help="Target roadmap ID"),
    title: Optional[str] = typer.Option(None, "--title", "-t", help="Override derived title"),
    priority: Optional[str] = typer.Option(None, "--priority", "-p", help="p0|p1|p2|p3"),
    deps: Optional[str] = typer.Option(None, help="Comma-separated ab-IDs"),
    points: Optional[int] = typer.Option(None, help="Story point estimate"),
    project: Optional[str] = typer.Option(None, "--project", help="Override the project field (beats frontmatter and cwd inference)"),
    force_new_roadmap: bool = typer.Option(False, "--force-new-roadmap"),
    batch: bool = typer.Option(False, "--batch", hidden=True),
    dry_run: bool = typer.Option(False, "--dry-run", "-N"),
    claims: Optional[str] = typer.Option(
        None, "--claims",
        help=(
            "ab-XXXXXXXX of an existing idea-state node this plan implements. "
            "Updates the node in place rather than creating a new one. "
            "Beats any frontmatter 'claims:' value."
        ),
    ),
) -> None:
    """Pull in an existing plan file as a backlog node."""
    _refuse_create_on_external_backend()
    _intake_impl(
        plan_paths=plan_paths,
        from_list=from_list,
        roadmap_id=roadmap_id,
        title=title,
        priority=priority,
        deps=deps,
        points=points,
        project=project,
        force_new_roadmap=force_new_roadmap,
        batch=batch,
        dry_run=dry_run,
        claims=claims,
    )


# -- update --

@cli.command("note", hidden=True)
def cmd_note(
    task_id: str = typer.Argument(..., help="Node id to append a progress note to."),
    text: str = typer.Argument(..., help="Progress note text (one line)."),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the appended note as JSON."
    ),
) -> None:
    """Append a timestamped progress note to a backlog node (append-only).

    Distinct from ``update --details`` (which REPLACES the rationale) and the
    single ``completion_note``: ``note`` accumulates a list of ``{ts, text}``
    entries. The status-fanout backlog-progress adapter stamps one per
    ``task_done``/``run_summary`` (x-2057); it is also hand-runnable.
    """
    from fno.graph.store import append_progress_note

    text = text.strip()
    if not text:
        typer.echo("Error: note text is empty", err=True)
        raise typer.Exit(code=1)

    note = {"ts": datetime.now(timezone.utc).isoformat(), "text": text}
    found, _ = append_progress_note(_graph_path(), task_id, note)
    if not found:
        typer.echo(f"Error: no node resolves to '{task_id}'", err=True)
        raise typer.Exit(code=1)
    if json_output:
        typer.echo(json.dumps({"id": task_id, "note": note}, separators=(",", ":")))
    else:
        typer.echo(f"noted {task_id}: {text}")


@cli.command("update")
def cmd_update(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    locked_by: Optional[str] = typer.Option(None, "--locked-by", help="Lock owner id ('null' to release)"),
    locked_by_harness: Optional[str] = typer.Option(None, "--locked-by-harness", help="Holder's harness/provider (claude|codex|gemini). 'null' clears."),
    locked_by_harness_session: Optional[str] = typer.Option(None, "--locked-by-harness-session", help="Holder's harness session UUID. 'null' clears."),
    has_brief: Optional[str] = typer.Option(None, "--has-brief", help="Set has_brief flag"),
    plan_path: Optional[str] = typer.Option(
        None, "--plan-path", help="Plan directory path. 'null' clears."
    ),
    pr_number: Optional[str] = typer.Option(
        None, "--pr-number", help="PR number. 'null' clears."
    ),
    pr_url: Optional[str] = typer.Option(None, "--pr-url", help="PR URL. 'null' clears."),
    priority: Optional[str] = typer.Option(None, "--priority", "-p", help="New priority"),
    title: Optional[str] = typer.Option(None, "--title", "-t", help="Update display title"),
    details: Optional[str] = typer.Option(
        None,
        "--details",
        "--description",
        "-d",
        help="Update free-form details/rationale (stored in `details`). Pass 'null' to clear.",
    ),
    domain: Optional[str] = typer.Option(None, "--domain", help="Update domain (e.g. code)"),
    size: Optional[str] = typer.Option(None, "--size", help="Update size estimate: S|M|L"),
    difficulty: Optional[str] = typer.Option(
        None,
        "--difficulty",
        help="Intrinsic work difficulty: low|medium|high. Not a model or capacity hint.",
    ),
    model: Optional[str] = typer.Option(
        None,
        "--model",
        help="Pin the model dispatchers launch this node's worker on (x-571f), e.g. fable|opus|sonnet or a full provider-model id. Single non-whitespace token. Pass 'null' to clear (revert to provider default).",
    ),
    model_tier: Optional[str] = typer.Option(
        None,
        "--model-tier",
        help="Pin a minimum quality tier (high|medium|low) resolved to the cheapest reachable model at dispatch from the benchmark snapshot. Outranked by an exact --model. Pass 'null' to clear.",
    ),
    batch: Optional[str] = typer.Option(
        None,
        "--batch",
        help="Set the batch id this node is a member of (marks node.batch, batch-lane Wave 2). Pass 'null' to clear (requeue for individual ship on abandon).",
    ),
    orphan_ok: Optional[str] = typer.Option(
        None,
        "--orphan-ok",
        help="Record why this node deliberately serves no mission, exempting it from the orphan metric, the board flag, and the ordering tiebreaker. Pass 'null' to clear.",
    ),
    dispatch_verb: Optional[str] = typer.Option(
        None,
        "--dispatch-verb",
        help="Verb a dispatcher launches this node with (US3), e.g. /think. Validated against config.dispatch.allowed_verbs at dispatch, not here. Pass 'null' to clear (revert to the /target --no-merge default).",
    ),
    dispatch_brief: Optional[str] = typer.Option(
        None,
        "--dispatch-brief",
        help="Free-text brief carried to the worker via TARGET_BRIEF env at cold-start (US3), never the command line. Capped at 8 KB at dispatch. Pass 'null' to clear.",
    ),
    type_: Optional[str] = typer.Option(None, "--type", help="Update node type (feature|epic|bug)"),
    public: Optional[bool] = typer.Option(None, "--public/--no-public", help="Mark node for the public roadmap (fno backlog roadmap)"),
    project: Optional[str] = typer.Option(None, "--project", help="Reproject this node (use for migrating wrong-scope nodes)"),
    cwd: Optional[str] = typer.Option(None, "--cwd", "-c", help="Update cwd (pair with --project for migration)"),
    source_node: Optional[str] = typer.Option(
        None,
        "--source-node",
        help=(
            "Set the origin node this node came out of (id, slug, or bare hex). "
            "Pass 'null' to clear. Refuses a self-reference or an id that does not resolve."
        ),
    ),
    related: Optional[List[str]] = typer.Option(
        None,
        "--related",
        help=(
            "Replace the related list (asserted, symmetric, non-blocking). Repeat or "
            "comma-separate. Pass 'null' to clear. Refuses a self-reference or an id "
            "that does not resolve."
        ),
    ),
    blocked_by: Optional[List[str]] = typer.Option(None, "--blocked-by", help="Replace blocked_by list"),
    add_blocker: Optional[List[str]] = typer.Option(None, "--add-blocker", help="Append blocker IDs"),
    remove_blocker: Optional[List[str]] = typer.Option(None, "--remove-blocker", help="Remove blocker IDs"),
    acknowledge_collisions: Optional[str] = typer.Option(
        None,
        "--acknowledge-collisions",
        help="Comma-separated ab-IDs of collisions deliberately accepted. Pass '__skipped_check__' to record a skipped check.",
    ),
    parent: Optional[str] = typer.Option(
        None,
        "--parent",
        help="Set parent node ID. Pass 'null' to clear (de-orphan to top-level). Validates target exists and rejects cycles.",
    ),
    completion_note: Optional[str] = typer.Option(
        None,
        "--completion-note",
        help="Append free-text completion note. Multiple calls append with ' + ' separator. Whitespace-only is a no-op. Pass 'null' to clear.",
    ),
    add_pr: Optional[int] = typer.Option(
        None,
        "--add-pr",
        help="Append a follow-up PR number to additional_prs. Pair with --add-pr-url / --add-pr-note for context. Re-adding an existing number updates that entry in place.",
    ),
    add_pr_url: Optional[str] = typer.Option(
        None,
        "--add-pr-url",
        help="URL for the --add-pr entry (optional).",
    ),
    add_pr_note: Optional[str] = typer.Option(
        None,
        "--add-pr-note",
        help="One-line note for the --add-pr entry (optional).",
    ),
    remove_pr: Optional[int] = typer.Option(
        None,
        "--remove-pr",
        help="Remove a PR entry from additional_prs by number. No-op if absent. The primary pr_number is unaffected (use --pr-number to change that).",
    ),
    caused_by: Optional[str] = typer.Option(
        None,
        "--caused-by",
        help="Node id this node was created to address (causal link, W4). Pass 'null' to clear.",
    ),
    fixes_pr: Optional[int] = typer.Option(
        None,
        "--fixes-pr",
        help="PR number this node fixes (causal link, W4). Pass 0 to clear.",
    ),
    reverted: Optional[bool] = typer.Option(
        None,
        "--reverted/--no-reverted",
        help="Mark this node's ship as reverted (manual fallback for reconcile's best-effort revert detection).",
    ),
    tag: Optional[List[str]] = typer.Option(
        None, "--tag", hidden=True, help="Add a tag (repeatable, idempotent, lowercase-kebab)."
    ),
    untag: Optional[List[str]] = typer.Option(
        None, "--untag", hidden=True, help="Remove a tag (repeatable, no-op if absent)."
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help=(
            "Escape the one-plan-one-node refusal on --plan-path: bind this node to "
            "a plan another node already owns (a deliberate repoint). The other "
            "holder is still named on stderr."
        ),
    ),
) -> None:
    from fno.graph._constants import (
        PRIORITY_ORDER,
        has_node_id_prefix,
        normalize_difficulty,
        normalize_tag,
    )
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import (
        _parse_blocker_list,
        _validate_blocker_ids,
        _find_node,
        _would_create_cycle,
        _would_exceed_epic_depth,
    )
    from fno.graph._constants import EPIC_NEST_MAX_DEPTH

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    if priority is not None and priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    if project is not None and (not isinstance(project, str) or not project.strip()):
        typer.echo("Error: --project must be a non-empty string", err=True)
        raise typer.Exit(code=1)

    # Validate size/type the same way priority is validated above, so update
    # can't store garbage (e.g. `--size foo`). 'null' clears size.
    if size is not None and size.lower() != "null" and size.upper() not in {"S", "M", "L"}:
        typer.echo(f"Error: invalid size '{size}'. Must be one of: S, M, L", err=True)
        raise typer.Exit(code=1)
    # Model pin (x-571f): a validated-shape pass-through, not an allowlist. The
    # value must be a single shell-safe token so it survives unquoted use in the
    # dispatchers (dispatch-node.sh $model_arg, the loop-driver MODEL_FLAG
    # word-split) without word-splitting OR globbing. The charset [A-Za-z0-9._:/-]
    # covers every real model id (fable, claude-opus-4-8, openai/gpt-4,
    # us.anthropic.claude-...) while forbidding whitespace and shell/glob
    # metacharacters (* ? [ ] etc.); the CLI (not fno) resolves the alias.
    # 'null' clears.
    if model is not None and model.lower() != "null":
        import re  # module-level `re` is function-local elsewhere; scope it here

        if not re.fullmatch(r"[A-Za-z0-9._:/-]{1,64}", model):
            typer.echo(
                "Error: --model must be a single token of [A-Za-z0-9._:/-], at most 64 chars "
                "(e.g. fable|opus|sonnet or a full provider-model id); no whitespace or "
                "shell/glob metacharacters",
                err=True,
            )
            raise typer.Exit(code=1)
    from fno.graph._intake import VALID_NODE_TYPES

    if type_ is not None and type_ not in VALID_NODE_TYPES:
        typer.echo(
            f"Error: invalid type '{type_}'. Must be one of: "
            f"{', '.join(sorted(VALID_NODE_TYPES))}",
            err=True,
        )
        raise typer.Exit(code=1)

    # Normalize + validate tags OUTSIDE the lock so a malformed tag refuses
    # before any mutation (the node is unchanged on a bad --tag).
    add_tags: list[str] = []
    remove_tags: list[str] = []
    try:
        add_tags = [normalize_tag(t) for t in (tag or [])]
        remove_tags = [normalize_tag(t) for t in (untag or [])]
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1)
    has_tag_edit = bool(add_tags or remove_tags)

    # Derive cwd from the work-map when --project is explicit but --cwd was
    # not given. Do this OUTSIDE the mutator so settings reads never happen
    # under the graph lock.
    derived_cwd_for_update: Optional[str] = None
    if project is not None and cwd is None:
        from fno.graph._intake import project_root_from_settings
        workmap_root = project_root_from_settings(project)
        if workmap_root is not None:
            derived_cwd_for_update = workmap_root
        else:
            typer.echo(
                f"warning: project '{project}' not in any settings.yaml work-map; cwd left unchanged",
                err=True,
            )

    if cwd is not None:
        if not isinstance(cwd, str) or not cwd.strip():
            typer.echo("Error: --cwd must be a non-empty string", err=True)
            raise typer.Exit(code=1)
        cwd = os.path.abspath(os.path.expanduser(cwd))

    replace_blockers = _parse_blocker_list(blocked_by)
    add_blockers = _parse_blocker_list(add_blocker)
    remove_blockers = _parse_blocker_list(remove_blocker)
    has_blocker_edit = bool(blocked_by is not None or add_blockers or remove_blockers)

    if blocked_by is not None and (add_blockers or remove_blockers):
        typer.echo(
            "Error: --blocked-by is mutually exclusive with --add-blocker/--remove-blocker",
            err=True,
        )
        raise typer.Exit(code=2)

    if add_pr is None and (add_pr_url is not None or add_pr_note is not None):
        typer.echo(
            "Error: --add-pr-url and --add-pr-note require --add-pr",
            err=True,
        )
        raise typer.Exit(code=2)

    # pr_number and pr_url travel together: a url-less pr_number names no repo,
    # and PR numbers collide across repos, so any consumer matching on the bare
    # number can attribute a foreign PR. Resolve here (subprocess I/O stays out
    # of the graph lock) and fail closed when the repo will not resolve.
    from fno.graph._reconcile import (
        pr_number_from_url,
        pr_url_for_repo,
        repo_slug_from_url,
    )

    _pre_lock_node: list = []

    def _node_before_update() -> dict:
        """The node as it stands now, for cwd + current-PR reads. Cached."""
        if not _pre_lock_node:
            from fno.graph._intake import _find_node as _find
            from fno.graph.load import load_graph

            _pre_lock_node.append(_find(load_graph(_graph_path()), task_id) or {})
        return _pre_lock_node[0]

    def _refuse(msg: str) -> None:
        typer.echo(f"Error: {msg}", err=True)
        raise typer.Exit(code=2)

    def _resolve_or_refuse(number: int, label: str) -> str:
        slug_cwd = derived_cwd_for_update or cwd or _node_before_update().get("cwd")
        url = pr_url_for_repo(number, slug_cwd)
        if url is None:
            _refuse(
                f"cannot resolve the repo for PR #{number} - refusing to stamp an "
                "unattributable pr_number. Fix with either `gh auth login` or "
                f"`{label} https://github.com/<owner>/<repo>/pull/{number}`."
            )
        typer.echo(f"note: derived {label} {url}", err=True)
        return url  # type: ignore[return-value]

    def _check_url_shape(value: str, label: str, expect_pr: Optional[int] = None) -> None:
        if repo_slug_from_url(value) is None:
            _refuse(
                f"{label} {value!r} is not a GitHub PR url "
                "(expected https://github.com/<owner>/<repo>/pull/<n>)"
            )
        named = pr_number_from_url(value)
        if expect_pr is not None and named != expect_pr:
            _refuse(
                f"{label} names PR #{named}, not #{expect_pr} - a row pointing at "
                "two different PRs matches neither."
            )

    clearing_number = pr_number is not None and pr_number.lower() == "null"
    clearing_url = pr_url is not None and pr_url.lower() == "null"

    # Shape-check any url the caller supplies, on every path: an unparseable
    # url carries no repo slug, so it is url-less in every way that matters.
    if pr_url is not None and not clearing_url:
        # With no --pr-number the node keeps the one it already has, so that is
        # what the url must name - otherwise a url-only update re-creates the
        # two-different-PRs row the paired path refuses.
        expect: Optional[int]
        if pr_number is not None and pr_number.strip().isdigit():
            expect = int(pr_number)
        elif pr_number is None:
            current = _node_before_update().get("pr_number")
            expect = current if isinstance(current, int) else None
        else:
            expect = None
        _check_url_shape(pr_url, "--pr-url", expect)
    if add_pr_url is not None:
        _check_url_shape(add_pr_url, "--add-pr-url", add_pr)

    derived_pr_url: Optional[str] = None
    if pr_number is not None and not clearing_number:
        if not pr_number.strip().isdigit():
            _refuse(f"--pr-number {pr_number!r} is not a number (or 'null')")
        if clearing_url:
            _refuse(
                "--pr-url null cannot accompany --pr-number: that writes a "
                "url-less pr_number. Clear both, or supply a url."
            )
        if pr_url is None:
            derived_pr_url = _resolve_or_refuse(int(pr_number), "--pr-url")
    elif clearing_url and not clearing_number and _node_before_update().get("pr_number"):
        # Clearing the url alone strands the pr_number the node already carries.
        _refuse(
            "--pr-url null would leave this node's pr_number "
            f"({_node_before_update().get('pr_number')}) unattributable. "
            "Pass --pr-number null too, or supply a replacement url."
        )

    # Symmetric to derived_pr_url: a url-only update derives pr_number from the
    # url. repo_slug_from_url and pr_number_from_url share _PR_URL_RE, so any
    # url that passed _check_url_shape above carries a number. Without this,
    # reconcile never sees the PR (node_pr_refs gates on isinstance(pr_number,
    # int)) - a node linked by --pr-url alone stays invisible to merge
    # detection, the x-9ab2 shape.
    derived_pr_number: Optional[int] = None
    if pr_url is not None and not clearing_url and pr_number is None:
        derived_pr_number = pr_number_from_url(pr_url)

    # additional_prs entries are read by the same repo-scoped matcher as the
    # primary field, so a bare --add-pr is unattributable for the same reason.
    derived_add_pr_url: Optional[str] = None
    if add_pr is not None and add_pr_url is None:
        derived_add_pr_url = _resolve_or_refuse(int(add_pr), "--add-pr-url")

    projected_node: list = [None]
    reparent_old_parent: list = [None]
    ship_stamp_node: list = [None]
    blueprint_stamp_node: list = [None]

    # Size flows doc->graph when a plan is (re)linked and the node has no size
    # yet (Wave 2.2). Read the linked plan's frontmatter size best-effort,
    # outside the lock; an explicit --size still wins (applied later in-mutator).
    linked_size: Optional[str] = None
    if plan_path is not None:
        try:
            from fno.graph._intake import normalize_size, repo_root
            from fno.plan._stamp import read_plan_file

            pp = Path(plan_path)
            if not pp.is_absolute():
                pp = Path(repo_root()) / pp
            _, fm, _ = read_plan_file(pp)
            linked_size = normalize_size(fm.get("size"))
        except Exception:
            linked_size = None

    def mutator(entries):
        node = _find_node(entries, task_id)
        if node is None:
            typer.echo(f"Error: graph node {task_id} not found", err=True)
            raise typer.Exit(code=1)
        projected_node[0] = node

        if related is not None:
            from fno.graph.store import set_related

            tokens = _parse_blocker_list(related)
            desired = (
                []
                if tokens == ["null"]
                else [
                    _resolve_asserted_id(
                        t, entries, flag="--related", self_id=node["id"]
                    )
                    for t in tokens
                ]
            )
            set_related(entries, node["id"], desired)

        if source_node is not None:
            node["source_node_id"] = (
                None
                if source_node == "null"
                else _resolve_asserted_id(
                    source_node, entries, flag="--source-node", self_id=node["id"]
                )
            )

        if has_blocker_edit:
            if blocked_by is not None:
                desired = list(dict.fromkeys(replace_blockers))
                _validate_blocker_ids(desired, entries, task_id)
                node["blocked_by"] = desired
            else:
                current = list(node.get("blocked_by", []))
                _validate_blocker_ids(add_blockers, entries, task_id)
                for b in add_blockers:
                    if b not in current:
                        current.append(b)
                current = [b for b in current if b not in remove_blockers]
                node["blocked_by"] = current

        if locked_by is not None:
            session = locked_by if locked_by != "null" else None
            # locked_by is canonical; session_id mirror is re-synced at serialize
            # by _normalize_lock_fields. Clearing the lock also clears the US6
            # harness stamp so an unclaim never leaves a stale holder identity.
            node["locked_by"] = session
            node["claimed_at"] = datetime.now(timezone.utc).isoformat() if session else None
            if session is None:
                node["locked_by_harness"] = None
                node["locked_by_harness_session"] = None
        # Harness stamp (US6): the holder's provider + harness-session UUID,
        # settable alongside the claim. 'null' clears; an explicit unclaim above
        # already cleared both.
        if locked_by_harness is not None:
            node["locked_by_harness"] = None if locked_by_harness == "null" else locked_by_harness
        if locked_by_harness_session is not None:
            node["locked_by_harness_session"] = (
                None if locked_by_harness_session == "null" else locked_by_harness_session
            )
        if has_brief is not None:
            node["has_brief"] = has_brief.lower() == "true"
        if plan_path is not None:
            # 'null' clears. Storing the literal string binds the node to a plan
            # file named "null", which reads as bound to every gate that only
            # checks presence - worse than unbound, and unfixable on a
            # hand-edit-forbidden graph.
            if plan_path.lower() == "null":
                node["plan_path"] = None
            else:
                # plan == PR == node (x-04b9): a plan file is the delivery unit
                # of exactly one node. Refuse to arm a second node against a plan
                # another node already owns; the ambiguous state is what armed
                # two concurrent dispatches against one sequential-mode plan on
                # 2026-07-28. Routed through the shared plan_path_owner_conflict
                # so every write site (this one + intake's lanes) checks one way;
                # the decompose repoint is scoped to a slug it owns and never
                # lands here. --force escapes a deliberate repoint and still
                # names the other holder (AC2).
                from fno.graph.store import plan_path_owner_conflict

                owner = plan_path_owner_conflict(entries, node["id"], plan_path)
                if owner is not None and not force:
                    typer.echo(
                        f"error: plan {plan_path} is already the delivery unit of {owner}\n"
                        f"  a plan is one PR is one node; binding it to {node['id']} would arm both\n"
                        f"  to record that {node['id']} ships inside that PR: "
                        f"fno backlog decompose ... \"adopt\": [\"{node['id']}\"]\n"
                        f"  to repoint deliberately: --force",
                        err=True,
                    )
                    raise typer.Exit(code=2)
                if owner is not None:
                    typer.echo(
                        f"note: plan {plan_path} is also held by {owner}; "
                        f"binding {node['id']} anyway (--force). Both will "
                        f"dispatch and cost independently.",
                        err=True,
                    )
                _plan_path_before = node.get("plan_path")
                node["plan_path"] = plan_path
                # Blueprint provenance fires once when a plan is first bound to
                # the node. plan_path set is blueprint's END (the plan blueprint
                # produced is now linked); there is no clean blueprint start
                # (think has no writer), so the row carries ended_at only and the
                # roster renders it 'end only'. Choke point, not prose: every
                # blueprinted node passes through `backlog update --plan-path`.
                if plan_path and not _plan_path_before:
                    blueprint_stamp_node[0] = node["id"]
                if linked_size and not node.get("size"):
                    node["size"] = linked_size
        _pr_number_before = node.get("pr_number")
        if pr_number is not None:
            # 'null' clears, like every other nullable scalar here. Without it a
            # node linked to the wrong PR could not be unlinked at all, and the
            # graph is hand-edit-forbidden - so a mislink would ride to a merge
            # and close a node that shipped nothing.
            node["pr_number"] = None if pr_number.lower() == "null" else int(pr_number)
        elif derived_pr_number is not None:
            node["pr_number"] = derived_pr_number
        # Ship provenance fires once on the unset->set transition. Every shipped
        # node passes through this link regardless of which worker or skill
        # opened the PR, so this is the choke point that records the implementer
        # (not the merger). Detected inside the lock so two racing linkers see
        # one transition; the stamp itself runs after the lock releases, because
        # append_session_record takes its own lock and calling it here would
        # deadlock. Best-effort + idempotent, never fails the update.
        if isinstance(node.get("pr_number"), int) and not isinstance(
            _pr_number_before, int
        ):
            ship_stamp_node[0] = node["id"]
        if pr_url is not None:
            node["pr_url"] = None if pr_url.lower() == "null" else pr_url
        elif derived_pr_url is not None:
            node["pr_url"] = derived_pr_url
        if batch is not None:
            # 'null' clears the mark (requeue as individual ship on abandon); any
            # other value records the batch id this node is a member of.
            node["batch"] = None if batch.lower() == "null" else batch
        if orphan_ok is not None:
            # The reason IS the opt-out: an empty string would read as unset and
            # silently leave the node counted, so refuse it rather than no-op.
            if orphan_ok.lower() == "null":
                node["orphan_ok"] = None
            elif not orphan_ok.strip():
                typer.echo(
                    "Error: --orphan-ok needs a reason (or 'null' to clear)", err=True
                )
                raise typer.Exit(code=2)
            else:
                node["orphan_ok"] = orphan_ok
        # Dispatch overrides (US3). Stored permissively; the resolver is the trust
        # boundary (allowlist + 8 KB cap at dispatch time, not write time).
        if dispatch_verb is not None:
            node["dispatch_verb"] = None if dispatch_verb.lower() == "null" else dispatch_verb
        if dispatch_brief is not None:
            node["dispatch_brief"] = None if dispatch_brief.lower() == "null" else dispatch_brief
        if priority is not None:
            node["priority"] = priority
        if project is not None:
            node["project"] = project
        if cwd is not None:
            node["cwd"] = cwd
        elif derived_cwd_for_update is not None:
            node["cwd"] = derived_cwd_for_update
        if title is not None:
            new_title = title.strip()
            if not new_title:
                typer.echo("Error: --title cannot be empty or whitespace-only", err=True)
                raise typer.Exit(code=1)
            node["title"] = new_title
        if details is not None:
            node["details"] = None if details.lower() == "null" else details
        if domain is not None:
            node["domain"] = domain
        if size is not None:
            node["size"] = size.upper() if size.lower() != "null" else None
        if difficulty is not None:
            try:
                node["difficulty"] = normalize_difficulty(
                    None if difficulty.lower() == "null" else difficulty
                )
            except ValueError as exc:
                typer.echo(f"fno backlog update: {exc}", err=True)
                raise typer.Exit(code=2)
        if model is not None:
            node["model"] = None if model.lower() == "null" else model
        if model_tier is not None:
            typer.echo(
                "warning: --model-tier is deprecated; use --difficulty",
                err=True,
            )
            if model_tier.lower() == "null":
                node["model_tier"] = None
                node["difficulty"] = None
            else:
                try:
                    band = normalize_difficulty(model_tier)
                except ValueError:
                    typer.echo(
                        f"fno backlog update: invalid --model-tier {model_tier!r}; "
                        "expected high, medium, or low.",
                        err=True,
                    )
                    raise typer.Exit(code=2)
                node["model_tier"] = band
                node["difficulty"] = band
        if type_ is not None:
            node["type"] = type_
        if public is not None:
            node["public"] = public
        if acknowledge_collisions is not None:
            ids = [x.strip() for x in acknowledge_collisions.split(",") if x.strip()]
            node["collisions_acknowledged"] = ids
        if completion_note is not None:
            if completion_note.lower() == "null":
                node["completion_note"] = None
            else:
                new_note = completion_note.strip()
                if new_note:
                    existing = node.get("completion_note")
                    if existing and str(existing).strip():
                        node["completion_note"] = f"{existing} + {new_note}"
                    else:
                        node["completion_note"] = new_note
        if add_pr is not None:
            existing_list = list(node.get("additional_prs") or [])
            entry = {"number": int(add_pr)}
            entry["url"] = add_pr_url if add_pr_url is not None else derived_add_pr_url
            if add_pr_note is not None:
                entry["note"] = add_pr_note
            replaced = False
            for i, item in enumerate(existing_list):
                if isinstance(item, dict) and item.get("number") == entry["number"]:
                    merged = dict(item)
                    merged.update(entry)
                    existing_list[i] = merged
                    replaced = True
                    break
            if not replaced:
                existing_list.append(entry)
            node["additional_prs"] = existing_list
        if remove_pr is not None:
            existing_list = list(node.get("additional_prs") or [])
            node["additional_prs"] = [
                item for item in existing_list
                if not (isinstance(item, dict) and item.get("number") == int(remove_pr))
            ]
        if caused_by is not None:
            if caused_by.lower() == "null":
                node["caused_by"] = None
            else:
                origin = _find_node(entries, caused_by)
                if origin is None:
                    typer.echo(f"Error: --caused-by node {caused_by} not found", err=True)
                    raise typer.Exit(code=1)
                if origin["id"] == node["id"]:
                    typer.echo("Error: --caused-by cannot reference the node itself", err=True)
                    raise typer.Exit(code=1)
                # Store the resolved id, not the raw input (which may be a
                # prefix/bare-hex form _find_node normalized).
                # ponytail: self-reference check only; add a cycle walk if a
                # consumer ever traverses caused_by chains.
                node["caused_by"] = origin["id"]
        if fixes_pr is not None:
            node["fixes_pr"] = None if fixes_pr == 0 else int(fixes_pr)
        if reverted is not None:
            node["reverted"] = reverted
        if has_tag_edit:
            # Idempotent set semantics, order-preserving: adds skip dupes,
            # removes are no-ops if absent. Normalization happened above.
            current_tags = list(node.get("tags") or [])
            for t in add_tags:
                if t not in current_tags:
                    current_tags.append(t)
            current_tags = [t for t in current_tags if t not in remove_tags]
            node["tags"] = current_tags
        if parent is not None:
            # Remember the outgoing parent so its rollup can be repainted too:
            # a reparent (or --parent null) leaves the OLD epic/mission counting
            # a child it no longer owns until it is projected (codex P2).
            reparent_old_parent[0] = node.get("parent")
            # Re-parenting AWAY from the delivery unit un-adopts the node
            # (x-e957). Without this there is no supported way out of a mistyped
            # `adopt` id: only decompose writes `contained_in`, re-running the
            # spec without the entry leaves the stale value, and graph.json is a
            # hook-blocked forbidden surface - so one typo permanently unarmed a
            # real delivery unit AND had the cascade later stamp it "shipped
            # inside <owner>", a false completion note on work that never
            # shipped. It also keeps `parent` and `contained_in` from
            # disagreeing, which is what produced that false note.
            #
            # Deliberately keyed on moving away from THE OWNER, not on any
            # re-parent: a contained node moved between two nodes that both sit
            # under its delivery unit is still contained.
            _new_parent = None if parent.lower() == "null" else parent
            _owner = node.get("contained_in")
            if _owner:
                # SUBTREE, not identity (codex P2): the comment above says a
                # move between two nodes under the unit is still contained, and
                # an `== _owner` test contradicted it - re-parenting onto a
                # descendant of the unit silently un-contained the node, making
                # it independently dispatchable and costed again and dropping it
                # from the owner's merge cascade. Walk up from the new parent;
                # depth-capped and cycle-safe like the other ancestor walks.
                _cur = (_find_node(entries, _new_parent) or {}).get("id") if _new_parent else None
                _seen: set = set()
                _still_contained = False
                while _cur and _cur not in _seen and len(_seen) < 64:
                    if _cur == _owner:
                        _still_contained = True
                        break
                    _seen.add(_cur)
                    _cur = (_find_node(entries, _cur) or {}).get("parent")
                if not _still_contained:
                    node.pop("contained_in", None)
            if parent.lower() == "null":
                node["parent"] = None
            else:
                target = _find_node(entries, parent)
                if target is None:
                    typer.echo(f"Error: parent node {parent} not found", err=True)
                    raise typer.Exit(code=1)
                if _would_create_cycle(entries, node["id"], target["id"]):
                    typer.echo(
                        f"Error: setting parent of {node['id']} to {target['id']} "
                        f"would create a cycle",
                        err=True,
                    )
                    raise typer.Exit(code=1)
                if _would_exceed_epic_depth(entries, node, target):
                    typer.echo(
                        f"Error: parenting epic {node['id']} under {target['id']} "
                        f"would exceed the {EPIC_NEST_MAX_DEPTH}-level cap "
                        f"(mission -> epic -> leaf); an epic may nest only under a "
                        f"top-level mission",
                        err=True,
                    )
                    raise typer.Exit(code=1)
                node["parent"] = target["id"]

        # The depth cap must also hold when a --type change alone promotes a node
        # to epic under an already-nested epic (the --parent guard above never
        # fires without --parent). Checked against the FINAL parent edge (codex
        # P1), so a combined --type epic --parent <x> is covered by whichever ran.
        if type_ is not None and node.get("type") == "epic" and node.get("parent"):
            parent_node = _find_node(entries, node["parent"])
            if parent_node is not None and _would_exceed_epic_depth(
                entries, node, parent_node
            ):
                typer.echo(
                    f"Error: making {node['id']} an epic under {parent_node['id']} "
                    f"would exceed the {EPIC_NEST_MAX_DEPTH}-level cap (mission -> "
                    f"epic -> leaf); an epic may nest only under a top-level mission",
                    err=True,
                )
                raise typer.Exit(code=1)

        # `update` deliberately cannot close a node. Closing is merge-gated and
        # belongs to done/reconcile; an ungated, event-silent close flag here is
        # the shape every bypass has taken.
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    # Mutation receipts read the committed, recomputed row. Flags express the
    # caller's intent; only the reread can say whether ownership and dispatch
    # state actually landed.
    from fno.graph.load import load_graph

    stored_node = _find_node(load_graph(_graph_path()), task_id) or {}
    if add_pr is not None and stored_node.get("status") == "ready":
        typer.echo(
            f"warning: {stored_node.get('id', task_id)} is still offered by ready; "
            f"bind ownership and the primary PR with --locked-by <worker> "
            f"--pr-number {add_pr}",
            err=True,
        )
    if pr_number is not None and not clearing_number:
        stored_owner = stored_node.get("locked_by") or "unknown"
        stored_pr = stored_node.get("pr_number")
        stored_status = stored_node.get("status") or "unknown"
        ready_effect = (
            "still offered by ready"
            if stored_status == "ready"
            else "not offered by ready"
        )
        typer.echo(
            f"ownership: node={stored_node.get('id', task_id)} "
            f"owner={stored_owner} pr={stored_pr} status={stored_status}; "
            f"{ready_effect}"
        )
    typer.echo(f"Updated {task_id}")

    # Ship provenance: the link just committed (lock released), so stamp the row
    # here rather than inside the mutator (which would re-enter the graph lock).
    if ship_stamp_node[0] is not None:
        _stamp_ship_on_pr_link(ship_stamp_node[0])
    if blueprint_stamp_node[0] is not None:
        _stamp_blueprint_on_plan_link(blueprint_stamp_node[0])

    # Project the graph-authoritative fields (nav mirror + forward-only status)
    # onto the plan when a mirrored OR status-affecting field changed. Routed
    # through the fresh-re-read helper (not the pre-recompute `projected_node`)
    # so the node carries its recomputed status: a `--locked-by` claim reads
    # `claimed` -> plan `in_progress` (AC1-HP; the claim goes through this update
    # path, not the `claim` verb). Best-effort.
    if projected_node[0] and (
        locked_by is not None
        or priority is not None
        or project is not None
        or type_ is not None
        or has_blocker_edit
        or plan_path is not None
        or size is not None
        or parent is not None
        or has_tag_edit
    ):
        # Include the OLD parent on a reparent so its now-stale rollup repaints
        # alongside the new parent's (the converger walks each id's ancestors in
        # the post-mutation graph, so the old chain is only reachable via this id).
        _project_plans_from_graph(
            [
                projected_node[0]["id"],
                *([reparent_old_parent[0]] if reparent_old_parent[0] else []),
            ],
            # The operator typed `--type`, so THIS node's value is observed:
            # write it through, or the graph and the doc disagree and Obsidian's
            # `type == "epic"` view drops a node the graph is rolling up. Scoped
            # to this id - the repaint fan-out must not carry it to siblings.
            mirror_type_for=(projected_node[0]["id"] if type_ is not None else None),
        )


# -- unclaim / release --


def _invoking_session_id() -> Optional[str]:
    """Best-effort id of the session running this command, for the unclaim
    "is this lockfile mine?" check. None => treat any live holder as foreign
    (the safe default: never yank a live peer's claim)."""
    try:
        from fno.carveout.core import resolve_session_id
        from fno.graph._intake import repo_root

        # repo_root() returns a str; resolve_session_id() needs a Path (it does
        # `root / ".fno" / ...`). Without the wrap the TypeError is swallowed
        # below and this always returns None, disabling the own-claim release.
        return resolve_session_id(Path(repo_root()))
    except Exception:
        return None


def _invoking_claim_holder() -> Optional[str]:
    """Best-effort full holder recorded by the active target manifest.

    Codex uses a unique per-target ``session_id`` for event deduplication while
    the durable thread id owns its graph/claim lock. Prefer the manifest's
    explicit ``target_claim_holder``; legacy manifests fall back to the target
    session id.
    """
    try:
        from fno.graph._intake import repo_root

        state = Path(repo_root()) / ".fno" / "target-state.md"
        for line in state.read_text(encoding="utf-8").splitlines():
            if line.lstrip().startswith("target_claim_holder:"):
                value = line.split(":", 1)[1].strip().strip("\"'")
                if value and value != "null":
                    return value
    except Exception:
        pass

    sid = _invoking_session_id()
    return f"target-session:{sid}" if sid else None


def _release_node_lockfile(node_id: str) -> str:
    """Best-effort release of the ``node:<id>`` fno-claim lockfile.

    Releases when the holder is stale (PID dead / TTL expired) or matches the
    invoking session; refuses a LIVE foreign holder (warn + point at
    ``force-release``) so we never silently yank a live peer's claim. Returns a
    short human note for the command summary. Never raises - the graph clear is
    the load-bearing part and must not be undone by a lockfile hiccup.
    """
    try:
        from fno.claims.core import (
            claim_status,
            release_claim,
        )
        from fno.claims.io import claims_root_for
    except Exception:
        return "lockfile untouched (claims module unavailable)"

    key = f"node:{node_id}"
    try:
        root = claims_root_for(key)
        status = claim_status(key, root=root)
        state = status.get("state")

        if state == "free":
            return "no lockfile"
        if state == "stale":
            # Holder-verified release, NOT unconditional force-release (codex P1):
            # between this stale snapshot and the unlink, another dispatcher can
            # reclaim the dead lock with a NEW holder. release_claim() only
            # removes the file if its holder still matches the stale holder we
            # saw, so a fresh live holder is left intact rather than yanked.
            release_claim(key, holder=status.get("holder") or "", root=root)
            return "released stale lockfile"
        if state == "corrupted":
            typer.echo(
                f"warning: lockfile {key} is corrupted; graph claim cleared but "
                f"lockfile left intact. Use `fno agents claim release {key} --force -R <why>` "
                f"to repair.",
                err=True,
            )
            return "lockfile left (corrupted)"

        # state == "live" or "suspect" (x-ba4b): only release if it is ours -
        # a suspect claim (TTL-unexpired, dead pid) is still owned, so a peer's
        # is left intact and only our own is cleared.
        holder = status.get("holder") or ""
        mine = holder == _invoking_claim_holder()
        if mine:
            release_claim(key, holder=holder, root=root)
            return "released own lockfile"

        typer.echo(
            f"warning: lockfile {key} held by LIVE holder {holder!r}; graph claim "
            f"cleared but lockfile left intact. Use "
            f"`fno agents claim release {key} --force -R <why>` to override.",
            err=True,
        )
        return "lockfile left (live foreign holder)"
    except Exception as exc:  # never let a lockfile error mask the graph clear
        return f"lockfile untouched ({exc})"


def _unclaim_node(task_id: str) -> None:
    """Free a claimed node in one call: clear the graph claim (always) and
    best-effort-release the lockfile (stale or owned). Mirrors the graph-side of
    ``update --locked-by null``, then adds the lockfile release the two-step
    dance forced you to do by hand."""
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    resolved_id: Optional[str] = None

    def mutator(entries):
        nonlocal resolved_id
        node = _find_node(entries, task_id)
        if node is None:
            typer.echo(f"Error: graph node {task_id} not found", err=True)
            raise typer.Exit(code=1)
        resolved_id = node["id"]
        # Same field clear as `update --locked-by null`; recompute_statuses
        # derives status back to ready from the now-empty locked_by.
        node["locked_by"] = None
        node["claimed_at"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    lock_note = _release_node_lockfile(resolved_id or task_id)
    typer.echo(f"Unclaimed {resolved_id or task_id} ({lock_note})")


@cli.command("unclaim", hidden=True)
def cmd_unclaim(
    task_id: str = typer.Argument(
        ..., help="Node id to free (reverts claimed -> ready, releases the lockfile)"
    ),
) -> None:
    """Free a claimed node in one call (graph claim + safe lockfile release)."""
    _unclaim_node(task_id)


# -- next --

def _starvation_receipts(
    entries: list[dict],
    project_filter: Optional[str],
    all_: bool,
    scope_ids: Optional[set],
    claimed: set,
    now,
    staleness_days: int,
    *,
    mission: Optional[str] = None,
    roadmap_id: Optional[str] = None,
) -> list[tuple[str, str]]:
    """Classify why each ready-ish in-scope node was NOT selected (G1 receipts).

    Zero-silent-starvation (x-3236, epic Success Definition 2): when ``next``
    returns null but buildable-looking nodes exist, name why each was excluded
    so an operator is never left guessing. Reasons: ``plan-less`` | ``container``
    | ``claimed`` | ``design`` | ``quarantined`` | ``dead-ancestor``. A node
    genuinely in review (open PR) or committed to a batch is not starved and
    gets no line.
    Pure over the injected ``claimed`` set + ``now`` so it is unit-testable.

    Mirrors ``_pick_ready``'s SCOPING (project, parent subtree via ``scope_ids``,
    ``--mission``, ``--roadmap-id``) so a scoped request that returns null never
    explains itself with an out-of-scope node (codex P2). Only the exclusion
    filters differ - that is the whole point of the receipt.
    """
    from fno.backlog.advance import selection_guards
    from fno.graph._intake import filter_by_project

    container_ids = _container_ids(entries)
    # One pass, guarding against a non-dict row (codebase convention: a malformed
    # entry must not AttributeError the cold receipt path).
    by_id: dict = {}
    ready_ish_rows: list[dict] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if e.get("id"):
            by_id[e["id"]] = e
        # `design` rides along with ready/idea: it is buildable-looking work a
        # human can still name explicitly, so a null `next` must explain it
        # rather than drop it silently - the exact starvation this receipt
        # exists to prevent (a backlog that is ALL design-stage would otherwise
        # return null with nothing to say).
        if e.get("status") not in ("ready", "design", "idea") or e.get("completed_at"):
            continue
        if roadmap_id and e.get("roadmap_id") != roadmap_id:
            continue
        if mission and e.get("mission_id") != mission:
            continue
        ready_ish_rows.append(e)
    ready_ish = filter_by_project(ready_ish_rows, project_filter, all_)
    if scope_ids is not None:
        ready_ish = [e for e in ready_ish if e.get("id") in scope_ids]
    out: list[tuple[str, str]] = []
    for e in ready_ish:
        nid = e.get("id")
        if not nid:
            continue
        # A plan hold outranks structural exclusions: the owner may also be a
        # container and a descendant may carry no plan of its own, but the
        # actionable reason every dispatcher must report is the attributable
        # hold on their shared delivery ancestry.
        hold_guard = selection_guards(
            e, by_id, now, staleness_days=staleness_days
        )
        if hold_guard and hold_guard.startswith("dispatch-hold"):
            reason = hold_guard
        elif not e.get("plan_path"):
            reason = "plan-less"
        elif nid in container_ids:
            reason = "container"
        elif nid in claimed:
            reason = "claimed"
        elif e.get("status") in ("design", "idea"):
            # Read off the persisted rung, not the guard: the guard only fires
            # for the stale window (graph still says `ready`, doc since edited
            # down a rung), so a node already ON the rung would fall through to
            # `selection_guards`, get None (it is gated on a persisted `ready`),
            # and be dropped by the `continue` - reporting nothing at all.
            #
            # `idea` is the COMMON case for a linked decompose scaffold, since
            # recomputation persists that rung directly; without this arm a
            # backlog of nothing but undesigned children prints a bare `null`
            # instead of naming what each one is waiting on.
            reason = e["status"]
        elif e.get("status") == "ready" and (
            _has_unmerged_open_pr(e) or _is_batched_member(e)
        ):
            continue  # in review / batched - handled, not starved
        else:
            g = hold_guard
            if not g:
                continue  # no known exclusion (would have been selected)
            if g.startswith("dead-ancestor"):
                reason = "dead-ancestor"
            elif g.startswith("contained"):
                # Not starvation either: the work IS being delivered, inside
                # another node's PR. Left in the generic `quarantined` bucket it
                # read as stale work needing attention, and a decomposed epic
                # printed one bogus line per adopted node on every `next` until
                # its unit merged - permanent noise the operator cannot act on.
                reason = "contained"
            elif g == "design-stage":
                # Not starvation: planned but not blueprinted, so it reads as
                # its own rung rather than the generic quarantine bucket.
                reason = "design"
            elif g == "idea-stage":
                # Also not starvation: a linked-but-undesigned doc (a decompose
                # scaffold, or a plan hand-edited back down). Named separately
                # from `design` so the receipt says which pass it is waiting on.
                reason = "idea"
            else:
                reason = "quarantined"
        out.append((nid, reason))
    return out


def _external_open_status(*, pr_number: Optional[int], plan_path: Optional[str]) -> str:
    """Read-time status for an OPEN external item: in_review > ready > idea.

    A plan-less, PR-less open node is dispatch work nobody has started
    planning yet, not ready work - the same three-way split selection
    filters on. Every external-backend status render (selection, the mux
    snapshot, `backlog get`) must derive from this one function so a node
    dispatch skips never reads as ready anywhere else.
    """
    if pr_number:
        return "in_review"
    return "ready" if plan_path else "idea"


def _read_external_node_and_sidecar(id: str):
    """Exact-id tracker read plus sidecar load, shared by every external-backend
    single-node renderer. Raises the tracker's ``NodeNotFound`` unchanged so
    each caller keeps its own not-found message and exit code."""
    from fno.tracker import get_tracker
    from fno.tracker import sidecar as sidecar_store

    node = get_tracker().read(id)
    sc = sidecar_store.load(id)
    return node, sc


class _ExternalSelectionError(RuntimeError):
    """A tracker or required sidecar read failed during joined selection.

    AC6-ERR: selection fails CLOSED. The message names the failing backend or
    id; the caller exits nonzero and never falls back to the local graph file
    or silently selects a different node.
    """


def _joined_open_candidates() -> list[dict]:
    """The transient joined selection model: ``list_open()`` exactly once, one
    sidecar load per OPEN id, never the closed history (AC4's bound: 48 live
    rows, not the ~2,000 inactive archive).

    A transient render for selection filters and ranking only - never
    persisted, never a shared convenience record (locked decision 3). The
    rung is DERIVED at read time from seam-carried evidence (open + PR = in
    review; open + linked plan = ready; plan-less = idea, the cold-dispatch
    admission): no stored status flag crosses the seam, and footnote's
    mutation-stamped rungs (deferred/superseded) cannot exist on an
    externally-owned item. Footnote-minted scoping pins (project, roadmap_id,
    mission_*) carry no external equivalent and stay absent: a scoped request
    over them is honestly empty, and `--project`/-A detection degrades to
    "all open candidates" exactly as a join with no project column must.
    Priority/rank/created_at ride the selection projection, so footnote's
    ranking (applied by _pick_ready AFTER this join) is tracker-owned on both
    backends - the same sort key picks the same winner (AC5).
    """
    from fno.tracker import get_tracker
    from fno.tracker import sidecar as sidecar_store

    tracker = get_tracker()
    try:
        candidates = tracker.list_open()
    except Exception as exc:  # noqa: BLE001 - name the backend, fail closed
        raise _ExternalSelectionError(
            f"tracker {tracker.name!r} list_open failed: {exc}"
        ) from exc
    joined: list[dict] = []
    for c in candidates:
        try:
            sc = sidecar_store.load(c.id)
        except Exception as exc:  # noqa: BLE001 - name the id, fail closed
            raise _ExternalSelectionError(
                f"sidecar read failed for {c.id}: {exc}"
            ) from exc
        row = {
            "id": c.id,
            "title": c.title,
            "state": str(c.state.value),
            "status": _external_open_status(pr_number=sc.pr_number, plan_path=sc.plan_path),
            "parent": c.parent,
            "blocked_by": list(c.blocked_by),
            "priority": c.priority,
            "rank": c.rank,
            "created_at": c.created_at,
            # Footnote-owned selection facts joined before the filters run.
            "cwd": sc.cwd,
            "plan_path": sc.plan_path,
            "pr_number": sc.pr_number,
            "pr_url": sc.pr_url,
            "additional_prs": sc.additional_prs,
            "batch": sc.batch,
            "contained_in": sc.contained_in,
            "sessions": sc.sessions,
            "claimed_at": sc.claimed_at,
            "cost_usd": sc.cost_usd,
        }
        joined.append(row)
    return joined


#: How long an external selector's `node:<id>` claim protects the node it just
#: handed out. It is a SELECTION window, not a work lease: the winner is handed
#: to a caller that has yet to launch anything, and the worker replaces this
#: hold with its own the moment `fno target init` runs. Sized to match the spawn
#: handover window for the same reason - both cover launch-to-init, and the
#: selector's caller has strictly less to do before init than a spawn does. Short
#: on purpose: a selector that never dispatches must not wedge the node, and an
#: expired claim is provably dead so the node self-heals.
EXTERNAL_SELECTION_TTL = "15m"


@cli.command("next")
def cmd_next(
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    parent: Optional[str] = typer.Option(
        None,
        "--parent",
        help="Restrict to transitive children of this epic node (ab-ID).",
    ),
    claim: Optional[str] = typer.Option(None, "--claim", help="Session ID to atomically claim"),
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Consider all projects"),
    include_ideas: bool = typer.Option(
        False,
        "--ideas",
        "-I",
        "--include-ideas",
        help="Also consider idea-stage rows (plan-less nodes) as claimable.",
    ),
    include_deferred: bool = typer.Option(
        False,
        "--include-deferred",
        help="Also consider deferred rows for explicit re-engagement.",
    ),
    mission: Optional[str] = typer.Option(
        None,
        "--mission",
        help=(
            "Restrict to nodes whose mission_id matches (megatron child walks: "
            "the walk works ONLY the mission's nodes)."
        ),
    ),
) -> None:
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import (
        detect_project, filter_by_project, make_selection_sort_key,
        descendants_of, _find_node,
    )
    from fno.graph.ladder import is_cold_dispatchable
    from fno.tracker import active_backend_name

    result: list = [None]
    project_filter = project
    _external = active_backend_name() != "graph"
    # One read for the prelude AND selection: under an external backend the
    # transient joined model (list_open + sidecar join, fail-closed); under
    # the default backend the working graph, read at most once for project
    # detection AND parent resolution (both need the full entry list).
    try:
        if _external:
            pre_entries = _joined_open_candidates()
        else:
            pre_entries = None
            if (not project_filter and not all_) or parent:
                pre_entries = read_graph(_graph_path())
    except _ExternalSelectionError as exc:
        typer.echo(f"Error: {exc}; selection refused", err=True)
        raise typer.Exit(code=1)
    if not project_filter and not all_:
        assert pre_entries is not None  # set under the same condition above
        project_filter = detect_project(pre_entries)

    # Epic-scope filter (C2, ab-facfaade): restrict candidates to the
    # transitive children of --parent. Resolve the parent id up-front so a
    # missing node is a hard error (AC2-ERR) and a childless node prints a
    # clear note while still returning null so the walker can fall back
    # (AC2-EDGE). The actual descendant SET is computed inside _pick_ready
    # from the entries it receives so that under --claim it reflects the
    # locked graph state, not a pre-read snapshot (avoids a TOCTOU where a
    # concurrent reparent could claim a node no longer in the subtree).
    parent_target_id: Optional[str] = None
    if parent:
        assert pre_entries is not None  # set when `parent` is truthy above
        target = _find_node(pre_entries, parent)
        if target is None:
            typer.echo(f"Error: no such node '{parent}'", err=True)
            raise typer.Exit(code=1)
        parent_target_id = target["id"]
        if not descendants_of(pre_entries, parent_target_id):
            typer.echo(f"no children under {parent_target_id}", err=True)

    allowed = {"ready"}
    if include_ideas:
        allowed.add("idea")
    if include_deferred:
        allowed.add("deferred")

    def _pick_ready(entries):
        # read_graph does not recompute status, so a node closed out of band
        # (e.g. PR merged via reconcile/done in another process) can carry
        # completed_at while its persisted status is still "ready". Guard on
        # completed_at so advance / megawalk never dispatch a /target worker for
        # an already-done node.
        # `allowed` covers the persisted-status gate (ready, plus idea/deferred
        # only on explicit --include-ideas/--include-deferred). A plan-less idea
        # (Rung.NONE) is ALSO admitted by default (x-e24a): `/target` authors its
        # plan, so the autonomous drain dispatches it. A linked-but-undesigned
        # decompose stub (Rung.IDEA) is NOT admitted here - it needs warm
        # inline-fill and stays behind --include-ideas.
        candidates = [
            e for e in entries
            if (e.get("status") in allowed or is_cold_dispatchable(e))
            and not e.get("completed_at")
        ]
        if roadmap_id:
            candidates = [e for e in candidates if e.get("roadmap_id") == roadmap_id]
        if mission:
            candidates = [e for e in candidates if e.get("mission_id") == mission]
        if parent_target_id is not None:
            scope = descendants_of(entries, parent_target_id)
            candidates = [e for e in candidates if e.get("id") in scope]
        candidates = filter_by_project(candidates, project_filter, all_)
        # Selection-time claim enforcement (ab-fcf9cec5): drop nodes a live
        # session already holds so a second pickup is impossible.
        claimed = _require_live_claimed_node_ids("backlog selection")
        if claimed:
            candidates = [e for e in candidates if e.get("id") not in claimed]
        # Drop READY nodes that already carry an unmerged open PR so a successor
        # dispatch (advance / megawalk, both shelling `fno backlog next`) never
        # re-builds already-PR'd work (ab-372130f6). The PID-based node claim
        # is gone once the builder session exits, so this PR-state guard is the
        # only in-flight signal left during the review window.
        #
        # Scoped to status "ready": a deferred/idea row only appears here when
        # an operator explicitly asked for it via --include-deferred /
        # --include-ideas, and the defer contract says those resurface on
        # request. The guard is about AUTO re-selection of fresh ready work, not
        # explicit re-engagement, so it must not suppress an explicitly-included
        # paused PR-bearing node (codex PR #516 P2). The auto paths only ever
        # pass bare `next` (allowed == {"ready"}), so this scoping is a no-op
        # for them and the originally-observed bug node (ready + open PR) is
        # still caught.
        candidates = [
            e for e in candidates
            if e.get("status") != "ready" or not _has_unmerged_open_pr(e)
        ]
        # Containers are never directly buildable (x-33b2): an epic's work lives
        # in its decomposed children, so `next` must never return it - it
        # otherwise ranks first among its siblings (make_selection_sort_key) and
        # is repeatedly re-selected as the head, starving the genuinely-ready leaf
        # below it. Build the leaves, not the box; the epic closes itself via
        # _cascade_close_parents when its last child lands. Computed from the FULL
        # graph so a parent already filtered out of `candidates` still suppresses
        # correctly. Shared with cmd_ready.
        container_ids = _container_ids(entries)
        candidates = [e for e in candidates if e.get("id") not in container_ids]
        # Batch-lane Wave 2: a node already committed to an open batch ships via
        # the batch PR, so drop it from the dispatch pool (else a second worker
        # rebuilds work already on the shared branch). Cleared on abandon so a
        # requeued member resurfaces. Shared with cmd_ready.
        candidates = [e for e in candidates if not _is_batched_member(e)]
        # G1 guards (x-3236): dead-ancestor + stale-ready quarantine. The single
        # narrowing choke point shared with the converge path
        # (advance._direct_dependents), so a leaf under a killed epic or a
        # long-abandoned ready node is never dispatched. Fail-open per guard.
        from fno.backlog.advance import selection_guards, _guard_staleness_days

        guard_now = datetime.now(timezone.utc)
        guard_stale = _guard_staleness_days()
        guard_by_id = {e.get("id"): e for e in entries if e.get("id")}
        candidates = [
            e for e in candidates
            if not selection_guards(
                e, guard_by_id, guard_now, staleness_days=guard_stale
            )
        ]
        # Epics-first, then flat priority (C3, Locked Decision 7). Build the
        # key from the FULL graph so epic parents resolve even when filtered
        # out of the candidate set.
        candidates.sort(key=make_selection_sort_key(entries, live_claimed=claimed))
        return candidates

    def _node_summary(e):
        return {
            # slug leads (ab-f82e8083); `id` stays the canonical key right after.
            "slug": e.get("slug"),
            "id": e["id"], "title": e.get("title"),
            "priority": e.get("priority"), "domain": e.get("domain"),
            "project": e.get("project"), "cwd": e.get("cwd"),
            "size": e.get("size"), "plan_path": e.get("plan_path"),
            "difficulty": e.get("difficulty") or e.get("model_tier"),
            # x-571f: the per-node model pin must ride in the next-JSON so the
            # active-backlog drain can prefer it over cfg.model.
            # model_tier rides alongside it so the dispatch-time tier resolver
            # sees the annotation (else it silently falls back to the default).
            "model": e.get("model"),
            "model_tier": e.get("model_tier"),
            # x-0676: the per-node dispatch overrides must ride in the next-JSON so
            # `advance`'s resolver routing (US1) actually fires for real graph nodes
            # (which come from this summary), not only for tests that inject them.
            "dispatch_verb": e.get("dispatch_verb"),
            "dispatch_brief": e.get("dispatch_brief"),
            "mission_id": e.get("mission_id"),
            "mission_wave": e.get("mission_wave"),
            "mission_slug": e.get("mission_slug"),
            "mission_from_msg_id": e.get("mission_from_msg_id"),
        }

    from fno.backlog.undispatched import (
        ObserverReadError,
        build_selection_divergence_event,
        classify_planned_unclaimed,
        prepend_missed_rows,
        read_claim_snapshot,
        read_planned_unclaimed,
        read_planned_unclaimed_from_entries,
    )
    try:
        if _external:
            assert pre_entries is not None
            read_planned_unclaimed_from_entries(
                pre_entries,
                project=None if all_ else project_filter,
                mission=mission,
                roadmap_id=roadmap_id,
                parent=parent_target_id,
            )
        else:
            read_planned_unclaimed(
                graph_path=_graph_path(),
                project=None if all_ else project_filter,
                mission=mission,
                roadmap_id=roadmap_id,
                parent=parent_target_id,
            )
    except ObserverReadError as exc:
        typer.echo(f"Error: {exc}; selection refused", err=True)
        raise typer.Exit(code=1) from exc

    def _with_observer(candidates: list[dict], source_entries: list[dict]) -> list[dict]:
        by_id = {entry.get("id"): entry for entry in source_entries}
        try:
            current_observer = classify_planned_unclaimed(
                source_entries,
                read_claim_snapshot(),
                project=None if all_ else project_filter,
                mission=mission,
                roadmap_id=roadmap_id,
                parent=parent_target_id,
            )
        except Exception as exc:  # noqa: BLE001 - unknown state refuses recovery
            typer.echo(f"Error: observer revalidation failed: {exc}", err=True)
            raise typer.Exit(code=1) from exc
        claimed = _require_live_claimed_node_ids("backlog next observer recovery")
        container_ids = _container_ids(source_entries)
        from fno.backlog.advance import _guard_staleness_days, selection_guards

        guard_now = datetime.now(timezone.utc)
        guard_stale = _guard_staleness_days()
        safe_rows = []
        for row in current_observer["rows"]:
            entry = by_id.get(row.get("id"))
            if entry is None or row.get("id") in claimed:
                continue
            if entry.get("completed_at") or _has_unmerged_open_pr(entry):
                continue
            if entry.get("id") in container_ids or _is_batched_member(entry):
                continue
            if selection_guards(
                entry,
                by_id,
                guard_now,
                staleness_days=guard_stale,
            ):
                continue
            safe_rows.append(entry)
        safe_observer = {**current_observer, "rows": safe_rows}
        merged, missed = prepend_missed_rows(candidates, safe_observer)
        if missed:
            scope = f"project={(project_filter if not all_ else '*')}"
            if mission:
                scope += f",mission={mission}"
            if roadmap_id:
                scope += f",roadmap={roadmap_id}"
            for row in missed:
                try:
                    from fno import paths
                    from fno.events import append_event

                    append_event(
                        build_selection_divergence_event(
                            node_id=row["id"],
                            selector_command="fno backlog next",
                            scope=scope,
                            selector_entries_scanned=len(candidates),
                            observer_entries_scanned=current_observer["entries_scanned"],
                        ),
                        paths.project_events_json(),
                    )
                except Exception as exc:  # noqa: BLE001 - receipt is non-gating
                    typer.echo(f"warning: selection divergence event failed: {exc}", err=True)
        return merged

    if claim:
        if _external:
            # External claims use the claims subsystem only: no graph
            # mutation, and no claim pointer written into tracker or sidecar
            # (the live holder lives in the claims dir). Contention falls
            # through to the next ranked candidate rather than failing the
            # whole selection.
            from fno.claims.cli import _parse_ttl
            from fno.claims.core import ClaimHeldByOther, acquire_claim
            from fno.claims.io import claims_root_for

            assert pre_entries is not None
            candidates = _with_observer(_pick_ready(pre_entries), pre_entries)
            for winner in candidates:
                key = f"node:{winner['id']}"
                # TWO things have to be true for this lock to protect anything,
                # and routing alone gave only the first.
                #
                # ROUTE the root, or the lock lands in the cwd-default tree
                # while every reader of a `node:` key resolves the global root
                # through `claims_root_for`, so the node still reads `free`.
                # `_read_node_claim` names the same trap from the other side.
                #
                # TTL, or the lock is visible and still not honored. Selection
                # runs in a process that exits as soon as it prints the node,
                # so a pid-liveness claim is dead on arrival: it reads `stale`,
                # which does not block dispatch, and a worker launches onto the
                # node this selector just handed out. The TTL makes the dead pid
                # read `suspect` instead, which does block, and it bounds the
                # hold so an external selector that never dispatches cannot wedge
                # the node past the window.
                try:
                    acquire_claim(
                        key,
                        claim,
                        ttl_ms=_parse_ttl(EXTERNAL_SELECTION_TTL),
                        root=claims_root_for(key),
                    )
                except ClaimHeldByOther:
                    continue
                result[0] = _node_summary(winner)
                break
        else:
            def mutator(entries):
                candidates = _with_observer(_pick_ready(entries), entries)
                if candidates:
                    winner = candidates[0]
                    winner["locked_by"] = claim
                    winner["claimed_at"] = datetime.now(timezone.utc).isoformat()
                    result[0] = _node_summary(winner)
                return entries
            locked_mutate_graph(_graph_path(), mutator)
    else:
        if _external:
            assert pre_entries is not None
            entries = pre_entries
        else:
            entries = read_graph(_graph_path())
        candidates = _with_observer(_pick_ready(entries), entries)
        if candidates:
            result[0] = _node_summary(candidates[0])

    if result[0] is None:
        # Zero-silent-starvation receipts (x-3236 G1): explain to stderr why
        # nothing was picked. Advisory - stdout stays exactly the node-or-"null"
        # contract `_next_node` parses, so a receipt failure never breaks
        # dispatch. Under an external backend the receipts explain the ACTUAL
        # joined denominator, never the local graph.
        try:
            from fno.backlog.advance import _guard_staleness_days

            recv_entries = (
                pre_entries if _external
                else (read_graph(_graph_path()) if claim else entries)
            ) or []
            scope_ids = (
                descendants_of(recv_entries, parent_target_id)
                if parent_target_id is not None else None
            )
            for nid, reason in _starvation_receipts(
                recv_entries, project_filter, all_, scope_ids,
                _live_claimed_node_ids(),
                datetime.now(timezone.utc),
                _guard_staleness_days(),
                mission=mission,
                roadmap_id=roadmap_id,
            ):
                typer.echo(f"excluded {nid}: {reason}", err=True)
        except Exception as exc:  # noqa: BLE001 - receipts are advisory
            typer.echo(f"warning: starvation receipts failed: {exc}", err=True)

    typer.echo(json.dumps(result[0], indent=2) if result[0] else "null")


# -- undispatched --

@cli.command("undispatched", hidden=True)
def cmd_undispatched(
    project: Optional[str] = typer.Option(None, "--project", "-p"),
    all_: bool = typer.Option(False, "--all", "-A"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    parent: Optional[str] = typer.Option(None, "--parent"),
    mission: Optional[str] = typer.Option(None, "--mission"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Name finalized, ready leaf plans with no node claim."""
    del all_, json_output  # the observer is JSON by contract and all-scoped by default
    from fno.backlog.undispatched import (
        ObserverReadError,
        read_planned_unclaimed,
        read_planned_unclaimed_from_entries,
    )

    try:
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            receipt = read_planned_unclaimed_from_entries(
                _joined_open_candidates(),
                project=project,
                mission=mission,
                roadmap_id=roadmap_id,
                parent=parent,
            )
        else:
            receipt = read_planned_unclaimed(
                graph_path=_graph_path(),
                project=project,
                mission=mission,
                roadmap_id=roadmap_id,
                parent=parent,
            )
    except _ExternalSelectionError as exc:
        typer.echo(f"Error: tracker unreadable: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    except ObserverReadError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    typer.echo(json.dumps(receipt, indent=2))


# -- ready --

@cli.command("ready", hidden=True)
def cmd_ready(
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    parent: Optional[str] = typer.Option(
        None,
        "--parent",
        help="Restrict to transitive children of this epic node (ab-ID).",
    ),
    include_ideas: bool = typer.Option(
        False,
        "--ideas",
        "-I",
        "--include-ideas",
        help="Also list idea-stage rows (plan-less nodes) alongside ready ones.",
    ),
    include_deferred: bool = typer.Option(
        False,
        "--include-deferred",
        help="Also list deferred rows for explicit re-engagement.",
    ),
    mission: Optional[str] = typer.Option(
        None,
        "--mission",
        help="Restrict to nodes whose mission_id matches (same contract as `next`).",
    ),
    # ponytail: `ready` already always emits JSON; the flag exists only so a
    # caller passing --json (inbox triage) isn't rejected with Typer exit 2.
    # Accepted-and-ignored, never a behavior switch.
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON (default; flag accepted for parity)."
    ),
) -> None:
    from fno.graph.store import read_graph
    from fno.graph._intake import (
        filter_by_project, make_selection_sort_key, descendants_of, _find_node,
    )
    from fno.graph.ladder import is_cold_dispatchable
    from fno.tracker import active_backend_name

    # Joined selection under an external backend: the same filters and ranking
    # run over the transient list_open + sidecar join (fail-closed, never the
    # local graph), so `ready` and `next` cannot drift between backends.
    if active_backend_name() != "graph":
        try:
            entries = _joined_open_candidates()
        except _ExternalSelectionError as exc:
            typer.echo(f"Error: {exc}; selection refused", err=True)
            raise typer.Exit(code=1)
    else:
        entries = read_graph(_graph_path())
    allowed = {"ready"}
    if include_ideas:
        allowed.add("idea")
    if include_deferred:
        allowed.add("deferred")
    # read_graph does not recompute status, so a node closed out of band can
    # carry completed_at while its persisted status is still "ready". Guard on
    # completed_at so a done node never lists as actionable work (the same guard
    # is in `next`'s _pick_ready, the dispatch path).
    # Same plan-less idea admission as `next`'s _pick_ready (x-e24a): a Rung.NONE
    # idea is cold-dispatchable and surfaces alongside ready work; a linked
    # Rung.IDEA stub stays behind --include-ideas.
    ready = [
        e for e in entries
        if (e.get("status") in allowed or is_cold_dispatchable(e))
        and not e.get("completed_at")
    ]
    ready = filter_by_project(ready, project, all_)
    if roadmap_id:
        ready = [e for e in ready if e.get("roadmap_id") == roadmap_id]
    # Mission scope (same rule as `next`): a mission-scoped caller (the
    # active-backlog daemon's lane-fill, megatron child walks) must never see
    # out-of-mission nodes as actionable (codex P1 on PR #137).
    if mission:
        ready = [e for e in ready if e.get("mission_id") == mission]
    # Epic-scope filter (C2, ab-facfaade): transitive children of --parent.
    if parent:
        target = _find_node(entries, parent)
        if target is None:
            typer.echo(f"Error: no such node '{parent}'", err=True)
            raise typer.Exit(code=1)
        scope = descendants_of(entries, target["id"])
        if not scope:
            typer.echo(f"no children under {target['id']}", err=True)
        ready = [e for e in ready if e.get("id") in scope]
    # Selection-time claim enforcement (ab-fcf9cec5): hide nodes a live
    # session already holds (same rule as `graph next`).
    claimed = _require_live_claimed_node_ids("backlog ready")
    if claimed:
        ready = [e for e in ready if e.get("id") not in claimed]
    # Same in-flight guard as `next` (ab-372130f6): a human / megawalk `ready`
    # listing must not present an already-PR'd node as actionable work.
    # cmd_ready keeps its own inline status/claim filter (it does not route
    # through _pick_ready), so the guard is applied here too for parity.
    # Scoped to status "ready" so an explicitly --include-deferred / -ideas
    # paused PR-bearing node still lists (the defer contract resurfaces those
    # on request; codex PR #516 P2).
    ready = [
        e for e in ready
        if e.get("status") != "ready" or not _has_unmerged_open_pr(e)
    ]
    # Containers are never actionable work (x-33b2 / codex P2 on PR #69): drop
    # epics so `fno backlog ready` - and the `dispatch-node.sh --all-ready` bulk
    # path that enumerates it - never presents/launches the box instead of its
    # leaves. No all-done exception: the epic auto-closes via
    # _cascade_close_parents when its last child lands, so it is already done
    # rather than a lingering ready container. Shares _container_ids with `next`'s
    # _pick_ready so the surfaces cannot drift.
    container_ids = _container_ids(entries)
    ready = [e for e in ready if e.get("id") not in container_ids]
    # Batch-lane Wave 2: hide open-batch members (they ship via the batch PR, not
    # as individual ready work). Shares _is_batched_member with `next`'s
    # _pick_ready so the surfaces cannot drift.
    ready = [e for e in ready if not _is_batched_member(e)]
    # G1 guards (x-3236): dead-ancestor + stale-ready quarantine, the SAME filter
    # `next`'s _pick_ready applies. `ready` is a third dispatch-feeding surface -
    # `select_lane_fill` -> `_ready_nodes` shells `fno backlog ready` for both
    # parallel lane-fill AND the active-backlog daemon's single-node path, and
    # `dispatch-node.sh --all-ready` enumerates it - so an unguarded `ready`
    # would dispatch exactly the nodes `next` quarantines. Shares selection_guards
    # so the surfaces cannot drift.
    from fno.backlog.advance import selection_guards, _guard_staleness_days

    _guard_now = datetime.now(timezone.utc)
    _guard_stale = _guard_staleness_days()
    _guard_by_id = {e.get("id"): e for e in entries if e.get("id")}
    ready = [
        e for e in ready
        if not selection_guards(
            e, _guard_by_id, _guard_now, staleness_days=_guard_stale
        )
    ]
    # Epics-first, then flat priority (C3, Locked Decision 7); key built
    # from the full graph so epic parents always resolve.
    ready.sort(key=make_selection_sort_key(entries, live_claimed=claimed))

    output = [{
        # slug leads (ab-f82e8083) so a `ready` list / clipboard is readable.
        "slug": e.get("slug"),
        "id": e["id"], "title": e.get("title"), "priority": e.get("priority"),
        "domain": e.get("domain"), "project": e.get("project"),
        "cwd": e.get("cwd"), "parent": e.get("parent"),
        "difficulty": e.get("difficulty") or e.get("model_tier"),
        # select_lane_fill's dispatch-time collision gate compares plan file
        # surfaces; without this it has nothing to read.
        "plan_path": e.get("plan_path"),
        # x-571f: carry the model pin so the lane-fill dispatcher (select_lane_fill
        # -> _ready_nodes -> `fno backlog ready`) can thread it into the spawn.
        # model_tier rides alongside so the tier resolver sees the annotation.
        "model": e.get("model"),
        "model_tier": e.get("model_tier"),
    } for e in ready]

    typer.echo(json.dumps(output, indent=2))


# -- lane-fill --

@cli.command("lane-fill", hidden=True)
def cmd_lane_fill(
    max_lanes: Optional[int] = typer.Option(
        None, "--max", help="Max lanes (default: config.parallel.max_lanes)."
    ),
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Filter by project name"
    ),
    mission: Optional[str] = typer.Option(
        None, "--mission", help="Restrict selection to this mission's nodes."
    ),
    claim: bool = typer.Option(
        False,
        "--claim",
        help="Atomically hold a lane slot per selected node (default: preview only).",
    ),
) -> None:
    """Select up to max_lanes ready nodes from DISTINCT domains (parallel mode).

    Prints the JSON list of nodes that would dispatch as concurrent lanes, one
    per distinct domain (epic x-42d5, group 2). Read-only by default; ``--claim``
    atomically holds a dispatch-time lane slot per node - what the dispatcher
    does before spawn (Locked Decision #8). ``max_lanes < 2`` prints ``[]``
    (sequential: use ``fno backlog next``).
    """
    from fno.backlog.advance import select_lane_fill

    if max_lanes is None:
        from fno.config import load_settings
        max_lanes = load_settings().parallel.max_lanes

    selected = select_lane_fill(max_lanes, project, mission=mission, claim=claim)
    typer.echo(json.dumps(selected, indent=2))


# -- schedule (shadow) --

# -- dispatch-lanes --

@cli.command("dispatch-lanes", hidden=True)
def cmd_dispatch_lanes(
    max_lanes: Optional[int] = typer.Option(
        None, "--max", help="Max lanes (default: config.parallel.max_lanes)."
    ),
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Filter by project name"
    ),
    mission: Optional[str] = typer.Option(
        None, "--mission", help="Restrict dispatch to this mission's nodes."
    ),
    model: Optional[str] = typer.Option(
        None, "--model", "-m",
        help="Pin a model for every lane spawned this run, overriding node annotations.",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider",
        help="Pin a provider for every lane. (No -p short: it is --project here.)",
    ),
) -> None:
    """Spawn up to max_lanes isolated background lanes (parallel mode, group 3).

    Selects distinct-domain ready nodes (like ``lane-fill``), then for each one
    isolates a worktree off origin/main, seeds its per-lane
    ``.fno/config.local.toml`` (x-cbce: own project.id), and
    spawns a detached ``/target --no-merge`` worker rooted there. Prints one JSON
    receipt per lane (``status`` dispatched | skipped). ``max_lanes < 2`` spawns
    nothing (sequential: use ``fno backlog advance`` / ``next``).
    """
    from fno.dispatch_flags import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )
    from fno.backlog.advance import dispatch_lanes

    try:
        model = reject_empty_model(model)
        provider = resolve_dispatch_provider(provider)[0] if provider is not None else None
    except DispatchFlagError as exc:
        typer.echo(f"dispatch-lanes: {exc}", err=True)
        raise typer.Exit(code=2)

    if max_lanes is None:
        from fno.config import load_settings
        max_lanes = load_settings().parallel.max_lanes

    receipts = dispatch_lanes(max_lanes, project, mission=mission, model=model, provider=provider)
    typer.echo(json.dumps(receipts, indent=2))


# -- groom --

@cli.command("groom", hidden=True)
def cmd_groom(
    model: Optional[str] = typer.Option(
        None, "--model", "-m", help="Model for the groom worker (default: sonnet)."
    ),
    dry_run: bool = typer.Option(
        False, "--dry-run", "-N", help="Print the brief and day key without dispatching."
    ),
    age: Optional[int] = typer.Option(
        None, "--age", help="Archive-leg age gate in days (default: 14)."
    ),
    install_agent: bool = typer.Option(
        False, "--install-agent", help="Install the daily LaunchAgent and exit (macOS)."
    ),
    refresh_agent: bool = typer.Option(
        False,
        "--refresh-agent",
        help="Re-render an installed LaunchAgent onto the current binary and exit.",
    ),
    hour: Optional[int] = typer.Option(
        None, "--hour", help="Local hour for --install-agent (default: 2)."
    ),
    check: bool = typer.Option(
        False,
        "--check",
        help="Report grooming freshness and exit 0 only if a pass is due. Runs nothing.",
    ),
) -> None:
    """Run today's grooming pass over the backlog (at most once a day).

    One pipeline: the mechanical legs (archive, reconcile, maintain, relatedness)
    run first under the daily claim, then ONE Sonnet worker makes the judgment
    calls. Judgment is levers-only: the worker may supersede, defer/undefer,
    re-prioritize, rank, promote, and file ideas - never anything else - and mails
    a one-screen report of every mutation with its receipt. A second run on the
    same UTC day exits 0 with an ``already-ran`` receipt, running nothing at all.
    """
    from fno.backlog.groom import (
        GROOM_AGE_DEFAULT,
        GROOM_HOUR_DEFAULT,
        GROOM_MODEL_DEFAULT,
        groom_is_due,
        groom_staleness,
        install_groom_agent,
        refresh_groom_agent,
        run_groom,
    )

    if check:
        # The shell bridge to the freshness predicate, so the SessionStart
        # fallback can gate on it without re-implementing the marker scan.
        # Exit code is the answer; stdout is for a human reading the receipt.
        state, hours = groom_staleness()
        typer.echo(json.dumps({"state": state, "hours": hours}))
        raise typer.Exit(code=0 if groom_is_due((state, hours)) else 1)

    if refresh_agent:
        # The tail of `fno doctor update`, so it must never fail the update: a skipped
        # or failed refresh reports and exits 0.
        typer.echo(json.dumps(refresh_groom_agent(), indent=2))
        return

    if install_agent:
        receipt = install_groom_agent(hour=hour if hour is not None else GROOM_HOUR_DEFAULT)
        typer.echo(json.dumps(receipt, indent=2))
        if receipt.get("status") == "failed":
            raise typer.Exit(code=1)
        return

    receipt = run_groom(
        cwd=os.getcwd(),
        model=model or GROOM_MODEL_DEFAULT,
        dry_run=dry_run,
        age=age if age is not None else GROOM_AGE_DEFAULT,
    )
    typer.echo(json.dumps(receipt, indent=2))
    # `degraded` exits non-zero too: the pass ran, but a mechanical leg is broken
    # and a scheduler log nobody reads is not a signal.
    if receipt.get("status") in ("failed", "degraded"):
        raise typer.Exit(code=1)


# -- lanes --

@cli.command("lanes", hidden=True)
def cmd_lanes(
    json_output: bool = typer.Option(False, "--json", "-J", help="JSON rollup."),
) -> None:
    """One-read parallel-lane rollup (US5): live lanes vs the cap.

    Joins each live lane-slot claim with its graph node (slug, status, PR) so
    the operator reviews the fleet's shape - which nodes hold lanes, in which
    domains - without stitching ``fno agents claim list`` to the board by hand. The
    grid's BgRoster tiles show the workers themselves; this is the aggregated
    outcome view. Read-only.
    """
    from fno.claims.core import list_claims
    from fno.claims.lanes import LANE_SLOT_PREFIX

    try:
        from fno.config import load_settings

        max_lanes = load_settings().parallel.max_lanes
    except Exception:  # noqa: BLE001 - a config miss must not hide live lanes
        max_lanes = 1

    nodes: dict = {}
    try:
        from fno.graph.store import read_graph

        nodes = {
            e["id"]: e
            for e in read_graph(_graph_path())
            if isinstance(e, dict) and e.get("id")
        }
    except Exception:  # noqa: BLE001 - rollup degrades to claims-only rows
        pass

    lanes = []
    for s in sorted(list_claims(prefix=LANE_SLOT_PREFIX), key=lambda c: c.get("key", "")):
        meta = s.get("metadata") or {}
        lane_id = meta.get("lane_id") or ""
        node = nodes.get(lane_id) or {}
        lanes.append(
            {
                "slot": s.get("key"),
                "lane_id": lane_id,
                "domain": meta.get("domain"),
                "slug": node.get("slug"),
                "status": node.get("status"),
                "pr_number": node.get("pr_number"),
                "holder": s.get("holder"),
            }
        )

    if json_output:
        typer.echo(json.dumps({"max_lanes": max_lanes, "active": len(lanes), "lanes": lanes}))
        return
    typer.echo(f"lanes: {len(lanes)}/{max_lanes} active")
    for ln in lanes:
        slug = f"  {ln['slug']}" if ln.get("slug") else ""
        pr = f"  pr#{ln['pr_number']}" if ln.get("pr_number") else ""
        typer.echo(
            f"{ln['slot']}  {ln['lane_id']}{slug}  "
            f"domain={ln.get('domain') or '-'}  {ln.get('status') or '-'}{pr}"
        )


# -- board --

_BOARD_SECTIONS = (
    ("just_finished", "Just finished"),
    ("in_progress", "In progress"),
    ("on_deck", "On deck"),
)


def _board_unreadable_graph_payload(reason: str) -> dict:
    return {key: {"rows": [], "more": 0, "note": None, "unknown": reason} for key, _ in _BOARD_SECTIONS}


def _print_board(board: dict) -> None:
    for key, title in _BOARD_SECTIONS:
        section = board.get(key) or {}
        typer.echo(title)
        unknown = section.get("unknown")
        if unknown:
            typer.echo(f"  (unknown: {unknown})")
            typer.echo("")
            continue
        note = section.get("note")
        if note:
            typer.echo(f"  ({note})")
        rows = section.get("rows") or []
        if not rows:
            typer.echo("  (none)")
        for row in rows:
            typer.echo(f"  {row['id']}  {row['title']}   {row['fact']}")
        more = section.get("more") or 0
        if more:
            typer.echo(f"  ... and {more} more")
        typer.echo("")


@cli.command("board", hidden=True)
def cmd_board(
    project: Optional[str] = typer.Option(
        None, "--project", help="Filter to one project; default the current repo's."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Same three sections, JSON, unknown markers included."
    ),
) -> None:
    """Just finished / In progress / On deck - the board in one glance.

    Reads only the graph and the on-disk pr-status cache, never GitHub: a
    verb whose job is showing the board must never be the thing that
    exhausts the quota. An unreadable source renders as an explicit
    unknown, never as an empty section.
    """
    from fno.graph.board import compute_board
    from fno.graph.store import GraphUnreadableError, read_graph_strict
    from fno.tracker import active_backend_name

    # The board spans done + in-progress + ready sections, which needs
    # lookback past list_open()'s open-only contract (the same done-at-
    # PR-green grace window pr_watch's discovery needs and cannot get from
    # the tracker seam without storage-engine work, out of scope here). An
    # external backend degrades the same way an unreadable graph already
    # does, rather than reading the wrong store.
    if active_backend_name() != "graph":
        payload = _board_unreadable_graph_payload(
            f"board is unavailable under the {active_backend_name()} tracker backend"
        )
        if json_output:
            typer.echo(json.dumps(payload))
        else:
            _print_board(payload)
        raise typer.Exit(code=1)

    try:
        entries = read_graph_strict(_graph_path())
    except GraphUnreadableError as exc:
        payload = _board_unreadable_graph_payload(f"graph unreadable ({exc})")
        if json_output:
            typer.echo(json.dumps(payload))
        else:
            _print_board(payload)
        raise typer.Exit(code=1)

    proj = project
    if proj is None:
        from fno.graph._intake import detect_project

        proj = detect_project(entries)

    board = compute_board(entries, project=proj)
    if json_output:
        typer.echo(json.dumps(board))
        return
    _print_board(board)


# -- get --

def _read_time_status_external(
    state: str, pr_number: Optional[int], plan_path: Optional[str]
) -> str:
    """Read-time rung for the external get render - never stored, derived on
    every read from tracker state plus sidecar evidence."""
    if state == "closed":
        return "done"
    return _external_open_status(pr_number=pr_number, plan_path=plan_path)


def _render_external_get(id: str, field: Optional[str]) -> None:
    """`backlog get` under an external backend: exact-id tracker read plus the
    sidecar, joined FOR DISPLAY ONLY (a render, not a stored convenience
    record). Byte-compatibility binds the graph mode above, not this branch."""
    from fno.tracker.types import NodeNotFound

    try:
        node, sc = _read_external_node_and_sidecar(id)
    except NodeNotFound:
        typer.echo(
            f"fno backlog get: no node matches '{id}' "
            "(an external backend resolves exact ids; slug/bare-hex are "
            "footnote-minted)",
            err=True,
        )
        raise typer.Exit(code=1)
    state = str(node.state.value)
    joined: dict = {
        "id": node.id,
        "title": node.title,
        "state": state,
        "status": _read_time_status_external(state, sc.pr_number, sc.plan_path),
        "parent": node.parent,
        "blocked_by": list(node.blocked_by),
    }
    joined.update(sc.model_dump(exclude_unset=True, exclude={"id"}))
    joined["_resolved_cwd"] = sc.cwd

    if field:
        value = joined.get(field)
        if value is None:
            typer.echo("null")
        elif isinstance(value, (list, dict)):
            typer.echo(json.dumps(value))
        else:
            typer.echo(value)
        return
    typer.echo(json.dumps(joined, indent=2))


@cli.command("get")
def cmd_get(
    id: str = typer.Argument(
        ...,
        help="Node ab-id, slug, or bare 8-hex (e.g. ab-ff6f96e0 | dashless-spawn | ff6f96e0)",
    ),
    field: Optional[str] = typer.Option(None, help="Print only this field"),
    strict: bool = typer.Option(
        False,
        "--strict",
        help="Exact-only resolution (id/slug/bare-hex); never fuzzy. The stable "
        "surface the /think router seeds a design from - a miss exits 1 so a "
        "typo'd token can never silently seed.",
    ),
) -> None:
    from fno.graph.fuzzy import resolve_node
    from fno.tracker import active_backend_name

    # Pre-rename spelling; shell consumers outside this repo still pass it.
    if field == "_status":
        field = "status"

    # Backend-neutral read: an external tracker resolves the OPAQUE id exactly
    # (never through the local <prefix>-<hex> grammar) and displays the five
    # tracker fields plus the sidecar. Slug/bare-hex tiers are footnote-minted
    # metadata and refuse with the backend named rather than reporting absent.
    if active_backend_name() != "graph":
        _render_external_get(id, field)
        return

    # Strict read: exit 1 stays "read cleanly, node absent"; an unreadable graph
    # gets GRAPH_UNREADABLE_EXIT so a caller cannot mistake it for an absent node.
    entries = _resolve_entries_or_exit(id)
    # Deterministic resolution tiers 1-3 (ab-f82e8083): exact ab-id, exact slug,
    # bare-8-hex re-prefix. A slug/bare-hex argument resolves to the same node
    # an ab-id would, so the spawn VALIDATE step (`fno backlog get "$node"`)
    # accepts every exact entry form. `resolve_node` is already exact-only, so
    # --strict pins that contract for the router (x-4af4): should `get` ever gain
    # a describe-it fuzzy default, --strict stays the exact-only seed path.
    match = resolve_node(id, entries)
    if match.kind == "exact":
        e = match.candidates[0]
        from fno.graph._intake import project_root_from_settings
        root = project_root_from_settings(e["project"]) if e.get("project") else None
        e["_resolved_cwd"] = root or e.get("cwd")
        if field:
            value = e.get(field)
            if value is None:
                typer.echo("null")
            elif isinstance(value, (list, dict)):
                typer.echo(json.dumps(value))
            else:
                typer.echo(value)
        else:
            typer.echo(json.dumps(e, indent=2))
        return

    # Read-through fallback: a node the sweep archived still resolves here
    # (read-only). Mutating verbs stay working-graph-only and error instead.
    from fno.paths import graph_archive_json
    from fno.graph.store import read_graph

    archive_path = graph_archive_json()
    if archive_path.exists():
        # Best-effort, read-only: an unreadable archive degrades to the final
        # miss below rather than erroring, so keep the soft reader here.
        archived = read_graph(archive_path)
        amatch = resolve_node(id, archived)
        matched_entry = amatch.candidates[0] if amatch.kind == "exact" else None
        if matched_entry is None:
            # x-f69b: an archive-side id collision with the working graph gets
            # reminted, and the old id is kept as `previous_id` so a reference
            # made before the remint (a doc, a mail thread, a stale branch
            # name) still resolves instead of reading as a plain miss.
            matched_entry = next(
                (e for e in archived if isinstance(e, dict) and e.get("previous_id") == id),
                None,
            )
        if matched_entry is not None:
            e = dict(matched_entry)
            e["_archived"] = True
            if field:
                value = e.get(field)
                if value is None:
                    typer.echo("null")
                elif isinstance(value, (list, dict)):
                    typer.echo(json.dumps(value))
                else:
                    typer.echo(value)
            else:
                typer.echo(json.dumps(e, indent=2))
            return

    typer.echo(f"No node matching '{id}' (id/slug/bare-hex) in {_graph_path()}", err=True)
    raise typer.Exit(code=1)


# -- project-root (work-map resolution; null-for-unmapped) --

@cli.command("project-root", hidden=True)
def cmd_project_root(
    project: str = typer.Argument(..., help="Project name to resolve against config.work.workspaces."),
) -> None:
    """Print a project's work-map root, or exit 1 (empty stdout) if unmapped.

    The G2 session-project invariant needs to tell "mapped to a root" apart from
    "unmapped" so it can REFUSE an unmapped foreign wave by name rather than
    guess a cwd (AC2-ERR). ``backlog get --field _resolved_cwd`` can't answer
    this: it applies a ``root or cwd`` fallback, so an unmapped project with a
    recorded cwd still prints a (guessed) path. This verb exposes the raw
    ``project_root_from_settings`` lookup - the same pure work-map resolver G1
    uses - with no cwd fallback, so empty/exit-1 means exactly "unmapped".
    """
    from fno.graph._intake import project_root_from_settings

    root = project_root_from_settings(project)
    if not root:
        raise typer.Exit(code=1)
    typer.echo(root)


# -- provenance --

# A cycle in source_node_id needs a visited set to terminate; the cap is the
# second belt, and bounds output on a legitimately deep chain. Not configurable:
# a follow-up chain this long is a graph problem, not a display preference.
_SPAWNED_MAX_DEPTH = 10


def _spawned_walk(
    entries: list, root_id: str, *, max_depth: int = _SPAWNED_MAX_DEPTH
) -> "tuple[list, bool, bool]":
    """Walk source_node_id in reverse: what did ``root_id`` produce?

    Breadth-first so depth falls out of the traversal rather than being assigned,
    and so the shortest path to a node is the depth reported for it.

    Returns ``(rows, cycle_detected, truncated)`` where rows are
    ``(depth, entry)``. A cycle TRUNCATES the walk and keeps the descendants
    already found - returning an empty set with a cycle flag would satisfy a
    naive reading of "terminates" while silently discarding the answer.
    """
    from fno.graph.rollup import origin_index

    by_source = origin_index(entries)
    rows: list = []
    seen = {root_id}
    frontier = [root_id]
    cycle = False
    depth = 0
    while frontier and depth < max_depth:
        depth += 1
        nxt: list = []
        for parent_id in frontier:
            for child in sorted(by_source.get(parent_id, []), key=lambda e: e.get("id") or ""):
                child_id = child.get("id")
                if not isinstance(child_id, str):
                    continue
                if child_id in seen:
                    cycle = True
                    continue
                seen.add(child_id)
                rows.append((depth, child))
                nxt.append(child_id)
        frontier = nxt
    # Truncated only if something was actually cut: a chain ending exactly at
    # the cap leaves a non-empty frontier with nothing below it.
    truncated = any(
        child.get("id") not in seen
        for parent_id in frontier
        for child in by_source.get(parent_id, [])
    )
    return rows, cycle, truncated


_LIFECYCLE_PHASES = ("think", "blueprint", "do", "review", "ship")


def _lifecycle_roster(sessions: list) -> "tuple[list[str], dict]":
    """Per-phase lifecycle roster: start, end, duration per row, and an honest
    node total. Returns ``(human_lines, summary_dict)``.

    Honesty is the acceptance criterion (node x-015c): a phase with no row
    renders 'not recorded'; a row with an end but no start renders 'end only';
    neither renders as a duration, and the total states how many of the
    lifecycle phases contributed a duration rather than summing silently over
    gaps. Start reads ``started_at`` (canonical) with ``claimed_at`` as the
    legacy fallback.
    """
    from datetime import datetime

    by_phase: "dict[str, list[dict]]" = {p: [] for p in _LIFECYCLE_PHASES}
    for s in sessions or []:
        ph = s.get("phase") if isinstance(s, dict) else None
        if ph in by_phase:
            by_phase[ph].append(s)

    def _start(row: dict) -> "str | None":
        return row.get("started_at") or row.get("claimed_at")

    def _end(row: dict) -> "str | None":
        return row.get("ended_at") or row.get("at")

    def _honest(row: dict) -> bool:
        # A duration is honest only when both CANONICAL names are present.
        # Legacy rows (claimed_at/at) hold stamp-fire time, not phase boundaries
        # - their span is the whole session - so they render 'end only' and are
        # never summed, named, or displayed as a phase duration.
        return "started_at" in row and "ended_at" in row

    def _dur(row: dict) -> "float | None":
        if not _honest(row):
            return None
        try:
            sp = datetime.fromisoformat(row["started_at"].replace("Z", "+00:00"))
            ep = datetime.fromisoformat(row["ended_at"].replace("Z", "+00:00"))
        except (ValueError, AttributeError):
            return None
        sec = (ep - sp).total_seconds()
        # An inverted window (started_at after ended_at) is a backfill typo or
        # a clock skew, not a phase duration. Render it as end-only rather than
        # summing a negative span into the node total.
        if sec < 0:
            return None
        return sec

    def _fmt(sec: float) -> str:
        sec = int(round(sec))
        if sec < 60:
            return f"{sec}s"
        if sec < 3600:
            return f"{sec // 60}m"
        return f"{sec // 3600}h{(sec % 3600) // 60}m"

    lines: list[str] = []
    phases: list[dict] = []
    total = 0.0
    phases_with_window = 0
    for ph in _LIFECYCLE_PHASES:
        rows = by_phase[ph]
        if not rows:
            lines.append(f"    {ph:<9} not recorded")
            phases.append({"phase": ph, "recorded": False})
            continue
        phase_has_window = False
        for row in rows:
            st, en, dur = _start(row), _end(row), _dur(row)
            head = f"    {ph:<9} {row.get('harness', '?')}:{row.get('session_id', '?')}"
            if dur is not None:
                lines.append(f"{head} {row['started_at']} -> {row['ended_at']} ({_fmt(dur)})")
                total += dur
                phase_has_window = True
            elif en:
                lines.append(f"{head} end only @ {en}")
            elif st:
                lines.append(f"{head} in progress (since {st})")
            else:
                lines.append(head.rstrip())
            phases.append({
                "phase": ph, "recorded": True,
                "harness": row.get("harness"), "session_id": row.get("session_id"),
                "start": st, "end": en,
                "duration_seconds": dur,
            })
        if phase_has_window:
            phases_with_window += 1

    # Predicate on phases_with_window, not total: a zero-second window
    # (started_at == ended_at) or any out-of-order stamp leaves total at 0
    # while a phase still contributed a window. Keying on total would drop
    # the duration line and report total_duration_seconds: null despite a
    # real recorded window.
    if phases_with_window > 0:
        lines.append(
            f"    total     {_fmt(total)} "
            f"({phases_with_window} of {len(_LIFECYCLE_PHASES)} phases recorded)"
        )
    else:
        lines.append(
            f"    total     {phases_with_window} of {len(_LIFECYCLE_PHASES)} phases recorded"
        )

    summary = {
        "phases": phases,
        "total_duration_seconds": total if phases_with_window > 0 else None,
        "phases_recorded": phases_with_window,
        "phases_total": len(_LIFECYCLE_PHASES),
    }
    return lines, summary


def _render_external_provenance(id: str, spawned: bool, json_out: bool) -> None:
    """`backlog provenance` under an external backend: exact-id tracker read
    plus the sidecar provenance edges (the AC2 provenance path). Footnote-minted
    extras the sidecar does not carry (``related``) render empty rather than
    from stale local rows."""
    import dataclasses

    from fno.provenance.resolver import resolve_transcript, _DEFAULT_PROJECTS_ROOT
    from fno.tracker import get_tracker
    from fno.tracker import sidecar as sidecar_store
    from fno.tracker.types import NodeNotFound

    try:
        node, sc = _read_external_node_and_sidecar(id)
    except NodeNotFound:
        typer.echo(f"No node matching '{id}' (external backend; exact ids)", err=True)
        raise typer.Exit(code=1)

    def _title_of(nid: Optional[str]) -> Optional[str]:
        if not nid:
            return None
        try:
            return get_tracker().read(nid).title
        except Exception:  # noqa: BLE001 - advisory title; absent is honest
            return None

    birth_result = (
        resolve_transcript(sc.source_harness, sc.source_session_id,
                           sc.source_cwd or sc.cwd,
                           projects_root=_DEFAULT_PROJECTS_ROOT)
        if sc.source_session_id else None
    )
    spawn_result = (
        resolve_transcript(sc.spawned_by_harness, sc.spawned_by_session,
                           sc.spawned_by_cwd,
                           projects_root=_DEFAULT_PROJECTS_ROOT)
        if sc.spawned_by_session else None
    )

    # Spawned walk over the sidecar origin index (source_node_id edges),
    # titles joined from the tracker.
    walk_rows: list = []
    if spawned:
        by_source: dict = {}
        for nid, other in sidecar_store.load_all().items():
            if other.source_node_id:
                by_source.setdefault(other.source_node_id, []).append(nid)
        seen = {node.id}
        frontier = [node.id]
        depth = 0
        while frontier and depth < _SPAWNED_MAX_DEPTH:
            depth += 1
            nxt = []
            for parent_id in frontier:
                for child_id in sorted(by_source.get(parent_id, [])):
                    if child_id in seen:
                        continue
                    seen.add(child_id)
                    walk_rows.append((depth, child_id))
                    nxt.append(child_id)
            frontier = nxt

    def _edge(label: str, result) -> dict:
        if result is None:
            return {"edge": label, "session_id": None, "resolved": False}
        d = dataclasses.asdict(result)
        d["edge"] = label
        return d

    if json_out:
        output: dict[str, Any] = {
            "node_id": node.id,
            "title": node.title,
            "edges": [_edge("node_birth", birth_result), _edge("spawn", spawn_result)],
            "sessions": sc.sessions,
            "lifecycle": None,  # roster derives from sc.sessions; kept for shape parity
            "source_node_id": sc.source_node_id,
            "source_node_title": _title_of(sc.source_node_id),
            "source_plan_path": sc.source_plan_path,
            "related": [],  # footnote-minted; unavailable under an external backend
        }
        if spawned:
            output["spawned"] = {
                "nodes": [
                    {"depth": d, "id": nid, "title": _title_of(nid)}
                    for d, nid in walk_rows
                ],
                "cycle_detected": False,
                "truncated_at_depth": None,
            }
        typer.echo(json.dumps(output, indent=2))
        return
    typer.echo(f"provenance for {node.id}: {node.title or ''}")
    typer.echo(f"  node_birth: {sc.source_session_id or '(none)'}")
    typer.echo(f"  spawn: {sc.spawned_by_session or '(none)'}")
    typer.echo(f"  sessions: {len(sc.sessions)} row(s)")
    if spawned:
        for d, nid in walk_rows:
            typer.echo(f"  spawned d{d}: {nid} ({_title_of(nid) or ''})")


@cli.command("provenance", hidden=True)
def cmd_provenance(
    id: str = typer.Argument(
        ...,
        help="Node ab-id, slug, or bare 8-hex",
    ),
    spawned: bool = typer.Option(
        False,
        "--spawned",
        help="Also walk the origin edge in reverse: every node this one produced, transitively.",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit machine-readable JSON instead of human summary"
    ),
) -> None:
    """Show provenance pointers for a node and resolve transcripts where possible.

    Reads two provenance edges stored on the node:

      node-birth edge  source_session_id + source_harness + source_cwd
      spawn edge       spawned_by_session + spawned_by_harness + spawned_by_cwd

    For each edge that carries a session id the resolver is run (claude only;
    codex/gemini/etc. return resolved=False). Read-only: no graph mutation.
    """
    from fno.graph.fuzzy import resolve_node
    from fno.provenance.resolver import resolve_transcript, _DEFAULT_PROJECTS_ROOT
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        _render_external_provenance(id, spawned, json_out)
        return

    # Strict read for the same reason cmd_get uses it: a wedged graph must not
    # read as "No node matching", which asserts the node is absent.
    entries = _resolve_entries_or_exit(id)
    match = resolve_node(id, entries)
    if match.kind != "exact":
        typer.echo(f"No node matching '{id}' in {_graph_path()}", err=True)
        raise typer.Exit(code=1)

    e = match.candidates[0]
    node_id = e["id"]

    # node-birth edge: resolve against the originating SESSION cwd
    # (source_cwd), NOT the node's durable project `cwd`. Claude transcript dirs
    # are slugged by the session cwd, so a node filed from a worktree resolves
    # only via source_cwd; fall back to `cwd` for legacy pre-source_cwd nodes.
    birth_session = e.get("source_session_id")
    birth_harness = e.get("source_harness")
    birth_cwd = e.get("source_cwd") or e.get("cwd")
    birth_result = None
    if birth_session:
        birth_result = resolve_transcript(
            birth_harness, birth_session, birth_cwd,
            projects_root=_DEFAULT_PROJECTS_ROOT,
        )

    # spawn edge: uses spawned_by_cwd
    spawn_session = e.get("spawned_by_session")
    spawn_harness = e.get("spawned_by_harness")
    spawn_cwd = e.get("spawned_by_cwd")
    spawn_result = None
    if spawn_session:
        spawn_result = resolve_transcript(
            spawn_harness, spawn_session, spawn_cwd,
            projects_root=_DEFAULT_PROJECTS_ROOT,
        )

    index = {e.get("id"): e for e in entries if isinstance(e, dict)}

    def _titled(other_id: str) -> str:
        """`<id> (<title>)`, so the line reads without a second lookup."""
        other = index.get(other_id)
        if other is None:
            return f"{other_id} (not in graph)"
        return f"{other_id} ({other.get('title', '')})"

    origin_id = e.get("source_node_id")
    related_ids = e.get("related") or []
    walk_rows, walk_cycle, walk_truncated = (
        _spawned_walk(entries, node_id) if spawned else ([], False, False)
    )

    # Runtime-attempt projection (x-2ccd wave 3): live/suspect/stale/interrupted
    # attempts read off manifests + the claim, alongside the confirmed lifecycle
    # rows below. Never a confirmed `do` row; never a graph mutation.
    from fno.provenance.runtime_attempts import runtime_attempts

    runtime = runtime_attempts(node_id, e)

    # Lifecycle roster (x-015c): per-phase start/end/duration + an honest total,
    # computed once for both the JSON and human paths.
    roster_lines, roster_summary = _lifecycle_roster(e["sessions"])

    if json_out:
        import dataclasses

        def _edge(label: str, result) -> dict:
            if result is None:
                return {"edge": label, "session_id": None, "resolved": False}
            d = dataclasses.asdict(result)
            d["edge"] = label
            return d

        output = {
            "node_id": node_id,
            "title": e.get("title"),
            "edges": [
                _edge("node_birth", birth_result),
                _edge("spawn", spawn_result),
            ],
            # Append-only lifecycle provenance in raw append order (x-b6e4).
            # read_graph's defaults guarantee the key, so no fallback guard.
            "sessions": e["sessions"],
            # Per-phase roster with starts, durations, and an honest total
            # (absent values are null, never 0). See _lifecycle_roster.
            "lifecycle": roster_summary,
            "source_node_id": origin_id,
            "source_node_title": (index.get(origin_id) or {}).get("title"),
            "source_plan_path": e.get("source_plan_path"),
            "related": related_ids,
            "runtime_attempts": runtime,
        }
        if spawned:
            output["spawned"] = {
                "nodes": [
                    {"depth": d, "id": n.get("id"), "title": n.get("title")}
                    for d, n in walk_rows
                ],
                "cycle_detected": walk_cycle,
                "truncated_at_depth": _SPAWNED_MAX_DEPTH if walk_truncated else None,
            }
        typer.echo(json.dumps(output, indent=2))
        return

    # Human-readable summary
    lines = [f"provenance for {node_id}: {e.get('title', '')}"]

    def _fmt_edge(label: str, result, session: Optional[str], harness: Optional[str]) -> None:
        if session is None:
            lines.append(f"  {label}: (none)")
            return
        lines.append(f"  {label}:")
        lines.append(f"    session:  {session}")
        lines.append(f"    harness:  {harness or '(unknown)'}")
        if result is None:
            lines.append("    transcript: (not resolved)")
        elif result.resolved:
            ambig = " [ambiguous match]" if result.ambiguous else ""
            lines.append(f"    transcript: {result.transcript_path}{ambig}")
        else:
            reason = result.reason or "not-found"
            lines.append(f"    transcript: (unresolved - {reason})")

    _fmt_edge("node-birth", birth_result, birth_session, birth_harness)
    # Rendered even when null: an omitted line reads as "this verb does not
    # report origins", which is how the field stayed invisible for a month.
    lines.append(f"  origin: {_titled(origin_id) if origin_id else '(none)'}")
    if e.get("source_plan_path"):
        lines.append(f"    plan: {e['source_plan_path']}")
    _fmt_edge("spawn", spawn_result, spawn_session, spawn_harness)

    lines.append(f"  related: {'(none)' if not related_ids else ''}".rstrip())
    for rid in related_ids:
        lines.append(f"    {_titled(rid)}")

    if spawned:
        lines.append(f"  spawned: {'(none)' if not walk_rows else ''}".rstrip())
        for depth, n in walk_rows:
            lines.append(f"    {'  ' * (depth - 1)}d{depth} {_titled(n.get('id'))}")
        if walk_cycle:
            lines.append("    note: cycle detected; walk truncated at the repeat")
        if walk_truncated:
            lines.append(f"    note: depth cap {_SPAWNED_MAX_DEPTH} reached; walk truncated")

    # Runtime attempts (x-2ccd wave 3): live/suspect/stale/interrupted attempts
    # projected from manifests + the claim. Rendered BEFORE lifecycle so an
    # operator sees the current/interrupted state first; explicitly labeled
    # unconfirmed so it is never mistaken for a confirmed `do` lifecycle row.
    if runtime:
        lines.append("  runtime:")
        for a in runtime:
            work_bits = []
            if a.get("commits_ahead"):
                work_bits.append(f"{a['commits_ahead']} commits")
            if a.get("pr_number"):
                work_bits.append(f"PR #{a['pr_number']}")
            work = ", ".join(work_bits) or "(no work evidence)"
            lines.append(
                f"    {a['attempt_state']:<11} {a.get('harness', '?')}:{a.get('harness_session_id', '?')}"
            )
            lines.append(f"      run:       {a.get('fno_id', '?')}")
            lines.append(f"      worktree:  {a.get('worktree', '?')}")
            lines.append(f"      claim:     {a.get('claim_state', '?')} pid={a.get('claim_pid')}")
            lines.append(f"      work:      {work}")
            lines.append(f"      lifecycle: {a.get('lifecycle', '?')}")

    # Lifecycle roster (x-b6e4, x-015c): per-phase start/end/duration + an
    # honest total. Distinct from the birth/spawn edges above -- those are
    # single parent pointers; this is the per-phase who-did-what across sessions
    # and harnesses. Every lifecycle phase always renders (not recorded / end
    # only for gaps) so a roster never reads "nobody touched this" by omission.
    lines.append("  lifecycle:")
    lines.extend(roster_lines)

    typer.echo("\n".join(lines))


# -- session add (lifecycle provenance, x-b6e4) --

session_app = typer.Typer(
    name="session",
    help="Append-only lifecycle session provenance (x-b6e4).",
    no_args_is_help=True,
    add_completion=False,
)


def _plan_claims(plan_path: str) -> "set[str]":
    """Delegate to the single parser (``_intake.plan_claims``).

    Kept as a thin alias because the stamp-guard call site reads better with
    a local name; the implementation lives in one place so this reader and
    collision's self-exclusion default cannot diverge.
    """
    from fno.graph._intake import plan_claims

    return plan_claims(plan_path)


@session_app.callback()
def _session_callback() -> None:
    """Keep ``add`` a real subcommand (a single-command Typer app auto-collapses,
    which would parse ``session add <node>`` with ``add`` as the node)."""


@session_app.command("add")
def cmd_session_add(
    node: Optional[str] = typer.Argument(
        None, help="Node id / slug / bare-hex to stamp (mutually exclusive with --pr-number)."
    ),
    phase: str = typer.Option(
        ..., "--phase", help="Lifecycle phase: think|blueprint|do|review|ship."
    ),
    pr: Optional[int] = typer.Option(
        None, "--pr-number", help="Resolve the UNIQUE node carrying this PR number instead "
                                  "of passing NODE (rejects 0 or multiple matches; never fans out)."
    ),
    repo: Optional[str] = typer.Option(
        None, "--repo", help="Scope --pr-number resolution to an <owner>/<repo> slug "
                             "(pr_number is not unique across repos in a cross-project graph). "
                             "Omit and the verb resolves the current checkout's slug itself."
    ),
    harness: Optional[str] = typer.Option(
        None, "--harness", help="Override harness (default: ambient session identity)."
    ),
    session_id: Optional[str] = typer.Option(
        None, "--session-id", help="Override session id (default: ambient session identity)."
    ),
    ended_at: Optional[str] = typer.Option(
        None, "--ended-at", "--at",
        help="ISO-8601 UTC instant the phase ended. Omit when there is no honest end "
              "to record (a row opened mid-session); explicit for backfill of completed work."
    ),
    started_at: Optional[str] = typer.Option(
        None, "--started-at", "--claimed-at",
        help="ISO-8601 UTC instant the work began; lands on the "
             "row so it bounds the window with --ended-at. Honest for every "
             "phase (a think row starts but claims nothing)."
    ),
    require_session: Optional[str] = typer.Option(
        None, "--require-session", help="Skip (exit 0) unless the ambient session id equals "
                                        "this. Identity-continuity guard for stale manifests."
    ),
    guard_plan: Optional[str] = typer.Option(
        None, "--guard-plan", help="Skip (exit 0) if this plan's frontmatter `claims:` names "
                                   "a DIFFERENT node. Requires NODE (not --pr-number)."
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the result as JSON."),
) -> None:
    """Stamp a node with a lifecycle phase record (idempotent, append-only).

    Identify the node by NODE (id/slug/hex) or by ``--pr-number <n>`` (the unique
    PR-linked node) -- exactly one of the two. With ``--pr-number`` and no
    ``--repo`` the verb resolves the current checkout's slug itself, so a caller
    needs no conditional flag (x-f47f). Harness + session id default to the
    ambient session identity; with neither an env marker nor an explicit flag the
    stamp is skipped and a warning names the node/PR and phase (provenance is
    never invented, AC2-ERR). Exit 0 on append or duplicate; exit 2 on missing
    identity, an unresolvable NODE, unknown phase, or bad input. A ``--pr-number``
    that maps to zero or several nodes is a best-effort SKIP, not an error: it
    warns (naming the candidates) and exits 0, because refusing to guess is the
    designed outcome and a caller must not log it as a failure (x-f47f AC3-ERR).

    ``--require-session`` and ``--guard-plan`` are the honesty guards an
    unattended caller (finalize's do-provenance backstop, x-0469) needs: a stale
    manifest in a reused worktree or a plan claiming another node must not
    mis-attribute work on an append-only record. Both skip with exit 0 and one
    named reason, because a guard skip is a designed outcome the caller must not
    log as a failure.
    """
    from fno.graph.fuzzy import resolve_node
    from fno.graph.store import (
        append_session_record,
        find_nodes_for_pr,
        read_graph,
        stamp_session_for_pr,
    )

    if (node is None) == (pr is None):
        typer.echo("session add: pass exactly one of NODE or --pr-number.", err=True)
        raise typer.Exit(code=2)
    # Refused, never ignored: the guard compares against the node the row lands
    # on, and the --pr-number path resolves that only after it has stamped.
    if guard_plan is not None and pr is not None:
        typer.echo("session add: --guard-plan requires NODE, not --pr-number.", err=True)
        raise typer.Exit(code=2)

    who = node if node is not None else f"pr#{pr}"

    def _skip(reason: str, node_id: "str | None" = None) -> None:
        typer.echo(f"session add: {reason} (target={who} phase={phase}). Skipped.", err=True)
        if json_out:
            typer.echo(json.dumps({
                "node_id": node_id, "status": "skipped", "reason": reason,
                "phase": phase, "harness": eff_harness, "session_id": eff_session,
                "added": False,
            }))

    from fno.claims.self_identity import resolve_self_identity

    ident = resolve_self_identity()
    eff_harness = (harness or ident.harness or "").strip()
    eff_session = (session_id or ident.session_id or "").strip()
    if not eff_harness or not eff_session:
        typer.echo(
            f"session add: no ambient identity for {who} phase={phase}; "
            "pass --harness/--session-id or run inside a session. Skipped.",
            err=True,
        )
        raise typer.Exit(code=2)

    # Identity continuity: the caller vouches for whose session this manifest
    # belongs to; a mismatch means it belongs to a different conversation (the
    # stale-manifest squatter), so the record is not this session's to write.
    # Compared against the AMBIENT id, never the --session-id override: a guard a
    # caller can satisfy by asserting its own answer is not a guard. No ambient
    # identity at all therefore also skips - continuity is unprovable.
    if require_session is not None:
        # The row must record the identity the guard actually checked. Allowing an
        # override would verify one identity and permanently write another, which
        # is the same self-certification hole in a different shape. Refused rather
        # than ignored, like --guard-plan with --pr-number.
        if session_id is not None or harness is not None:
            typer.echo(
                "session add: --require-session cannot be combined with "
                "--session-id/--harness (it would verify one identity and "
                "record another).",
                err=True,
            )
            raise typer.Exit(code=2)
        ambient = (ident.session_id or "").strip()
        if ambient != require_session.strip():
            return _skip(
                f"ambient session {ambient!r} != required {require_session.strip()!r}"
            )

    # After the identity guard: resolution shells out to git and possibly gh, and
    # a run with no identity is about to skip anyway.
    if pr is not None and repo is None:
        from fno.graph._reconcile import resolve_current_repo_slug

        repo = resolve_current_repo_slug()
        if repo is None:
            typer.echo(
                f"session add: could not resolve this checkout's repo slug for pr#{pr}; "
                "matching on the bare PR number (skips on cross-repo ambiguity).",
                err=True,
            )
        # No bare-number fallback once a slug resolves. The graph is GLOBAL and
        # cross-project, so a url-less node is unattributable to ANY repo - a
        # fallback cannot tell "this repo's legacy node" from "another project's
        # legacy node with the same PR number", and stamping the latter is the
        # wrong-node write repo scoping exists to prevent. Refusing to guess
        # costs a stamp on a legacy node; guessing costs a corrupted one, and
        # the skip is now LOUD (it names the candidates), so nothing is silent.

    try:
        if pr is not None:
            node_id, status = stamp_session_for_pr(
                _graph_path(), pr, phase=phase,
                harness=eff_harness, session_id=eff_session, ended_at=ended_at,
                started_at=started_at, repo=repo,
            )
            if status in ("no-node", "ambiguous"):
                cands = find_nodes_for_pr(_graph_path(), pr, repo=repo)
                detail = f" (candidates: {', '.join(cands)})" if cands else ""
                repair = ""
                if status == "no-node":
                    # Resolution matches the node's STORED pr_number, so a node
                    # whose PR was never stamped (typical of a session killed
                    # before ship) is invisible - the exact state the repair
                    # path is needed in. Name the two ways out instead of
                    # leaving the operator stuck; a branch guess would risk
                    # stamping the wrong node, so refuse and explain.
                    repair = (
                        " A node whose PR was never stamped is invisible here. "
                        "Link it with `fno backlog update <node-id> "
                        f"--pr-number {pr}`, or pass the node id directly: "
                        f"`fno backlog session add <node-id> --phase {phase}`."
                    )
                typer.echo(
                    f"session add: PR {pr} maps to {status}{detail} (phase={phase}); "
                    f"resolution is exact and never fans out.{repair} Skipped.",
                    err=True,
                )
                if json_out:
                    typer.echo(json.dumps({
                        "node_id": None, "status": status, "phase": phase,
                        "harness": eff_harness, "session_id": eff_session,
                        "added": False, "candidates": cands,
                    }))
                return
            added = status == "added"
        else:
            # session add is a mutation verb: local-store resolution, guarded
            # against external backends by the shared refusal (task 4.2), not
            # the display-reader seam.
            match = resolve_node(node, read_graph(_graph_path()))
            if match.kind != "exact":
                typer.echo(f"session add: no node matches {node!r} (phase={phase}).", err=True)
                raise typer.Exit(code=2)
            node_id = match.candidates[0]["id"]
            # Plan agreement (mirrors /execute Step 1.5): only a POSITIVE disagreement
            # skips. An unreadable plan or an absent `claims:` is agreement-
            # unknown, and absent evidence of conflict is not conflict.
            #
            # This is the one guard that does NOT fail closed, so it says so out
            # loud when it could not evaluate. Otherwise an install whose
            # plan_path is systematically stale (plan moved, vault unmounted)
            # runs with G3 disabled and no operator signal anywhere.
            if guard_plan is not None:
                claims = _plan_claims(guard_plan)
                if not claims:
                    typer.echo(
                        f"session add: plan {guard_plan} is unreadable or declares no "
                        f"claims; agreement not evaluated for {node_id}.",
                        err=True,
                    )
                elif node_id not in claims:
                    return _skip(
                        f"plan {guard_plan} claims {sorted(claims)} != node {node_id}",
                        node_id=node_id,
                    )
            found, added = append_session_record(
                _graph_path(), node_id, phase=phase,
                harness=eff_harness, session_id=eff_session, ended_at=ended_at,
                started_at=started_at,
            )
            if not found:
                typer.echo(f"session add: node {node_id} not found (phase={phase}).", err=True)
                raise typer.Exit(code=2)
    except ValueError as exc:
        typer.echo(f"session add: {exc} (target={who} phase={phase})", err=True)
        raise typer.Exit(code=2)

    if json_out:
        typer.echo(json.dumps({
            "node_id": node_id, "status": "added" if added else "duplicate",
            "phase": phase, "harness": eff_harness,
            "session_id": eff_session, "added": added,
        }))
    else:
        state = "recorded" if added else "already recorded"
        typer.echo(f"{state} {phase} {eff_harness}:{eff_session} on {node_id}")


@session_app.command("close")
def cmd_session_close(
    node: str = typer.Argument(..., help="Node id / slug / bare-hex."),
    summary: str = typer.Option(..., "--summary", help="Completion summary for the blueprint."),
    launch: str = typer.Option(..., "--launch", help="Exact launch command for the next phase."),
    harness: Optional[str] = typer.Option(None, "--harness"),
    session_id: Optional[str] = typer.Option(None, "--session-id"),
    started_at: Optional[str] = typer.Option(None, "--started-at"),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the completion receipt as JSON."),
) -> None:
    """Close the blueprint phase with one identity-guarded completion receipt.

    The close writes the blueprint lifecycle row with an honest end, then emits
    the summary and exact launch line. Missing identity is a hard refusal: the
    close cannot claim completion while leaving provenance unresolved.
    """
    from datetime import datetime, timezone

    from fno.claims.self_identity import resolve_self_identity
    from fno.graph.fuzzy import resolve_node
    from fno.graph.store import append_session_record, read_graph

    summary = summary.strip()
    launch = launch.strip()
    if not summary or not launch:
        typer.echo("session close: summary and launch must be non-empty.", err=True)
        raise typer.Exit(code=2)
    ident = resolve_self_identity()
    eff_harness = (harness or ident.harness or "").strip()
    eff_session = (session_id or ident.session_id or "").strip()
    if not eff_harness or not eff_session:
        typer.echo(
            f"session close: no ambient identity for {node}; "
            "pass --harness/--session-id or run inside a session.",
            err=True,
        )
        raise typer.Exit(code=2)
    match = resolve_node(node, read_graph(_graph_path()))
    if match.kind != "exact":
        typer.echo(f"session close: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    node_id = match.candidates[0]["id"]
    ended_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    try:
        found, added = append_session_record(
            _graph_path(), node_id, phase="blueprint", harness=eff_harness,
            session_id=eff_session, ended_at=ended_at, started_at=started_at,
        )
    except ValueError as exc:
        typer.echo(f"session close: {exc}", err=True)
        raise typer.Exit(code=2)
    if not found:
        typer.echo(f"session close: node {node_id} disappeared before close.", err=True)
        raise typer.Exit(code=2)
    receipt = {
        "node_id": node_id,
        "status": "closed",
        "phase": "blueprint",
        "harness": eff_harness,
        "session_id": eff_session,
        "summary": summary,
        "launch": launch,
        "ended_at": ended_at,
        "added": added,
    }
    if json_out:
        typer.echo(json.dumps(receipt))
    else:
        typer.echo(f"blueprint closed {node_id} ({eff_harness}:{eff_session})")
        typer.echo(f"summary: {summary}")
        typer.echo(f"launch: {launch}")


@session_app.command("reap-open")
def cmd_session_reap_open(
    node: str = typer.Argument(..., help="Node id / slug / bare-hex."),
    harness: str = typer.Option(..., "--harness", help="Harness owning the dead session."),
    session_id: str = typer.Option(..., "--session-id", help="Dead harness session id."),
    phase: str = typer.Option(
        "do",
        "--phase",
        help=(
            "Lifecycle phase of the open row. 'do' removes the row (it wedges "
            "node status); any other phase (a spawn-opened review row) fills "
            "ended_at and keeps the provenance; 'all' settles every open row "
            "carrying the identity (the death-cascade spelling)."
        ),
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit a structured receipt."),
) -> None:
    """Reap one exact open session row after the observer proves session death."""
    from fno.graph.fuzzy import resolve_node
    from fno.graph.statuses import is_open_do_row, is_open_phase_row
    from fno.graph.store import reap_open_session_record, read_graph
    from fno.graph.types import SESSION_PHASES

    entries = read_graph(_graph_path())
    match = resolve_node(node, entries)
    if match.kind != "exact":
        typer.echo(f"session reap-open: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    node_id = match.candidates[0]["id"]
    try:
        receipt = reap_open_session_record(
            _graph_path(), node_id, phase=phase, harness=harness, session_id=session_id
        )
    except (ValueError, OSError, RuntimeError) as exc:
        typer.echo(f"session reap-open: {exc}", err=True)
        raise typer.Exit(code=2)

    reread = read_graph(_graph_path())
    rebound = next((entry for entry in reread if entry.get("id") == node_id), None)
    if rebound is None:
        typer.echo(f"session reap-open: node {node_id} disappeared on read-back.", err=True)
        raise typer.Exit(code=1)
    rows = rebound.get("sessions") or []
    want_phases = sorted(SESSION_PHASES) if phase == "all" else [phase]
    matching_open = any(
        any(is_open_phase_row(row, ph) for ph in want_phases)
        and (row.get("harness"), row.get("session_id")) == (harness.strip(), session_id.strip())
        for row in rows
    )
    remaining = sum(is_open_do_row(row) for row in rows)
    higher_precedence = any(
        rebound.get(field)
        for field in ("completed_at", "superseded_by", "deferred_at", "pr_number")
    ) or rebound.get("status") == "blocked"
    expected_in_progress = bool(rebound.get("locked_by")) or remaining > 0
    status_ok = higher_precedence or ((rebound.get("status") == "in_progress") == expected_in_progress)
    if matching_open or not status_ok:
        typer.echo(
            f"session reap-open: read-back did not settle {node_id} "
            f"(matching_open={matching_open}, status={rebound.get('status')!r}, "
            f"remaining_open_do={remaining}).",
            err=True,
        )
        raise typer.Exit(code=1)

    receipt.update({
        "node_id": node_id,
        "settled": True,
        "status_after": rebound.get("status"),
        "remaining_open_do": remaining,
    })
    if json_out:
        typer.echo(json.dumps(receipt, sort_keys=True))
    else:
        typer.echo(
            f"settled {node_id}: row_removed={receipt['row_removed']} "
            f"row_closed={receipt.get('row_closed')} "
            f"status={receipt['status_after']} remaining_open_do={remaining}"
        )


cli.add_typer(session_app, name="session", hidden=True)


# -- backfill-slugs --

# -- view --

@cli.command("view")
def cmd_view() -> None:
    """Render the backlog as HTML and open it with the system's default handler.

    Always rerenders before opening so the file reflects current graph.json
    state even if the auto-render hook hasn't fired since the last edit. The
    file lives at ``~/.fno/graph.html`` and is opened via ``open`` on
    macOS, ``xdg-open`` on Linux, ``os.startfile`` on Windows - whichever
    handler the OS has registered for ``.html`` takes over from there
    (browser, yazi, anything else).

    Set ``FNO_NO_OPEN=1`` to skip the launch step and just print the path -
    useful for scripts, CI, and tests.
    """
    import platform
    import shutil
    import subprocess

    from fno.graph._constants import GRAPH_HTML
    from fno.graph.render_html import render_graph_html

    render_graph_html(_display_entries("view"), GRAPH_HTML)
    typer.echo(str(GRAPH_HTML))

    if os.environ.get("FNO_NO_OPEN") == "1":
        return

    system = platform.system()
    try:
        if system == "Darwin":
            subprocess.run(["open", str(GRAPH_HTML)], check=False)
        elif system == "Windows":
            os.startfile(str(GRAPH_HTML))  # type: ignore[attr-defined]
        else:
            opener = shutil.which("xdg-open") or shutil.which("wslview")
            if opener:
                subprocess.run([opener, str(GRAPH_HTML)], check=False)
            else:
                typer.echo(
                    "No xdg-open / wslview found; file rendered but not opened.",
                    err=True,
                )
    except OSError as e:
        typer.echo(f"Could not launch opener: {e}", err=True)


# -- bases (canonical epic/mission progress Bases) --

@cli.command("bases", hidden=True)
def cmd_bases(
    out: Optional[str] = typer.Option(
        None,
        "--out",
        help="Directory to emit the .base files into (default: internal/fno/backlog/).",
    ),
) -> None:
    """Emit the canonical epic/mission progress Base files (x-6c2b).

    Regenerable: refreshes a file carrying the generated marker, refuses to
    clobber a hand-authored base (one prints `refused:`). Prints one line per
    file: written | unchanged | refused.
    """
    from fno.graph._bases import BASES, write_base
    from fno.graph._intake import repo_root

    out_dir = (
        Path(out) if out else Path(repo_root()) / "internal" / "fno" / "backlog"
    )
    for name, content in BASES.items():
        target = out_dir / name
        action = write_base(target, content)
        typer.echo(f"{action}: {target}")


# -- roadmap (public, curated) --

@cli.command("roadmap", hidden=True)
def cmd_roadmap(
    project: Optional[str] = typer.Option(
        None,
        "--project",
        help="Project to render (defaults to the project mapped to the cwd).",
    ),
    out: Optional[str] = typer.Option(None, "--out", help="Write markdown to this path instead of stdout."),
    html: Optional[str] = typer.Option(None, "--html", help="Also write a standalone HTML file to this path."),
) -> None:
    """Render a public, leak-free roadmap of `public`-flagged nodes.

    Only nodes flagged via `fno backlog update --public` for the given
    project appear, and only their title/priority/size - never IDs, plan
    paths, or cwd. Safe to commit to a public repo or host on a site.
    Grouped into Now / Next / Later / Shipped (reusing the live board's
    column + lane logic, so it can't drift).
    """
    from pathlib import Path

    from fno.graph._intake import detect_project_from_settings, repo_root
    from fno.graph.roadmap_public import (
        render_public_roadmap_html,
        render_public_roadmap_md,
    )

    resolved_project = project or detect_project_from_settings(repo_root())
    if not resolved_project:
        typer.echo(
            "Error: no project given and none mapped to the cwd; pass --project.",
            err=True,
        )
        raise typer.Exit(code=1)

    entries = _display_entries("roadmap")
    md = render_public_roadmap_md(entries, resolved_project)

    if out:
        Path(os.path.expanduser(out)).write_text(md)
        typer.echo(os.path.expanduser(out))
    else:
        typer.echo(md, nl=False)

    if html:
        html_path = os.path.expanduser(html)
        Path(html_path).write_text(render_public_roadmap_html(entries, resolved_project))
        typer.echo(html_path)


# -- tree --

# -- status --


# The stamp a closed-blocker tombstone carries in the live snapshot. Any
# non-empty string satisfies the consumer's has_stamp checks; a constant (not
# the real close time) keeps the tombstone honest about being a projection of
# "this dependency is satisfied", which is the only fact the consumer derives
# from it.
_SNAPSHOT_CLOSED_STAMP = "closed"


def _build_live_snapshot(tracker=None) -> dict:
    """The backend-neutral joined live view for non-Python consumers (the mux).

    Enumerates through ``list_open`` (bounded to the open set: a backend with
    thousands of historical rows is never materialized merely to render a
    queue), joins each id's sidecar, and emits exactly the live fields the
    Rust reader derives from. Readiness stays a derivation on the consumer
    side: open items carry their ``blocked_by`` ids, and a dependency that is
    already closed rides as a minimal tombstone row so the consumer's
    read-time blocked/ready logic (which fails closed on an unknown blocker)
    resolves it without footnote persisting any derived flag.
    """
    from fno.graph.slug import derive_base_slug
    from fno.tracker import get_tracker
    from fno.tracker import sidecar as sidecar_store

    tracker = tracker or get_tracker()
    try:
        candidates = tracker.list_open()
    except Exception as exc:  # noqa: BLE001 - name the backend, fail closed like selection
        raise _ExternalSelectionError(
            f"tracker {tracker.name!r} list_open failed: {exc}"
        ) from exc
    open_ids = {c.id for c in candidates}

    # Tombstones for closed dependencies referenced by open items. An
    # unresolvable blocker id is skipped: the consumer's own fail-closed rule
    # (unknown dep == blocked) is the correct outcome there, and this loop must
    # not invent an opinion about a backend read that errored.
    blocker_ids = {b for c in candidates for b in c.blocked_by} - open_ids
    tombstones: dict[str, dict] = {}
    for bid in sorted(blocker_ids):
        try:
            node = tracker.read(bid)
        except Exception:  # noqa: BLE001 - advisory resolution; consumer fails closed
            continue
        if str(node.state.value) == "closed":
            tombstones[bid] = {
                "id": bid,
                "status": "done",
                "completed_at": _SNAPSHOT_CLOSED_STAMP,
            }

    entries = []
    for c in candidates:
        sc = sidecar_store.load(c.id)
        entries.append(
            {
                "id": c.id,
                # Display handle; transient (graph mode's persistent slugs are
                # assigned by the store at write time, which a read-only
                # snapshot must not do).
                "slug": derive_base_slug(c.title) if c.title else "",
                "title": c.title,
                # Same three-way split _joined_open_candidates selects on:
                # a PR means in_review, else a plan means ready, else idea.
                # Computed here from evidence at read time, never stored.
                "status": _external_open_status(pr_number=sc.pr_number, plan_path=sc.plan_path),
                "priority": c.priority,
                "rank": c.rank,
                "created_at": c.created_at,
                "parent": c.parent,
                "blocked_by": list(c.blocked_by),
                "plan_path": sc.plan_path,
                "pr_number": sc.pr_number,
                "pr_url": sc.pr_url,
                "cwd": sc.cwd,
            }
        )
    entries.extend(tombstones.values())
    return {"backend": tracker.name, "entries": entries}


@cli.command("status", hidden=True)
def cmd_status(
    project: Optional[str] = typer.Option(None, help="Filter by project"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    snapshot: bool = typer.Option(
        False,
        "--snapshot",
        help=(
            "Internal: emit the backend-neutral joined live view as one JSON "
            "document. Consumed by the fno-agents mux reader when an external "
            "tracker backend is selected; the summary render below is the "
            "human surface."
        ),
    ),
) -> None:
    from fno.graph._intake import detect_project

    if snapshot:
        try:
            typer.echo(json.dumps(_build_live_snapshot(), indent=2))
        except _ExternalSelectionError as exc:
            typer.echo(f"backlog status --snapshot: {exc}", err=True)
            raise typer.Exit(code=1)
        return

    entries = _display_entries("status.summary")

    if not entries:
        typer.echo("No graph entries found. Run /megawalk vision.md to generate a roadmap.")
        return

    if roadmap_id:
        entries = [e for e in entries if e.get("roadmap_id") == roadmap_id]

    projects: dict[str, list]
    if project:
        projects = {project: [e for e in entries if e.get("project") == project]}
    elif all_:
        projects = {}
        for e in entries:
            proj = e.get("project") or "(no project)"
            projects.setdefault(proj, []).append(e)
    else:
        proj = detect_project(entries)
        if proj:
            projects = {proj: [e for e in entries if e.get("project") == proj]}
        else:
            projects = {"(all)": entries}

    global_done = 0
    global_total = 0
    global_cost = 0.0

    for proj_name, proj_entries in sorted(projects.items()):
        features = [e for e in proj_entries if e.get("type") == "feature"]
        done = sum(1 for e in features if e.get("status") == "done")
        claimed = sum(1 for e in features if e.get("status") == "in_progress")
        ready = sum(1 for e in features if e.get("status") == "ready")
        ideas = sum(1 for e in features if e.get("status") == "idea")
        blocked = sum(1 for e in features if e.get("status") == "blocked")
        deferred = sum(1 for e in features if e.get("status") == "deferred")
        total = len(features)
        cost = sum(e.get("cost_usd", 0) or 0 for e in features)

        global_done += done
        global_total += total
        global_cost += cost

        if all_:
            ideas_suffix = f", ideas: {ideas}" if ideas else ""
            deferred_suffix = f", deferred: {deferred}" if deferred else ""
            typer.echo(
                f"\n=== {proj_name} ({done}/{total} done{ideas_suffix}{deferred_suffix}, ${cost:.2f}) ==="
            )
        else:
            typer.echo(f"Project: {proj_name}")
            roadmaps = [e for e in proj_entries if e.get("type") == "roadmap"]
            if roadmaps:
                typer.echo(f"Roadmap: {roadmaps[0].get('roadmap_id', '?')} ({roadmaps[0].get('title', '?')})")
            ideas_suffix = (
                f" | ideas: {ideas} (use 'fno backlog ready --ideas' to list)"
                if ideas else ""
            )
            # Active-most → inactive-most ordering: done | claimed | ready
            # | ideas | blocked | deferred. Deferred is the only state that
            # requires an explicit `--include-deferred` to re-surface, so it
            # belongs at the tail.
            deferred_suffix = (
                f" | deferred: {deferred} (use 'fno backlog ready --include-deferred' to list)"
                if deferred else ""
            )
            typer.echo(
                f"Progress: {done}/{total} done | {claimed} claimed | {ready} ready"
                f"{ideas_suffix} | {blocked} blocked{deferred_suffix}"
            )
            typer.echo(f"Cost: ${cost:.2f}")
            typer.echo("")

        typer.echo(f"{'ID':<14} {'Title':<30} {'Status':<10} {'Priority':<10} {'Cost':>8}  {'PR'}")
        typer.echo("-" * 85)
        for e in features:
            eid = e.get("id", "?")
            title = (e.get("title", "?"))[:28]
            st = e.get("status", "?")
            pri = e.get("priority", "?")
            c = f"${e.get('cost_usd', 0) or 0:.2f}"
            pr = f"#{e.get('pr_number')}" if e.get("pr_number") else "-"
            typer.echo(f"{eid:<14} {title:<30} {st:<10} {pri:<10} {c:>8}  {pr}")

    if all_ and len(projects) > 1:
        typer.echo(f"\nTotal: {global_done}/{global_total} done, ${global_cost:.2f}")


# -- briefs --

# -- validate --

# -- cost --

@cli.command("cost", hidden=True)
def cmd_cost(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    session: Optional[str] = typer.Option(
        None,
        "--session-id",
        help=(
            "Run id owning this cost. Recording twice for one id REPLACES the "
            "row (a session's cost is a level, not an increment), so pass the "
            "unique fno run id - a shared harness/thread id would let a second "
            "attempt overwrite the first."
        ),
    ),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    amount: str = typer.Option(..., "--amount", help="Cost in USD"),
) -> None:
    import click

    from fno._flag_aliases import merge_deprecated_alias
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph

    session = merge_deprecated_alias(
        session, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    # --session-id is required; the merge returns None only when NEITHER
    # spelling was passed (the hidden alias forces a None default here).
    if session is None:
        raise click.UsageError("Missing option '--session-id'.")

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    try:
        amount_f = float(amount)
    except ValueError:
        typer.echo(f"Error: amount must be a number, got '{amount}'", err=True)
        raise typer.Exit(code=1)

    def mutator(entries):
        from fno.cost import upsert_cost_session

        for e in entries:
            if e.get("id") == task_id:
                upsert_cost_session(e, session, amount_f)
                return entries
        typer.echo(f"Error: feature {task_id} not found", err=True)
        raise typer.Exit(code=1)

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"Recorded ${amount_f:.2f} for {task_id} (session {session})")


# -- remove --

@cli.command(
    "remove",
    hidden=True,
    epilog="Reverses `add` / `idea` / `new` / `intake`. Softer options: `archive` "
    "(keeps the node readable), `supersede` (records what replaced it), `defer` "
    "(parks it).",
)
def cmd_remove(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    force: bool = typer.Option(False, "--force", "-F", help="Skip cascade warning"),
) -> None:
    """Delete a node from the graph permanently. This verb exists and works.

    It had no docstring until 2026-08-11, which is why `fno help backlog --all`
    printed its name against an empty description and read as a stub. An agent
    consequently ruled that no delete verb existed and made that the
    load-bearing reason for a decision, and another project kept 23 nodes it
    believed un-file-able. Hence this paragraph: the verb's own help is the one
    place a caller asking "can this node go away" will actually look.

    A HARD delete, unlike ``archive``, which moves the node to
    ``graph-archive.json`` and keeps it readable. Prefer ``archive`` for shipped
    work, ``supersede`` when something replaced it, and ``defer`` when it is
    merely not now. Reach for ``remove`` on a duplicate, a test artifact, or a
    node filed by mistake - the cases where the record itself is the noise.

    Repairs every edge that pointed at the node, because nothing else can once
    the node is gone: drops it from every ``blocked_by``, from the symmetric
    ``related`` lists, nulls a dependent's ``source_node_id`` rather than
    leaving a dangling string, and releases contained children (the reconcile
    heal deliberately skips a MISSING owner, so an orphan there is permanent).

    Refuses when other nodes name it as a blocker, listing them, since removing
    it silently unblocks work whose real dependency never landed. ``--force``
    confirms that trade.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import _find_node, _find_dependents

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    entries = read_graph(_graph_path())
    dependents = _find_dependents(entries, task_id)
    if dependents and not force:
        typer.echo(f"Removing {task_id} will orphan blocked_by in: {', '.join(dependents)}")
        typer.echo("Use --force to confirm.")
        raise typer.Exit(code=1)

    _freed_box: list[list] = [[]]
    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        for e in entries:
            if task_id in e.get("blocked_by", []):
                e["blocked_by"].remove(task_id)
            # related is symmetric, and no other verb can repair a peer that
            # names a node the graph no longer has: set_related only touches
            # peers in the declaring node's own delta.
            if task_id in (e.get("related") or []):
                e["related"].remove(task_id)
            # remove is a HARD delete, unlike archive (which keeps the node
            # readable and therefore guards it instead). A dependent's origin
            # would be left pointing at nothing, and the stated invariant is
            # that source_node_id is null or resolves - never a dangling string.
            if e.get("source_node_id") == task_id:
                e["source_node_id"] = None
        # Same invariant for containment (x-e957), and here a dangling pointer
        # is a permanent trap rather than mere untidiness: the reconcile heal
        # deliberately skips a MISSING owner, so nothing would ever free them.
        _freed_box[0] = _release_contained_children(entries, task_id)
        return [e for e in entries if e.get("id") != task_id]

    locked_mutate_graph(_graph_path(), mutator)
    _echo_freed(_freed_box[0], task_id)
    typer.echo(f"Removed {task_id}" + (f" (orphaned deps in {dependents})" if dependents else ""))


# -- defer / undefer --
#
# ``defer`` records a first-class pause on a backlog node via dedicated
# ``deferred_at`` + ``deferred_reason`` fields. The cascade derives
# ``status: deferred`` from those fields so the node disappears from the
# default ``ready`` / ``next`` candidate sets and from triage proposals,
# but resurfaces with ``--include-deferred``. Reversal is via ``undefer``
# (idempotent: clearing already-clear state warns but exits 0).
#
# Predates the ``completed_at: "deferred:<ts>"`` workaround; ``recompute_statuses``
# auto-migrates the prefix to the new schema, so callers should never see
# the old shape after one mutation.

@cli.command(
    "defer",
    epilog="Paired verb: `fno backlog undefer <id>...` reverses this (hidden; run its own --help).",
)
def cmd_defer(
    task_ids: List[str] = typer.Argument(
        ...,
        help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
    ),
    reason: str = typer.Option(
        ...,
        "--reason", "-R",
        help="Why these nodes are being deferred (applies to all). Free text, surfaced in triage.",
    ),
) -> None:
    """Mark one or more backlog nodes as deferred. Sets ``deferred_at`` + ``deferred_reason``.

    Atomic across the batch: if any ID is unknown, none are deferred.
    Same reason applies to every ID in the batch.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node, _find_dependents

    ids = _expand_id_args(task_ids)
    if not ids:
        typer.echo("Error: at least one task_id is required", err=True)
        raise typer.Exit(code=1)
    for tid in ids:
        if not has_node_id_prefix(tid):
            typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{tid}'", err=True)
            raise typer.Exit(code=1)

    # Strip and validate the reason at the CLI boundary so direct invocation
    # cannot land an empty-reason deferral. The triage validator already
    # rejects blank reasons; matching that contract here keeps both write
    # paths producing identically-shaped graph state.
    cleaned_reason = reason.strip()
    if not cleaned_reason:
        typer.echo("Error: --reason cannot be blank", err=True)
        raise typer.Exit(code=1)

    def mutator(entries):
        # Resolve every id and abort naming ALL missing ones before mutating,
        # mirroring cmd_queue's all-or-nothing batch atomicity.
        missing = [tid for tid in ids if _find_node(entries, tid) is None]
        if missing:
            typer.echo(
                f"Error: feature(s) not found: {', '.join(missing)}",
                err=True,
            )
            raise typer.Exit(code=1)
        now = datetime.now(timezone.utc).isoformat()
        for tid in ids:
            node = _find_node(entries, tid)
            dependents = _find_dependents(entries, tid)
            if dependents:
                typer.echo(
                    f"WARN: Deferring {tid} blocks: {', '.join(dependents)}",
                    err=True,
                )
            node["locked_by"] = None
            node["claimed_at"] = None
            # Clear completed_at PER NODE, inside the loop. The precedence
            # ladder is `done > deferred`, so hoisting this clear out of the
            # loop (or skipping it for the batch) makes deferring a done node
            # a silent no-op: completed_at would keep status pinned to done.
            # Symmetric with cmd_done, which clears deferred_at on the reverse
            # transition.
            node["completed_at"] = None
            node["deferred_at"] = now
            node["deferred_reason"] = cleaned_reason
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    for tid in ids:
        typer.echo(f'Deferred {tid}: "{cleaned_reason}"')
    _project_plans_from_graph(ids)


# -- queue / unqueue / queued --
#
# ``queue`` is the user-facing triage marker for "I'm pulling this off
# the backlog and intend to work on it next" (e.g. "tomorrow I'm going
# to queue x, y, z"). Orthogonal to ``status``: a queued node still has
# ``status: ready`` so ``fno backlog ready`` keeps surfacing it. The
# kanban renderer reads ``queued_at`` separately and promotes the card
# into the Now column (between ``claimed`` and the priority-driven
# promotion rule).
#
# Cleared automatically by ``cmd_done``; reversible via ``unqueue``.

def _expand_id_args(raw_ids: list[str]) -> list[str]:
    """Flatten a list of CLI args into individual node IDs.

    Accepts both space-separated args (``ab-X ab-Y``) and comma-
    separated bundles (``ab-X,ab-Y``) so end-of-day batch triage feels
    natural: ``fno backlog queue ab-X,ab-Y ab-Z`` is valid. Preserves
    first-occurrence order, dedupes ALL repeats (a ``seen`` set drops
    any id already encountered, not just adjacent ones), strips
    whitespace.
    """
    out: list[str] = []
    seen: set[str] = set()
    for raw in raw_ids:
        for part in str(raw).split(","):
            tid = part.strip()
            if not tid:
                continue
            if tid in seen:
                continue
            seen.add(tid)
            out.append(tid)
    return out


@cli.command("queued", hidden=True)
def cmd_queued(
    project: Optional[str] = typer.Option(None, help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects"),
) -> None:
    """List nodes the user has queued for action. JSON output, sorted by priority."""
    from fno.graph.store import read_graph
    from fno.graph._intake import filter_by_project, _graph_sort_key_fn
    from fno.tracker import active_backend_name

    # Queue state is footnote-minted (the queue verb is tracker-owned), so no
    # external item can be queued: answer the empty list rather than consult
    # local rows behind an external selection.
    if active_backend_name() != "graph":
        typer.echo("[]")
        return

    entries = read_graph(_graph_path())
    queued = [e for e in entries
              if e.get("queued_at")
              and not e.get("completed_at")
              and not e.get("deferred_at")]
    queued = filter_by_project(queued, project, all_)
    queued.sort(key=_graph_sort_key_fn)

    output = [{
        "id": e["id"], "title": e.get("title"), "priority": e.get("priority"),
        "project": e.get("project"), "queued_at": e.get("queued_at"),
        "queued_reason": e.get("queued_reason"), "status": e.get("status"),
    } for e in queued]
    typer.echo(json.dumps(output, indent=2))


@cli.command(
    "queue",
    hidden=True,
    epilog="Paired verb: `fno backlog unqueue <id>...` reverses this (hidden; run its own --help).",
)
def cmd_queue(
    task_ids: List[str] = typer.Argument(
        ...,
        help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
    ),
    reason: Optional[str] = typer.Option(
        None,
        "--reason", "-R",
        help="Why these nodes are being queued (applies to all). Free text, surfaced on the card.",
    ),
) -> None:
    """Queue one or more backlog nodes for action. Sets ``queued_at`` + optional ``queued_reason``.

    Atomic across the batch: if any ID is unknown, none of the nodes
    are queued. Same reason applies to every ID in the batch.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    ids = _expand_id_args(task_ids)
    if not ids:
        typer.echo("Error: at least one task_id is required", err=True)
        raise typer.Exit(code=1)
    for tid in ids:
        if not has_node_id_prefix(tid):
            typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{tid}'", err=True)
            raise typer.Exit(code=1)

    cleaned_reason = (reason or "").strip() or None

    def mutator(entries):
        missing = [tid for tid in ids if _find_node(entries, tid) is None]
        if missing:
            typer.echo(
                f"Error: feature(s) not found: {', '.join(missing)}",
                err=True,
            )
            raise typer.Exit(code=1)
        now = datetime.now(timezone.utc).isoformat()
        for tid in ids:
            node = _find_node(entries, tid)
            node["queued_at"] = now
            node["queued_reason"] = cleaned_reason
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    suffix = f': "{cleaned_reason}"' if cleaned_reason else ""
    for tid in ids:
        typer.echo(f"Queued {tid}{suffix}")


@cli.command("unqueue", hidden=True)
def cmd_unqueue(
    task_ids: List[str] = typer.Argument(
        ...,
        help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
    ),
) -> None:
    """Clear queued state on one or more backlog nodes. Idempotent.

    Atomic across the batch: if any ID is unknown, none are cleared.
    Reports each ID's prior state; warns (non-fatally) for IDs that
    were not actually queued.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    ids = _expand_id_args(task_ids)
    if not ids:
        typer.echo("Error: at least one task_id is required", err=True)
        raise typer.Exit(code=1)
    for tid in ids:
        if not has_node_id_prefix(tid):
            typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{tid}'", err=True)
            raise typer.Exit(code=1)

    not_queued: list[str] = []

    def mutator(entries):
        missing = [tid for tid in ids if _find_node(entries, tid) is None]
        if missing:
            typer.echo(
                f"Error: feature(s) not found: {', '.join(missing)}",
                err=True,
            )
            raise typer.Exit(code=1)
        for tid in ids:
            node = _find_node(entries, tid)
            if not node.get("queued_at"):
                not_queued.append(tid)
            node["queued_at"] = None
            node["queued_reason"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    for tid in not_queued:
        typer.echo(f"warning: {tid} was not queued", err=True)
    for tid in ids:
        typer.echo(f"Unqueued {tid}")


def _pick_format_line(entry: dict, id_to_entry: dict[str, dict] | None = None) -> str:
    """Format a graph entry as a single fzf row.

    Shape: ``[marker] kind priority project ab-id title [blockers]``.

    Marker semantics:
      ``[ ]`` ready / idea, not queued
      ``[Q]`` queued (already on tomorrow's plate)
      ``[B]`` blocked by an open dependency, not queued
      ``[Q!]`` queued AND blocked (will fire once unblocked)

    Kind column: ``plan`` if a plan_path exists, else ``idea``.
    Useful to spot pre-plan rows that need /think+/blueprint before
    target can pick them up.

    For blocked rows, the open blocker IDs are appended to the title so
    you can see at a glance why something is gated - useful when you
    want to queue A and the nodes it blocks together.
    """
    is_queued = bool(entry.get("queued_at"))
    is_blocked = entry.get("status") == "blocked"
    if is_queued and is_blocked:
        marker = "[Q!]"
    elif is_queued:
        marker = "[Q]"
    elif is_blocked:
        marker = "[B]"
    else:
        marker = "[ ]"
    kind = "plan" if entry.get("plan_path") else "idea"
    prio = entry.get("priority") or "p2"
    project = (entry.get("project") or "-")
    if len(project) > 22:
        project = project[:21] + "."
    title = (entry.get("title") or "").replace("\n", " ").strip() or "(untitled)"
    if len(title) > 75:
        title = title[:74] + "."
    blocker_suffix = ""
    if is_blocked and id_to_entry is not None:
        open_blockers: list[str] = []
        for bid in entry.get("blocked_by", []) or []:
            if not isinstance(bid, str):
                continue
            b = id_to_entry.get(bid)
            if b and not b.get("completed_at"):
                open_blockers.append(bid)
        if open_blockers:
            blocker_suffix = f"  (blocked by {','.join(open_blockers)})"
    marker_col = f"{marker:5s}"  # pad to width 5 so [Q!] doesn't shift columns
    return f"{marker_col} {kind}  {prio}  {project:22s}  {entry['id']}  {title}{blocker_suffix}"


def _pick_extract_id(line: str) -> str | None:
    """Recover the node ID from a picker row. None if not found.

    Extraction from a free-form row, so use the STRICT well-formed matcher (not
    the liberal prefix pre-check) - otherwise a non-id token that merely starts
    with the prefix (e.g. a project name ``fno-cli``) would be misread as an id.
    """
    from fno.graph._constants import is_wellformed_node_id

    for tok in line.split():
        if is_wellformed_node_id(tok):
            return tok
    return None


def _tsv_safe(s: str | None) -> str:
    """Strip TSV-breaking characters from a candidate field."""
    if not s:
        return ""
    return str(s).replace("\t", " ").replace("\n", " ").replace("\r", " ")


_PICK_RENDER_AWK = r"""
# Reads two files:
#   ARGV[1] = pending.txt (lines: "Q ab-xxxx" / "U ab-xxxx" / "T ab-xxxx",
#                          plus an initial "# pending" sentinel)
#   ARGV[2] = cands.tsv   (tab-delimited candidate snapshot)
# Emits TAB-delimited fzf rows: "<url>\t<id>\t<visible row>".
BEGIN { FS = "\t" }
# First file: record intents in order so T can flip the current running
# state (Q then T = back to original) rather than the immutable initial.
NR == FNR {
    if (length($0) >= 3) {
        kind = substr($0, 1, 1)
        if (kind == "Q" || kind == "U" || kind == "T") {
            rest = substr($0, 3)
            gsub(/[ \t\r\n]+$/, "", rest)
            gsub(/^[ \t]+/, "", rest)
            if (rest != "") {
                cnt = ++pending_count[rest]
                pending_seq[rest "|" cnt] = kind
            }
        }
    }
    next
}
# Second file: walk per-id intents in order to compute effective state.
{
    id = $1; title = $2; prio = $3; project = $4; status = $5
    q_initial = ($6 == "1") ? 1 : 0
    plan_path = $7; blocked_by = $8; url = $9

    queued = q_initial
    n = pending_count[id]
    for (i = 1; i <= n; i++) {
        k = pending_seq[id "|" i]
        if (k == "Q") queued = 1
        else if (k == "U") queued = 0
        else if (k == "T") queued = (1 - queued)
    }

    is_blocked = (status == "blocked")
    if (queued && is_blocked)       marker = "[Q!]"
    else if (queued)                 marker = "[Q]"
    else if (is_blocked)             marker = "[B]"
    else                             marker = "[ ]"

    kind_col = (plan_path == "") ? "idea" : "plan"

    if (length(project) > 22) project = substr(project, 1, 21) "."
    if (length(title)   > 75) title   = substr(title,   1, 74) "."

    while (length(marker) < 5)   marker = marker " "
    while (length(project) < 22) project = project " "

    blocker_suffix = ""
    if (is_blocked && blocked_by != "")
        blocker_suffix = "  (blocked by " blocked_by ")"

    printf "%s\t%s\t%s %s  %s  %s  %s  %s%s\n", \
        url, id, marker, kind_col, prio, project, id, title, blocker_suffix
}
"""


@cli.command("pick", hidden=True)
def cmd_pick(
    project: Optional[str] = typer.Option(None, help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects (default: current cwd)"),
    include_ideas: bool = typer.Option(
        True,
        "--ideas/--no-ideas",
        help="Include idea-stage rows alongside ready ones (default: yes).",
    ),
    include_blocked: bool = typer.Option(
        False,
        "--blocked/--no-blocked",
        "-b",
        help="Also show blocked rows so you can queue a node + its blocked dependents together. Open blockers are shown inline. Default: off.",
    ),
    reason: Optional[str] = typer.Option(
        None,
        "--reason", "-R",
        help="Reason applied to every newly-queued node (optional).",
    ),
) -> None:
    """Interactively manage the backlog queue via fzf with live marker updates.

    Pressing keys updates the marker in real time via fzf's reload
    action. So pressing ``q`` on a ``[ ]`` row flips it to ``[Q]``
    in-place; pressing ``u`` on a ``[Q]`` row flips it back to ``[ ]``.

      q       queue this row     -> marker becomes [Q]
      u       unqueue this row   -> marker becomes [ ]
      space   toggle this row    -> marker flips
      o       open plan in Obsidian (idea rows: no-op)
      Enter   commit all pending marker changes atomically
      Ctrl-C  cancel; no marks land on the graph
      type    fuzzy-filter the visible rows

    Markers reflect the effective state INCLUDING pending changes.
    Latest mark per row wins, so you can change your mind by pressing
    the opposite key. Idempotent: re-queuing an already-queued row is
    a no-op on commit.
    """
    import os as _os
    import platform
    import shlex
    import shutil
    import subprocess
    import tempfile

    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import filter_by_project, _find_node, _graph_sort_key_fn
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.render_html import _load_obsidian_vault, _obsidian_url

    fzf = shutil.which("fzf")
    if not fzf:
        typer.echo(
            "Error: fzf not found on PATH. Install with `brew install fzf` "
            "(macOS) or your package manager.",
            err=True,
        )
        raise typer.Exit(code=1)
    awk_bin = shutil.which("awk")
    if not awk_bin:
        typer.echo("Error: awk not found on PATH (needed for live marker updates).", err=True)
        raise typer.Exit(code=1)

    entries = read_graph(_graph_path())
    allowed = {"ready"}
    if include_ideas:
        allowed.add("idea")
    if include_blocked:
        allowed.add("blocked")
    candidates = [e for e in entries if e.get("status") in allowed]
    candidates = filter_by_project(candidates, project, all_)

    if not candidates:
        scope = "/".join(sorted(allowed))
        typer.echo(f"No {scope} rows to pick from in this scope.")
        return

    # Sort queued rows to the TOP, then by priority within each cluster.
    currently_queued = {e["id"] for e in candidates if e.get("queued_at")}
    candidates.sort(
        key=lambda e: (0 if e["id"] in currently_queued else 1, _graph_sort_key_fn(e))
    )

    vault = _load_obsidian_vault()
    open_cmd = "open" if platform.system() == "Darwin" else (
        shutil.which("xdg-open") or shutil.which("wslview") or "xdg-open"
    )

    # Tempfiles:
    #   cands.tsv  : the immutable snapshot of candidates the picker reads
    #   pending.txt: empty file the keybinds append intents to
    #   awk.script : the renderer logic invoked by fzf reload
    fd_cand, cand_path = tempfile.mkstemp(prefix="fno-pick-", suffix=".cands.tsv")
    fd_pend, pend_path = tempfile.mkstemp(prefix="fno-pick-", suffix=".pending.txt")
    fd_awk, awk_path = tempfile.mkstemp(prefix="fno-pick-", suffix=".awk")
    # Seed pending.txt with a sentinel comment line. Awk's NR==FNR test
    # misfires when the first file is empty (FNR resets at file
    # boundary so the first record of file 2 also has NR==FNR), and
    # then candidate rows get mistakenly parsed as pending intents.
    # Any non-intent line is silently skipped by the renderer.
    with _os.fdopen(fd_pend, "w") as f:
        f.write("# pending\n")

    try:
        with _os.fdopen(fd_cand, "w") as f:
            for e in candidates:
                url = ""
                if vault and e.get("plan_path"):
                    built = _obsidian_url(vault, e["plan_path"])
                    if built:
                        url = built
                blockers = ",".join(
                    b for b in (e.get("blocked_by") or []) if isinstance(b, str)
                )
                row_fields = [
                    e["id"],
                    _tsv_safe(e.get("title") or ""),
                    e.get("priority") or "p2",
                    _tsv_safe(e.get("project") or "-"),
                    e.get("status") or "ready",
                    "1" if e.get("queued_at") else "0",
                    _tsv_safe(e.get("plan_path") or ""),
                    blockers,
                    url,
                ]
                f.write("\t".join(row_fields) + "\n")
        with _os.fdopen(fd_awk, "w") as f:
            f.write(_PICK_RENDER_AWK)

        qa = shlex.quote(awk_bin)
        qs = shlex.quote(awk_path)
        qp = shlex.quote(pend_path)
        qc = shlex.quote(cand_path)
        render_cmd = f"{qa} -f {qs} {qp} {qc}"

        header_lines = [
            f"q=queue  u=unqueue  space=toggle  o=open plan  Enter=commit  Ctrl-C=cancel  "
            f"({len(candidates)} rows, {len(currently_queued)} queued initially)",
            "Markers update in-place as you press keys: [ ] not queued  [Q] queued  [B] blocked  [Q!] queued+blocked",
        ]
        if not vault:
            header_lines.append(
                "(set config.obsidian.vault in settings.yaml to enable 'o' opener)"
            )
        header = "\n".join(header_lines)

        # Initial row set: run the renderer once with empty pending.
        initial = subprocess.run(
            [awk_bin, "-f", awk_path, pend_path, cand_path],
            capture_output=True, text=True, check=False,
        ).stdout

        proc = subprocess.run(
            [
                fzf,
                "--no-multi",
                "--delimiter", "\t",
                "--with-nth", "3..",
                "--nth", "3..",
                # Q/U/T keybinds: append intent line to pending.txt, then
                # reload the row list from awk. Cursor preserves via fzf's
                # default reload behavior; +down advances to next row.
                "--bind", f"q:execute-silent(printf 'Q %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind", f"u:execute-silent(printf 'U %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind", f"space:execute-silent(printf 'T %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind", "enter:accept",
                "--bind", f"o:execute-silent(u={{1}}; [[ -n \"$u\" ]] && {open_cmd} \"$u\")",
                "--header", header,
                "--prompt", "pick> ",
                "--height", "85%",
                "--reverse",
                "--no-sort",
            ],
            input=initial,
            text=True,
            capture_output=True,
        )

        # rc 130 = Ctrl-C / Esc - drop pending unread.
        if proc.returncode == 130:
            typer.echo("Cancelled.")
            return

        # Parse pending.txt INSIDE the try so we read it before the
        # finally block deletes it. Preserve order so T flips the
        # running state (matches what awk renders to the screen).
        ordered_intents: list[tuple[str, str]] = []
        try:
            with open(pend_path) as f:
                for raw in f:
                    if len(raw) < 3:
                        continue
                    kind = raw[0]
                    if kind not in ("Q", "U", "T"):
                        continue
                    rest = raw[1:].strip()
                    if has_node_id_prefix(rest):
                        ordered_intents.append((rest, kind))
        except OSError:
            pass
    finally:
        for path in (cand_path, pend_path, awk_path):
            try:
                _os.unlink(path)
            except OSError:
                pass

    if not ordered_intents:
        typer.echo("No changes.")
        return

    final_queued: dict[str, bool] = {}
    for tid, kind in ordered_intents:
        if kind == "Q":
            final_queued[tid] = True
        elif kind == "U":
            final_queued[tid] = False
        elif kind == "T":
            prev = final_queued.get(tid, tid in currently_queued)
            final_queued[tid] = not prev

    to_queue: list[str] = []
    to_unqueue: list[str] = []
    for tid, want_queued in final_queued.items():
        was_queued = tid in currently_queued
        if want_queued and not was_queued:
            to_queue.append(tid)
        elif was_queued and not want_queued:
            to_unqueue.append(tid)

    if not to_queue and not to_unqueue:
        typer.echo("No changes (marks ended at original state).")
        return

    cleaned_reason = (reason or "").strip() or None

    # Capture mutator outputs via a dict in the enclosing scope rather
    # than function attributes (clearer than mutator.x = ... pattern,
    # per Gemini review on PR #253).
    results: dict[str, list[str]] = {"queued_applied": [], "unqueued_applied": []}

    def mutator(graph_entries):
        now = datetime.now(timezone.utc).isoformat()
        queued_applied: list[str] = []
        unqueued_applied: list[str] = []
        for tid in to_queue:
            # Silent skip when a node is missing or already in the target
            # state. A node disappearing between the picker snapshot read
            # and the lock acquisition is a tolerable race - aborting the
            # whole batch over it would lose the user's other valid marks.
            node = _find_node(graph_entries, tid)
            if not node or node.get("queued_at"):
                continue
            node["queued_at"] = now
            if cleaned_reason:
                node["queued_reason"] = cleaned_reason
            queued_applied.append(tid)
        for tid in to_unqueue:
            node = _find_node(graph_entries, tid)
            if not node or not node.get("queued_at"):
                continue
            node["queued_at"] = None
            node["queued_reason"] = None
            unqueued_applied.append(tid)
        results["queued_applied"] = queued_applied
        results["unqueued_applied"] = unqueued_applied
        return graph_entries

    locked_mutate_graph(_graph_path(), mutator)
    queued_applied = results["queued_applied"]
    unqueued_applied = results["unqueued_applied"]

    suffix = f': "{cleaned_reason}"' if cleaned_reason else ""
    for tid in queued_applied:
        typer.echo(f"Queued {tid}{suffix}")
    for tid in unqueued_applied:
        typer.echo(f"Unqueued {tid}")
    if queued_applied or unqueued_applied:
        typer.echo(
            f"({len(queued_applied)} queued, {len(unqueued_applied)} unqueued)"
        )
    else:
        typer.echo("(no changes)")


@cli.command("undefer", hidden=True)
def cmd_undefer(
    task_ids: List[str] = typer.Argument(
        ...,
        help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
    ),
) -> None:
    """Clear deferred state on one or more backlog nodes. Idempotent.

    Atomic across the batch: if any ID is unknown, none are cleared.
    Reports each ID's prior state; warns (non-fatally) for IDs that were
    not actually deferred. Each node that WAS deferred gets its own
    streak-reset event.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    ids = _expand_id_args(task_ids)
    if not ids:
        typer.echo("Error: at least one task_id is required", err=True)
        raise typer.Exit(code=1)
    for tid in ids:
        if not has_node_id_prefix(tid):
            typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{tid}'", err=True)
            raise typer.Exit(code=1)

    was_deferred: list[tuple[str, bool]] = []

    def mutator(entries):
        missing = [tid for tid in ids if _find_node(entries, tid) is None]
        if missing:
            typer.echo(
                f"Error: feature(s) not found: {', '.join(missing)}",
                err=True,
            )
            raise typer.Exit(code=1)
        for tid in ids:
            node = _find_node(entries, tid)
            was_deferred.append((tid, bool(node.get("deferred_at"))))
            node["deferred_at"] = None
            node["deferred_reason"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    for tid, did in was_deferred:
        if did:
            # Mark a streak-reset boundary so the failed-node cascade (#34) gives a
            # human-recovered node a clean slate: it needs N FRESH consecutive
            # failures before auto-defer re-triggers (AC5-FR). The reader keys on
            # data.unit_id; the flat agents envelope it writes is accepted too.
            # Best-effort - a failed emit only means the node keeps its pre-undefer
            # streak, never a crash in undefer. Per node: the boundary is keyed on
            # data.unit_id, so N nodes that were actually deferred emit N events.
            from fno.graph.failure import emit_undefer_boundary

            emit_undefer_boundary(tid)
        else:
            typer.echo(f"warning: {tid} was not deferred", err=True)
        typer.echo(f"Undeferred {tid}")
    _project_plans_from_graph(ids)


# -- done --

def _project_plans_from_graph(
    node_ids: list[str], *, mirror_type_for: str | None = None,
    force_status_off_terminal_for: str | None = None,
) -> None:
    """Project each named node's mirror fields + forward status onto its plan.

    Re-reads the graph so every node carries its recomputed ``status`` (a claim
    reads ``claimed`` -> ``in_progress``; a close reads ``done`` -> ``done`` +
    ``done_at``), then delegates to the shared converger. Covers cascade-closed
    epic parents that ``_stamp_and_graduate_plan`` never stamps. Best-effort per
    node: a missing or unreadable plan never fails the mutation.

    ``mirror_type_for`` names the ONE node whose ``type`` may be written, set
    only by the ``--type`` path where the operator supplied that node's value.
    It is an id, not a flag: this projection repaints ancestors and siblings
    too, and their ``type`` is still a mint-time default.
    """
    ids = [i for i in dict.fromkeys(node_ids) if i]
    if not ids:
        return
    try:
        # Vault mirror projection is default-backend machinery (same class as
        # plan sync): guarded metadata read, degrades to a no-op under an
        # external selection rather than painting stale local rows.
        from fno.plan._project import project_graph_nodes
        from fno.tracker.metadata import read_entries

        entries = read_entries("plan.project")
    except Exception as e:  # noqa: BLE001 - additive; never wedge the mutation
        sys.stderr.write(f"warning: plan projection setup failed: {e}\n")
        return
    project_graph_nodes(
        entries, ids, mirror_type_for=mirror_type_for,
        force_status_off_terminal_for=force_status_off_terminal_for,
    )


def _apply_completion_fields(node: dict, *, merge_status: Optional[str] = None) -> None:
    """Set the fields that mark a node done.

    Shared by ``done`` and ``reconcile`` so both close paths stay in
    lockstep. The caller owns the idempotency check (skip when
    ``completed_at`` is already set). ``recompute_statuses`` derives
    ``status: done`` from ``completed_at`` and unblocks dependents.

    ``merge_status`` is passed ONLY by a caller that resolved MERGED from gh,
    so the field keeps meaning "GitHub confirmed this". A ``--force`` close and
    a PR-less epic cascade leave it unset rather than assert a merge.
    """
    node["locked_by"] = None
    node["claimed_at"] = None
    # Done dominates deferred per the cascade. Clear any deferred/queued state
    # so the row presents as cleanly done with no ghost fields.
    node["deferred_at"] = None
    node["deferred_reason"] = None
    node["queued_at"] = None
    node["queued_reason"] = None
    node["completed_at"] = datetime.now(timezone.utc).isoformat()
    if merge_status is not None:
        node["merge_status"] = merge_status


def _clear_completion_fields(node: dict, *, reason: str) -> None:
    """Undo :func:`_apply_completion_fields`. Shared by ``reopen`` and its cascade.

    It lives beside its forward counterpart for that function's own stated
    reason: the close paths share one helper so they cannot drift, and an open
    path that drifts from the close path is the same defect pointed the other
    way.

    Clearing ``completed_at`` IS the status change - ``recompute_statuses``
    derives the node's underlying state from its absence, exactly as it derives
    ``done`` from its presence.

    ``completion_note`` is cleared rather than overwritten with the reopen
    trail, and this is load-bearing rather than tidy: ``_cascade_close_parents``
    only writes its ``auto-closed:`` note when that field is EMPTY, so an epic
    carrying reopen prose would never be recognizable as cascade-closed again,
    and a later reopen would leave it done under a live child. The trail goes in
    dedicated ``reopened_at`` / ``reopened_reason`` fields instead.

    Four things are deliberately NOT restored, because reopening a node is not
    rewinding time:

    - ``merge_status`` stays. It records that GitHub confirmed a merge, which is
      still true after a reopen; clearing it would erase a fact to express an
      opinion.
    - ``cost_usd`` / ``cost_sessions`` stay. The spend happened.
    - ``locked_by`` / ``claimed_at`` stay null. ``done`` cleared them, and
      inventing a holder here would give the node a claim no lockfile backs;
      claims are acquired by ``fno do target init``.
    - ``deferred_at`` / ``queued_at`` stay null. ``done`` cleared those too, and
      re-parking is ``defer``'s job - the same policy ``cmd_unsupersede``
      applies to un-containment.
    """
    node["completed_at"] = None
    node["completion_note"] = None
    node["reopened_at"] = datetime.now(timezone.utc).isoformat()
    node["reopened_reason"] = reason


def _auto_closed_note(entry: dict) -> str:
    """completion_note for a container closed by all-children-complete (x-b9a5).

    Both close paths (_cascade_close_parents on a child close,
    _sweep_close_done_epics on reconcile) reach here holding the parent dict. A
    container with its own plan_path but no PR may carry planned deliverables
    the children never built; the cascade closes it anyway and stamps a real
    completed_at, so the false-done is invisible to any audit keyed on
    completion. Flag the gap: when plan_path is set and there is no pr_number,
    the note records the container's own deliverables as UNVERIFIED.

    It asserts nothing about whether the deliverables exist. A filesystem stat
    had a measured 75% false-positive rate (renames and path conventions read
    as missing), so plan_path + no pr_number is the only signal that does not
    lie. A flag, not a gate: the close still happens, it just becomes findable.
    """
    if entry.get("plan_path") and not entry.get("pr_number"):
        return "auto-closed: all children complete; own plan deliverables UNVERIFIED (plan_path set, no PR)"
    return "auto-closed: all children complete"


def _cascade_close_parents(entries: list[dict], node_id: str) -> list[str]:
    """Close ancestor epics whose children are now all complete (x-33b2).

    Called inside the close mutator right after a node's completion fields are
    set. An epic is a container with no PR of its own - its work IS its
    decomposed children - so it is "done" exactly when all of them are. Walking
    UP the ``parent`` chain, each ancestor whose children all carry
    ``completed_at`` is closed too (and tagged with a completion_note so the
    PR-less close is self-explaining), continuing to the grandparent.

    This is the closure path that lets epics be excluded from build-SELECTION
    everywhere (`next`/`ready`/advance_dependents never dispatch the box): the
    box closes itself off the merge event that finishes its last child. It fires
    on every close path (done + reconcile) since each calls
    this after ``_apply_completion_fields``, and it is uniform across projects
    because it follows the parent EDGE, not a project filter - so a cross-project
    parent closes on the same merge that completes its last child.

    Idempotent: an already-done or missing ancestor stops that branch. The walk
    is depth-capped against a malformed parent cycle.
    """
    id_to_entry = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    children_by_parent: dict[str, list[dict]] = {}
    for e in entries:
        if isinstance(e, dict) and isinstance(e.get("parent"), str):
            children_by_parent.setdefault(e["parent"], []).append(e)

    closed: list[str] = []
    cur = id_to_entry.get(node_id)
    for _ in range(64):  # depth cap: guards against a malformed parent cycle
        pid = cur.get("parent") if isinstance(cur, dict) else None
        if not isinstance(pid, str):
            break
        parent = id_to_entry.get(pid)
        if parent is None or parent.get("completed_at"):
            break  # missing or already-closed ancestor -> stop this branch
        kids = children_by_parent.get(pid) or []
        if not kids or any(not k.get("completed_at") for k in kids):
            break  # at least one child still open -> the epic is not done yet
        _apply_completion_fields(parent)
        if not parent.get("completion_note"):
            parent["completion_note"] = _auto_closed_note(parent)
        # Deactivate the mission (x-9608 K1): a kicked-off epic carries
        # mission_active=true for K2's drain loop; its last child landing closes
        # the epic here, so clear the marker in the same mutation. Durable
        # deactivation - the drain never keeps looping a done mission.
        parent.pop("mission_active", None)
        closed.append(pid)
        cur = parent  # cascade up to the grandparent
    return closed


def _echo_freed(freed: list, owner_id: str) -> None:
    """Name the nodes a dying delivery unit just released.

    Silence here is a real gap, not tidiness: the release turns N nodes that
    were invisible to dispatch into autonomously buildable, separately costed
    ones, and a bare remove/supersede receipt gives the operator no way to know
    what the next selection pass will pick up.
    """
    if not freed:
        return
    typer.echo(
        f"Released {len(freed)} contained node(s) from {owner_id}; they are "
        f"dispatchable again: {', '.join(freed)}"
    )


def _release_contained_children(entries: list[dict], owner_id: Optional[str]) -> list[str]:
    """Un-contain everything shipping inside ``owner_id``; return the ids freed.

    Called wherever a delivery unit permanently dies: remove and supersede. A
    reversible defer keeps its folded delivery unit intact so undefer restores
    the same one-PR scope. A permanently dead unit will never merge, so
    ``_strandable_contained_ids`` (which keys on ``completed_at``) can never heal
    its children, while ``selection_guards`` and ``fno do target init`` keep
    refusing them: unbuildable, uncloseable, invisible to every sweep.

    Un-contained, never closed: a unit dying is not a claim that its children
    shipped.
    """
    if not owner_id:
        return []
    freed: list[str] = []
    for e in entries:
        if isinstance(e, dict) and e.get("contained_in") == owner_id:
            e.pop("contained_in", None)
            nid = e.get("id")
            if isinstance(nid, str) and nid:
                freed.append(nid)
    return freed


def _is_live(entry: dict) -> bool:
    """A child is LIVE when it is not terminal: it would strand if its owner died.

    Terminal is the precedence floor in `recompute_statuses` (done > superseded
    > deferred): a node with ``completed_at`` is done, one with
    ``superseded_by`` is superseded, one with ``deferred_at`` is deferred.
    Everything else (idea, ready, blocked, in_review, in_progress) is live and
    dispatchable, so killing its owner without releasing it leaves it
    unbuildable under the dead-ancestor guard.
    """
    if entry.get("completed_at") or entry.get("deferred_at"):
        return False
    if not entry.get("superseded_by"):
        return True
    supersession = entry.get("supersession")
    return isinstance(supersession, dict) and not supersession.get("verified_at")


def _live_child_ids(entries: list[dict], owner_id: Optional[str]) -> list[str]:
    """Ids of the owner's live children that the supersede guard refuses over.

    Membership children only (``parent == owner``), EXCLUDING contained
    children (``contained_in == owner``). The two axes are released differently:
    a contained child is folded delivery work, and superseding the unit
    releases it routinely - that release IS the safety, so it is not a reason
    to refuse. A parent-only child is epic membership; superseding orphans it
    (clearing ``parent``), a structural change the guard exists to consent to.
    This is also why the guard reads liveness, not ``type``: the epic that
    prompted this was itself typed ``feature``.
    """
    if not owner_id:
        return []
    live: list[str] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        if e.get("contained_in") == owner_id:
            continue  # folded work - the contained release handles it, not the guard
        if e.get("parent") != owner_id:
            continue
        if not _is_live(e):
            continue
        nid = e.get("id")
        if isinstance(nid, str) and nid:
            live.append(nid)
    return live


def _release_parented_children(entries: list[dict], owner_id: Optional[str]) -> list[str]:
    """Clear ``parent`` on the owner's non-done children; return the ids freed.

    The membership-axis sibling of ``_release_contained_children``. That helper
    covers ``contained_in`` (delivery: ships inside this PR); this one covers
    ``parent`` (epic membership). A permanently dead unit's children would
    otherwise stay parented to it: any one later revived (undeferred or
    unsuperseded) then hits the dead-ancestor selection guard and strands - the
    exact state this release exists to prevent.

    NON-DONE children only. ``completed_at`` is the one truly terminal marker
    (done never reactivates), so a shipped child keeps ``parent`` as history.
    Live, deferred, and superseded children can all return to dispatch, so all
    get cleared: leaving a deferred child parented to a dead unit would strand
    it the moment it is undeferred. This is the gap a release keyed on liveness
    alone misses - the supersede guard refuses only over currently-dispatchable
    children, so deferred/superseded children pass it and must be released here.

    Lives in ``cmd_supersede``, not the shared release helper, because
    ``cmd_defer`` also calls that helper and defer is a pause (undefer exists):
    a deferred epic's children must keep their membership through the pause.
    Supersede is permanent, so only it orphans.

    Nothing re-parents on ``unsupersede``: re-adoption is decompose's job, the
    same policy the contained release states for un-containment.
    """
    if not owner_id:
        return []
    freed: list[str] = []
    for e in entries:
        if not isinstance(e, dict) or e.get("parent") != owner_id:
            continue
        if e.get("completed_at"):
            continue  # done is truly terminal - keep parent as history
        # Set None (key kept) rather than pop, matching the supported un-adopt
        # path (`update --parent null`) and every other parent writer; readers
        # use .get(), so a present-None reads identically to absent.
        e["parent"] = None
        nid = e.get("id")
        if isinstance(nid, str) and nid:
            freed.append(nid)
    return freed


def _cascade_close_contained(entries: list[dict], node_id: str) -> list[str]:
    """Close every node that shipped inside ``node_id``'s PR (x-e957 task 1.5).

    Called inside the close mutator right after a delivery unit's completion
    fields are set. A node carrying ``contained_in`` was folded into that unit
    by ``decompose ... adopt:``; its work rides the unit's PR, so it has no PR
    of its own and ``scan_merge_drift`` - which only ever returns nodes carrying
    a PR - can never see it. Before this, dispatch and cost had each learned to
    read containment and completion had no inference at all, so a contained node
    stayed open forever behind a merged PR.

    Deliberately NOT the inverse of ``_cascade_close_parents``. That one closes
    a parent when its last child lands (bottom-up, conditional on the siblings);
    this closes children off their owner's merge (top-down, unconditional). One
    level only: containment is a direct relation to the node that owns the PR,
    not a chain, so a node contained in a contained node is a shape decompose
    cannot produce.

    Three deliberate omissions, each load-bearing:

    - No ledger rollup. Reusing the done root's path would hand each contained
      node the same plan's cost and re-introduce the very triple count task 1.4
      just removed. ``cost_usd`` stays None; the note is what makes that null
      read as located rather than missing.
    - No ``merge_status``. The field means "GitHub confirmed THIS node's PR
      merged" and a contained node has no PR, matching how the PR-less epic
      cascade leaves it unset.
    - No auto-continue dispatch. See the AC6 note in
      ``tests/unit/test_reconcile_cascade.py``: a contained node is not a legal
      ``blocked_by`` target, and fanning one merge into N dispatches would scale
      with how finely an epic happened to be decomposed. The close still
      re-arms any dependent for the next selection pass.

    Idempotent: an already-closed node keeps its own completion and note, so a
    child that shipped its own PR is never relabelled as contained cargo.
    Reconcile runs on every SessionStart, so this matters more than once.

    The note is built BEFORE any mutation and nothing fallible runs between a
    node's ``_apply_completion_fields`` and its note. The caller treats a raised
    cascade as a warning and keeps the delivery unit's close, so anything that
    can throw mid-loop leaves nodes closed with no note - done, with the reason
    they are done missing. Cheap to arrange, and the alternative is a state no
    reader can interpret.
    """
    unit = next(
        (e for e in entries
         if isinstance(e, dict) and e.get("id") == node_id),
        {},
    )
    pr = unit.get("pr_number")
    where = f"PR #{pr}" if pr else "its PR"
    note = (
        f"auto-closed: shipped inside {node_id} ({where}); "
        f"cost and session are recorded on {node_id}"
    )

    closed: list[str] = []
    for e in entries:
        if not isinstance(e, dict) or e.get("contained_in") != node_id:
            continue
        if e.get("completed_at"):
            continue  # already closed (out of band, or a previous sweep)
        nid = e.get("id")
        if not isinstance(nid, str) or not nid:
            continue  # unidentifiable row: nothing to report, nothing to close
        _apply_completion_fields(e)
        e["completion_note"] = note
        closed.append(nid)
    return closed


def _strandable_contained_ids(entries: list[dict]) -> set[str]:
    """Open nodes whose delivery unit is ALREADY done - closeable right now.

    ``_cascade_close_contained`` only fires while a unit is being closed, and
    ``scan_merge_drift`` never returns an already-closed unit, so a node that
    became contained AFTER its owner shipped is reachable by neither. That is
    not hypothetical: re-running an `adopt` spec back-fills ``contained_in``
    onto a node adopted by an older fno, and if that node's owner has already
    merged, the back-fill removes it from selection (the containment guard) with
    nothing left that would ever complete it - visible, unbuildable, never done.

    Read-only. The same self-heal role ``_strandable_epic_ids`` plays for
    all-done epics, and for the same reason: a state the forward path now
    prevents still has to be swept out of graphs that already carry it. Once
    migrated this returns empty and the sweep is a no-op.
    """
    by_id = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    out: set[str] = set()
    for e in entries:
        if not isinstance(e, dict) or e.get("completed_at"):
            continue
        owner_id = e.get("contained_in")
        if not isinstance(owner_id, str) or not owner_id:
            continue
        owner = by_id.get(owner_id)
        nid = e.get("id")
        # `.get`, not `e["id"]`: a row carrying contained_in but no id would
        # raise KeyError, and this runs OUTSIDE any try/except in cmd_reconcile
        # - so it would abort the whole sweep. Exactly the failure class as the
        # SessionStart jq bug this same PR fixes; a read of untrusted graph rows
        # must never be the thing that takes reconcile down.
        if owner is not None and owner.get("completed_at") and isinstance(nid, str) and nid:
            out.add(nid)
    return out


def _sweep_close_stranded_contained(entries: list[dict]) -> list[str]:
    """Close every node :func:`_strandable_contained_ids` names.

    Grouped by owner so each node gets the same note the merge-time cascade
    writes, naming its unit and that unit's PR.
    """
    # Hoisted: called inside the comprehension it re-ran once per entry, each
    # pass rebuilding the whole by_id map - O(N^2) on a path reconcile fires at
    # every SessionStart.
    stranded = _strandable_contained_ids(entries)
    if not stranded:
        return []
    owners = {
        e.get("contained_in")
        for e in entries
        if isinstance(e, dict) and e.get("id") in stranded
    }
    closed: list[str] = []
    for owner_id in sorted(o for o in owners if isinstance(o, str) and o):
        closed.extend(_cascade_close_contained(entries, owner_id))
    return closed


def _strandable_epic_ids(entries: list[dict]) -> set[str]:
    """Open epics (parents) whose children are ALL done - closeable right now.

    Read-only. The cascade (_cascade_close_parents) only fires on a child-CLOSE
    event, so an epic whose children were all completed BEFORE this code shipped
    (or whose last child closed via a path that did not cascade) is stranded:
    open, all children done, and - now that containers are hidden from
    next/ready - unreachable for closure. This identifies them so reconcile can
    self-heal (codex P2 on PR #69).
    """
    children_by_parent: dict[str, list[dict]] = {}
    for e in entries:
        if isinstance(e, dict) and isinstance(e.get("parent"), str):
            children_by_parent.setdefault(e["parent"], []).append(e)
    id_to_entry = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    out: set[str] = set()
    for pid, kids in children_by_parent.items():
        parent = id_to_entry.get(pid)
        if (
            parent is not None
            and not parent.get("completed_at")
            and all(k.get("completed_at") for k in kids)
        ):
            out.add(pid)
    return out


def _sweep_close_done_epics(entries: list[dict]) -> list[str]:
    """Close every open epic whose children are all done (self-heal/migration).

    Idempotent, mutating, run inside a close mutator. Repeats to a fixpoint so a
    freshly-closed epic heals ITS parent too (grandparent chains). Returns the
    ids it closed so the caller can auto-continue their dependents. Reconcile
    runs this so pre-existing stranded all-done epics (codex P2 on PR #69) heal
    on the next reconcile pass - going forward the cascade prevents new ones, so
    this is a no-op once migrated.
    """
    id_to_entry = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    closed: list[str] = []
    for _ in range(64):  # fixpoint, depth-capped against a malformed cycle
        ready = _strandable_epic_ids(entries)
        if not ready:
            break
        for pid in ready:
            parent = id_to_entry.get(pid)
            if parent is None or parent.get("completed_at"):
                continue
            _apply_completion_fields(parent)
            if not parent.get("completion_note"):
                parent["completion_note"] = _auto_closed_note(parent)
            closed.append(pid)
    return closed


def _stamp_and_graduate_plan(
    plan_path: str,
    *,
    url: Optional[str] = None,
    session_id: Optional[str] = None,
) -> bool:
    """Best-effort: stamp a plan ``shipped`` (when a ship URL is known) then graduate.

    The completion path (``done``/``reconcile``) closes a node because its PR
    landed. ``graduate`` ALONE is a no-op on a plan that never went through
    target's ship gate: ``cmd_graduate`` returns early unless ``status`` is
    already ``shipped``, so a never-stamped plan's frontmatter would never record
    the ship (ab-bd9f476c). When a concrete PR ``url`` is available we first
    ``stamp`` the plan (sets ``shipped_at`` + ``status: in_review`` + records the
    URL and session id) and THEN ``graduate`` (flips ``shipped -> done`` once the
    URL count is met). Without a URL we fall back to graduate-only - the prior
    behavior - rather than assert a ship we cannot evidence (e.g. a forced close
    on an advisory node with no PR).

    Returns True when a stamp/graduate actually ran successfully (the relevant
    verb exited 0); False when the run failed. Non-fatal: every failure warns
    and returns False, never raising, so a node close is never aborted by a
    stamp problem.

    Shared by ``done`` and ``reconcile``. The stamper is the in-package
    ``fno.plan._stamp`` module, run under the same interpreter as fno, so it
    resolves whether the package runs from the repo (editable install) or a
    uv-installed venv.
    """
    import subprocess

    def _run(verb_args: list[str]):
        try:
            # sys.executable + ``-m fno.plan._stamp``: run the stamp under the
            # same interpreter/venv as fno so it sees the same deps, and avoid
            # failing where the binary is named "python".
            return subprocess.run(
                [sys.executable, "-m", "fno.plan._stamp", *verb_args],
                check=False,
                capture_output=True,
                text=True,
                # Bound the stamp so a hung subprocess never blocks a node close
                # (gemini, PR #474). A timeout raises and is caught below ->
                # treated as a failed run, non-fatal.
                timeout=30,
            )
        except Exception as e:  # spawn failure / timeout: warn, treat as a failed run
            typer.echo(
                f"warning: fno.plan._stamp {verb_args[0]} failed to run: {e}",
                err=True,
            )
            return None

    stamped_shipped = False
    if url:
        sid = session_id or "backlog-close"
        res = _run(["stamp", "--plan-path", plan_path, "--session-id", sid, "--url", url])
        if res is None:
            return False
        if res.returncode != 0:
            typer.echo(
                f"warning: fno.plan._stamp stamp exited {res.returncode}"
                f"{f' - stderr: {res.stderr.strip()}' if res.stderr else ''}",
                err=True,
            )
            return False
        stamped_shipped = True

    res = _run(["graduate", "--plan-path", plan_path])
    if res is None:
        # A successful stamp already recorded the ship; report that win even if
        # the graduate spawn failed.
        return stamped_shipped
    if res.returncode != 0:
        # Surface the script's own error so a broken stamp run is diagnosable
        # instead of silently eaten.
        typer.echo(
            f"warning: fno.plan._stamp graduate exited {res.returncode}"
            f"{f' - stderr: {res.stderr.strip()}' if res.stderr else ''}",
            err=True,
        )
        return stamped_shipped
    return True


# Closed set of outcomes from _set_expected_count, so the call site's
# `status == "failed"` compare is type-checked rather than a free-form string.
SetExpectedStatus = Literal["ok", "skipped", "failed"]


def _set_expected_count(plan_path: str, count: int) -> tuple[SetExpectedStatus, str]:
    """Authoritatively write expected_url_count=count onto a plan's frontmatter.

    Used by ``decompose`` so a shared epic-decomposition doc graduates only
    after all N group PRs ship, not after the first. Runs the in-package
    ``fno.plan._stamp`` ``set-expected`` verb (the same sys.executable pattern
    as ``_graduate_plan``) to keep plan-frontmatter I/O in its single owner and
    this graph CLI graph-agnostic about frontmatter format.

    Returns ``(status, detail)`` where status is a ``SetExpectedStatus``:

    - ``"ok"``      - the count was written.
    - ``"skipped"`` - benign: the count could not be written for a reason that
      PROVABLY does NOT create the early-graduation risk: the base doc does not
      exist (set-expected exit 3). target also cannot stamp the doc at ship time,
      so it never graduates early. Mirrors ``_graduate_plan``'s best-effort,
      non-fatal philosophy. The caller proceeds silently.
    - ``"failed"``  - a real risk that must be surfaced: either the module RAN
      and reported a write failure on a doc it could read (e.g. malformed
      frontmatter; set-expected exit 1/2), OR the spawn itself raised. A spawn
      failure is INDETERMINATE - unlike an absent doc it does not prove the doc
      is unstampable at ship, so it could mask early graduation; the caller
      surfaces it as a loud, actionable stderr warning.

    The caller never rolls back the graph and never exits non-zero on any of
    these outcomes (group nodes are the source of truth, and a non-zero exit
    would break pipelines that call decompose for a best-effort stamp).
    """
    import subprocess
    try:
        result = subprocess.run(
            [
                sys.executable, "-m", "fno.plan._stamp", "set-expected",
                "--plan-path", plan_path,
                "--count", str(count),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            return "ok", ""
        # Exit 3 == base doc absent: benign (cannot be stamped at ship either).
        if result.returncode == 3:
            return "skipped", result.stderr.strip()
        # Any other non-zero means the module ran but could not write a doc it
        # could see (malformed frontmatter, etc.) - the real degradation.
        return "failed", result.stderr.strip() or f"exit {result.returncode}"
    except Exception as e:  # noqa: BLE001 - report any spawn failure to the caller
        # A spawn failure is indeterminate: unlike an absent doc it does not
        # prove the doc is unstampable at ship, so treat it as a surfaced
        # failure rather than a silent skip.
        return "failed", f"set-expected spawn failed: {e}"


# -- gh cross-check helpers (injectable for tests) --
# These module-level callables are replaced by test stubs via monkeypatch.

def _done_gh_query(pr_number, **kwargs):
    """Query gh for PR merge state. Delegates to reconcile's canonical helper."""
    from fno.graph._reconcile import query_pr_merge_state
    return query_pr_merge_state(pr_number, **kwargs)


def _done_gate_pipeline(
    task_id: str,
    node: dict,
    refs: list,
    *,
    force: bool,
    reason: Optional[str],
) -> Optional[str]:
    """The shared rich-completion gates (task 4.1): gh merge evidence,
    the forced-close journal, and the promise gate, with today's exit-code
    contract (3 refused / 4 outage / 5 awaiting merge / 6 promise unmet).
    Both completion front doors (``backlog done`` on either backend,
    the deprecated ``done`` spelling) run this BEFORE any close so neither can bypass the
    gates. Returns the evidencing PR url (None when no evidence).
    """
    from fno.graph._reconcile import (
        render_merge_evidence_failure,
        repo_slug_from_url,
        resolve_merge_evidence,
        resolve_promise_evidence,
    )

    # Usage guard lives in the shared terminal so every front door and every
    # backend gets it: the external dispatch reaches this pipeline before any
    # caller-side guard can fire.
    if force and not reason:
        typer.echo(
            "Error: --force requires --reason TEXT (explain why the cross-check is bypassed)",
            err=True,
        )
        raise typer.Exit(code=2)

    evidence_pr_url: Optional[str] = None
    if refs and not force:
        # There are PR references; require evidence before closing.
        first_pr_number, _ = refs[0]

        # Shared with `done_command` so the two paths cannot drift apart on what
        # counts as evidence.
        evidence = resolve_merge_evidence(
            refs, cwd=node.get("cwd"), query=_done_gh_query
        )
        evidence_found = evidence.outcome == "merged"
        if evidence_found:
            evidence_pr_url = evidence.pr_url
        else:
            if evidence.outcome == "awaiting_merge":
                typer.echo(
                    f"awaiting merge: PR #{evidence.open_pr_number} is OPEN, not merged. "
                    f"{task_id} stays in_review and closes on merge "
                    f"(reconcile / merge-triggered advance). "
                    f"Use --force --reason TEXT for an early close."
                    + (f" (note: {evidence.error})" if evidence.error else ""),
                    err=True,
                )
                raise typer.Exit(code=evidence.exit_code)

            if evidence.outcome == "outage":
                typer.echo(render_merge_evidence_failure(task_id, evidence, stays="open"), err=True)
                raise typer.Exit(code=evidence.exit_code)

            # Pure policy refusal - CLOSED-unmerged / UNKNOWN only.
            msg = evidence.reason or f"PR #{first_pr_number}: no merged evidence"
            if evidence.remedy:
                typer.echo(render_merge_evidence_failure(task_id, evidence, stays="open"), err=True)
            else:
                typer.echo(
                    f"Refused: {task_id} cross-check failed: {msg}\n"
                    f"Use --force --reason TEXT to bypass.",
                    err=True,
                )
            # Emit refusal event (best-effort)
            try:
                from fno import events as _evts
                event = _evts.backlog_done_refused(
                    node_id=task_id,
                    pr_number=first_pr_number,
                    reason=msg,
                )
                _evts.append_event(event)
            except Exception:
                pass
            raise typer.Exit(code=evidence.exit_code)

    # -- Step 3: Force path - proceed and journal loudly --
    if force and refs:
        assert reason is not None  # the `--force requires --reason` guard above ensures this
        first_pr_number, first_pr_url = refs[0]
        # A forced close still names a PR; stamp the plan against it so the ship
        # is recorded even when the cross-check was bypassed (ab-bd9f476c).
        evidence_pr_url = first_pr_url
        pr_repo = repo_slug_from_url(first_pr_url)
        # Best-effort: try to read the current PR state for journaling
        try:
            force_pr_state_obj = _done_gh_query(first_pr_number, repo=pr_repo)
            force_pr_state = force_pr_state_obj.state
        except Exception:
            force_pr_state = "UNKNOWN"
        typer.echo(
            f"Warning: force-closing {task_id} (reason: {reason}). "
            f"PR #{first_pr_number} state={force_pr_state}.",
            err=True,
        )
        # Emit forced-close event (best-effort)
        try:
            from fno import events as _evts
            event = _evts.backlog_done_forced(
                node_id=task_id,
                force_reason=reason,
                pr_number=first_pr_number,
                pr_state=force_pr_state,
            )
            _evts.append_event(event)
        except Exception:
            pass
    elif force:
        # --force with no refs: just log (advisory node)
        typer.echo(
            f"Warning: force flag set on advisory node {task_id} (reason: {reason}); no PR refs to check.",
            err=True,
        )

    # -- Step 3b: promise gate (x-5d34) --
    # The merge gate asked "is a PR merged"; this asks "did the plan's declared
    # work all ship". Skipped under --force so a deliberate half-ship stays a
    # journaled line (the backlog_done_forced event above) rather than silence.
    if not force:
        promise = resolve_promise_evidence(
            node, cwd=node.get("cwd"), query=_done_gh_query
        )
        if promise.outcome == "promise_unmet":
            typer.echo(promise.reason, err=True)
            raise typer.Exit(code=promise.exit_code)
        if promise.warning:
            typer.echo(f"warning: {promise.warning}", err=True)
    return evidence_pr_url


def _cascade_close_external_parents(tracker, child_id: str) -> list[str]:
    """Close ancestor containers whose children are now ALL closed - the
    external twin of ``_cascade_close_parents``: sibling state from ONE
    list_open, the chain from tracker reads, each close best-effort."""
    closed: list[str] = []
    try:
        open_children: dict[str, list[str]] = {}
        for cand in tracker.list_open():
            if cand.parent:
                open_children.setdefault(cand.parent, []).append(cand.id)
        cur = tracker.read(child_id).parent
        seen: set[str] = set()
        while cur and cur not in seen:
            seen.add(cur)
            if open_children.get(cur):
                break  # still has open children
            try:
                parent_node = tracker.read(cur)
            except Exception:  # noqa: BLE001 - unknown ancestor ends the walk
                break
            if str(parent_node.state.value) == "closed":
                cur = parent_node.parent
                continue
            try:
                tracker.close(cur)
                closed.append(cur)
            except Exception as exc:  # noqa: BLE001 - cascade is best-effort
                typer.echo(f"warning: cascade close failed for {cur}: {exc}", err=True)
                break
            cur = parent_node.parent
    except Exception as exc:  # noqa: BLE001 - cascade never fails the close
        typer.echo(f"warning: cascade evaluation failed: {exc}", err=True)
    return closed


def _done_via_seam(
    task_id: str, *, skip_stamp: bool, force: bool, reason: Optional[str]
) -> None:
    """Rich completion under an external backend (task 4.1, AC7/AC8).

    The same shared gate pipeline runs first; footnote-owned rollups persist
    to the sidecar BEFORE the irreversible close; then
    ``get_tracker().close(task_id)`` runs exactly once and success prints only
    after it returns. A failed external close is loud and retryable - the
    item stays open. Plan stamping and the ancestor cascade ride the same
    seam (tracker parent edges, sidecar plan_path)."""
    from fno.graph._reconcile import node_pr_refs
    from fno.tracker import get_tracker
    from fno.tracker import sidecar as sidecar_store
    from fno.tracker.types import NodeNotFound

    try:
        tnode, sc = _read_external_node_and_sidecar(task_id)
    except NodeNotFound:
        typer.echo(f"Error: feature {task_id} not found", err=True)
        raise typer.Exit(code=1)
    if str(tnode.state.value) == "closed":
        typer.echo(f"{task_id} is already done", err=True)
        return

    tracker = get_tracker()
    row = {
        "id": task_id, "title": tnode.title, "cwd": sc.cwd,
        "plan_path": sc.plan_path, "pr_number": sc.pr_number,
        "pr_url": sc.pr_url, "additional_prs": sc.additional_prs,
        "sessions": sc.sessions,
        # The rollup reads containment off the node (a contained child claims
        # no cost); dropping it here would double-count the delivery unit.
        "contained_in": sc.contained_in,
    }
    refs = node_pr_refs(row)
    evidence_pr_url = _done_gate_pipeline(
        task_id, row, refs, force=force, reason=reason
    )

    # Footnote-owned rollups BEFORE the close (one physical owner: the sidecar).
    try:
        from fno.done.cli import _rollup_from_ledger

        rollup = _rollup_from_ledger(row)
    except Exception:  # noqa: BLE001 - rollup is fill-only, never blocks
        rollup = {}
    if rollup.get("cost_usd") is not None and sc.cost_usd is None:
        sc.cost_usd = rollup["cost_usd"]
    if rollup.get("cost_sessions") and not sc.cost_sessions:
        sc.cost_sessions = list(rollup["cost_sessions"])
    if evidence_pr_url and sc.pr_url is None:
        sc.pr_url = evidence_pr_url
    sidecar_store.save(sc)

    # The one close. Failure keeps the item open and retryable (AC8-ERR).
    try:
        tracker.close(task_id)
    except Exception as exc:  # noqa: BLE001 - name backend + id, fail loud
        typer.echo(
            f"Error: external close failed for {task_id} on backend "
            f"{tracker.name!r}: {exc}\n"
            "The item stays open; retry once the backend is available.",
            err=True,
        )
        raise typer.Exit(code=1)

    _cascade_close_external_parents(tracker, task_id)
    typer.echo(f"Marked {task_id} done")

    # Closure releases the node claim at the SEAM, right after the one close,
    # so every tracker backend inherits it (github today; the graph backend
    # gets the same release from the store's closure hook, which an external
    # close never reaches). Placing it in one backend's close() would be a
    # guard on one of N reachable implementations.
    from fno.graph.store import release_node_claim_at_closure

    release_node_claim_at_closure(task_id, rung="done")

    if sc.plan_path and not skip_stamp:
        _stamp_and_graduate_plan(sc.plan_path, url=evidence_pr_url, session_id=None)

    # Retro-at-done lifecycle trigger (x-122a): same non-fatal posture as the
    # graph path; the seam row carries what the resolver needs.
    try:
        from fno.provenance.spawn_think import on_node_retro

        on_node_retro(row)
    except Exception:  # noqa: BLE001 - additive; never wedge the close
        pass


@cli.command(
    "done",
    epilog="Paired verb: `fno backlog reopen <id> --reason ...` reverses this "
    "(hidden; run its own --help). Related: `fno backlog reconcile` closes nodes "
    "whose PR merged outside the gate (hidden).",
)
def cmd_done(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    skip_stamp: bool = typer.Option(
        False,
        "--skip-stamp",
        help="Skip plan stamp even if plan_path is set",
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help="Bypass gh cross-check. Requires --reason.",
    ),
    reason: Optional[str] = typer.Option(
        None,
        "--reason",
        "-R",
        help="Required when --force is used. Explains why the cross-check is bypassed.",
    ),
) -> None:
    """Mark a node complete.

    Sets ``completed_at`` to an ISO timestamp; ``recompute_statuses`` derives
    ``status: done`` from that field and unblocks any dependents.

    Before mutation, a gh cross-check verifies that at least one referenced PR
    is MERGED (x-aba7: graph done = merged, uniformly). An OPEN PR is NOT
    closing evidence - the node is awaiting merge and closes on the actual
    merge via reconcile / merge-triggered advance. CI state is irrelevant to
    the close decision.

    Exit codes:
        0  success (node closed)
        1  validation error (bad id, node not found)
        2  usage error (--force without --reason)
        3  gh cross-check refused: CLOSED-unmerged / UNKNOWN, no merge evidence
           (retryable when the PR merges; walker treats this as Parked)
        4  gh outage: subprocess failure / timeout / parse error; retryable
        5  awaiting merge: PR OPEN, not merged; node stays in_review
           (success-shaped; close lands via reconcile/advance at merge)
        6  promise unmet: plan promised work that has not all shipped
           (multi-wave with no assertion, a failed close_probe, or fewer
           merged ships than expected_url_count). Use --force --reason to
           record a deliberate half-ship.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph, read_graph
    from fno.graph._intake import _find_node
    from fno.graph._reconcile import node_pr_refs

    # External backend (task 4.1): the shared gates then exactly one
    # tracker.close. The local <prefix>-<hex> grammar guard does not apply to
    # an opaque external id - resolution is the tracker's exact read.
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        _done_via_seam(task_id, skip_stamp=skip_stamp, force=force, reason=reason)
        return

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    # Usage guard: --force requires --reason
    if force and not reason:
        typer.echo(
            "Error: --force requires --reason TEXT (explain why the cross-check is bypassed)",
            err=True,
        )
        raise typer.Exit(code=2)

    # -- Step 1: Idempotency + node-lookup read (outside the lock) --
    # We must discover idempotency and PR refs before acquiring the lock, so
    # that gh I/O (which can be slow) never blocks other graph mutations.
    entries = read_graph(_graph_path())
    node = _find_node(entries, task_id)
    if not node:
        typer.echo(f"Error: feature {task_id} not found", err=True)
        raise typer.Exit(code=1)

    # Idempotency first: already done -> short-circuit with NO gh read (AC4-EDGE)
    if node.get("completed_at"):
        typer.echo(f"{task_id} is already done", err=True)
        return

    # -- Step 2: gh cross-check (outside the lock) --
    refs = node_pr_refs(node)

    # Shared rich-completion gates (task 4.1): both front doors and both
    # backends run the identical evidence/promise pipeline before any close.
    # The return is the PR url that evidences the close, captured so the plan
    # stamp records the actual ship; None when there is no PR ref / no evidence.
    evidence_pr_url = _done_gate_pipeline(
        task_id, node, refs, force=force, reason=reason
    )


    # -- Step 4: Mutation under the lock --
    plan_path_out: list = [None]
    already_holder: list = [False]
    cascade_closed_out: list = []

    # Cost stamp (Wave 2.2): the ledger has per-plan cost the node never captured
    # (2-3 fills). Aggregate it outside the lock; ledger absent/rowless -> null,
    # never blocks the close. Reuses the same rollup the done root uses.
    cost_rollup: dict = {}
    try:
        from fno.done.cli import _rollup_from_ledger

        cost_rollup = _rollup_from_ledger(node)
    except Exception:
        cost_rollup = {}

    def mutator(entries):
        n = _find_node(entries, task_id)
        if not n:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        existing = n.get("completed_at") or ""
        # Idempotent: a prior real completion is a no-op.
        if existing:
            already_holder[0] = True
            return entries
        _apply_completion_fields(n, merge_status="merged" if evidence_pr_url else None)
        # Fill-only: never overwrite a cost a richer path (e.g. the done root)
        # already stamped, and don't drop rows appended during the run.
        if cost_rollup.get("cost_usd") is not None and not n.get("cost_usd"):
            n["cost_usd"] = cost_rollup["cost_usd"]
        if cost_rollup.get("cost_sessions") and not n.get("cost_sessions"):
            n["cost_sessions"] = cost_rollup["cost_sessions"]
        # Close any now-all-done ancestor epic (x-33b2): the box is done when its
        # children are, and it carries no PR of its own to close it explicitly.
        cascade_closed_out.extend(_cascade_close_parents(entries, task_id))
        plan_path_out[0] = n.get("plan_path")
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    if already_holder[0]:
        typer.echo(f"{task_id} is already done", err=True)
        return

    typer.echo(f"Marked {task_id} done")

    # Operator-authority matrix (LD3/LD29): `fno backlog done` is an allowed
    # action during a drive window, but audit-tag it so the trail attributes
    # the completion to the operator rather than the LLM. Best-effort.
    try:
        from fno.drive_authority import (
            emit_operator_initiated,
            is_drive_authority_active,
        )

        if is_drive_authority_active():
            emit_operator_initiated(
                "backlog_done_operator_initiated",
                source="backlog",
                task_id=task_id,
            )
    except Exception:
        pass

    if plan_path_out[0] and not skip_stamp:
        # Stamp the plan shipped (against the evidencing PR) THEN graduate, so a
        # plan that never went through target's ship gate still records the ship
        # rather than getting a graduate no-op (ab-bd9f476c).
        _stamp_and_graduate_plan(
            plan_path_out[0],
            url=evidence_pr_url,
            session_id=node.get("session_id"),
        )

    # Project the closed node + any cascade-closed epic parents onto their plans
    # (forward-only, stamps done_at) AFTER the stamp above, so the primary plan's
    # shipped_at is written before its done_at (never done-before-shipped). The
    # primary is already `done` here, so this is a no-op on it and its real job
    # is the cascade-closed epic parents that _stamp_and_graduate_plan skips.
    # --skip-stamp suppresses ALL plan writes, projection included.
    if not skip_stamp:
        _project_plans_from_graph([task_id, *cascade_closed_out])

    # A2 (x-122a): retro-at-done lifecycle trigger. Dispatch a `retro` context
    # /think while the closed node's session context is still resolvable. Gated
    # by config.think_spawn.on_retro (default OFF) and strictly non-fatal: a
    # dispatch failure never unwinds the close it rode in on.
    try:
        from fno.provenance.spawn_think import on_node_retro

        on_node_retro(node)
    except Exception:  # noqa: BLE001 - additive; never wedge `done`
        pass


# -- reconcile (close merged-PR drift) --

def _run_advance_epic(
    epic: str,
    *,
    stop: bool,
    max_dispatch: Optional[int],
    json_out: bool,
    verbose: bool,
    model: Optional[str],
    provider: Optional[str],
    continuation: bool = False,
) -> None:
    """Run the epic advance and render its receipt (x-9608 K1).

    Refusals (no-such-node / not-a-container) exit non-zero: unlike the
    merge-advance path (a dispatch decision is never an error), an operator naming
    a bad node to --epic wants a clear failure. Everything else exits 0.

    ``continuation`` is the K2 daemon-drain mode (never reactivate; retire an
    inactive mission).
    """
    from fno.backlog.advance import advance_epic

    try:
        result = advance_epic(
            epic, stop=stop, max_dispatch=max_dispatch,
            verbose=verbose, model=model, provider=provider,
            continuation=continuation,
        )
    except Exception as exc:  # noqa: BLE001 - the epic advance itself is non-fatal per-child
        typer.echo(f"advance --epic: unexpected error (non-fatal): {exc}", err=True)
        raise typer.Exit(code=0)

    if json_out:
        typer.echo(json.dumps({
            "epic_id": result.epic_id,
            "error": result.error,
            "activated": result.activated,
            "deactivated": result.deactivated,
            "all_done": result.all_done,
            "dispatched": list(result.dispatched),
            "children": [
                {"node_id": r.node_id, "decision": r.decision, "reason": r.reason,
                 "short_id": r.short_id}
                for r in result.child_results
            ],
        }, indent=2))
    else:
        if result.error:
            typer.echo(f"epic {result.epic_id}: {result.error}", err=True)
        elif result.deactivated:
            reason = "complete" if result.all_done else "stopped"
            typer.echo(f"epic {result.epic_id}: mission deactivated ({reason})")
        else:
            n = len(result.dispatched)
            skips = [r for r in result.child_results if r.decision == "skipped"]
            fails = [r for r in result.child_results if r.decision == "failed"]
            typer.echo(
                f"epic {result.epic_id}: dispatched {n}"
                + (f", skipped {len(skips)}" if skips else "")
                + (f", failed {len(fails)}" if fails else "")
            )

    # A refusal (bad node) is the only non-zero exit; a per-child failure is a
    # loud receipt, not a verb error.
    if result.error in ("no-such-node", "not-a-container"):
        raise typer.Exit(code=1)


# -- reopen --
#
# The inverse of `done`, and a deliberate inversion of its gate: `done` refuses
# when no referenced PR is merged, `reopen` refuses when one IS. Both gates ask
# the same question of the same evidence and disagree only about which answer
# permits the transition, which is what makes them a pair rather than two verbs
# that happen to touch the same field.


def _archived_entry(node_id: str) -> Optional[dict]:
    """The node's row in graph-archive.json, or None. Read-only, never raises.

    Reopen needs this to tell "archived" apart from "absent". Without it an
    archived node reports "not found", which is the same message a typo gets,
    while the node sits readable in the sibling file - an absence with two
    explanations and no way to distinguish them.
    """
    from fno.graph._intake import _find_node
    from fno.graph.store import read_graph

    try:
        # The archive is default-backend storage: never consulted behind an
        # external selection (the caller's refusal already fired; this guard
        # keeps the helper honest for any future caller).
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            return None
        # `_archive_path`, not a second accessor: cmd_archive and cmd_unarchive
        # already route through it, and a helper that resolves the archive its
        # own way is a second path that drifts on the first config change.
        path = _archive_path()
        if not path.exists():
            return None
        # `_find_node`, not an exact compare: it is what resolved the id against
        # the WORKING graph a line earlier, and a stricter match here recreates
        # the very ambiguity this helper exists to remove. An exact compare made
        # `reopen ab-9728` report "not found" for an archived `ab-9728f3c1`,
        # which is the same message a typo gets.
        return _find_node(read_graph(path), node_id)
    except Exception:  # noqa: BLE001 - the archive is advisory; a bad read must not mask the real refusal
        return None


def _evidence_pr_number(evidence, refs: list) -> Optional[int]:
    """The PR number that produced ``evidence``'s outcome, not merely the first ref.

    ``resolve_merge_evidence`` reports an outcome derived from ANY ref, so the
    receipt has to name the ref that actually carries it: the merged PR's number
    on a merge, the open one on awaiting_merge. Falling back to ``refs[0]``
    everywhere let a node whose primary #41 was closed and whose additional #42
    merged emit ``pr_number: 41, pr_state: MERGED`` - a receipt pointing an
    auditor at the wrong PR.
    """
    if evidence.outcome == "merged" and evidence.pr_url:
        for number, url in refs:
            if url == evidence.pr_url:
                return number
    if evidence.outcome == "awaiting_merge" and evidence.open_pr_number is not None:
        return evidence.open_pr_number
    return refs[0][0] if refs else None


def _cascade_reopen_parents(entries: list[dict], node_id: str) -> tuple[list[str], list[str]]:
    """Reopen ancestor epics the cascade auto-closed. Returns (reopened, warned).

    The inverse of :func:`_cascade_close_parents`, and not a refusal, because a
    done epic with a live child is not a risky state to correct - it is an
    inconsistent one. The epic's work IS its children; one of them is open again.

    The judgment call is WHICH ancestors. An epic closed by the cascade carries
    the ``auto-closed:`` note :func:`_auto_closed_note` wrote, so reopening it
    just restores what the cascade would compute today. An epic closed WITHOUT
    that note was closed on its own evidence - a real PR, an operator decision -
    and silently reopening it would discard a judgment this verb never made. So
    those are left done and NAMED, which is the refuse-and-say-why rule applied
    to a case where either silent choice is wrong.

    Walks up under the same 64-deep cap the close path uses, for the same reason.
    """
    id_to_entry = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    reopened: list[str] = []
    warned: list[str] = []
    cur = id_to_entry.get(node_id)
    for _ in range(64):  # depth cap: guards against a malformed parent cycle
        pid = cur.get("parent") if isinstance(cur, dict) else None
        if not isinstance(pid, str):
            break
        parent = id_to_entry.get(pid)
        if parent is None or not parent.get("completed_at"):
            break  # missing or already-open ancestor -> stop this branch
        note = str(parent.get("completion_note") or "")
        if not note.startswith("auto-closed:"):
            warned.append(pid)
            break  # closed on its own evidence; stop rather than climb past it
        _clear_completion_fields(parent, reason=f"child {node_id} reopened")
        reopened.append(pid)
        cur = parent
    return reopened, warned


@cli.command(
    "reopen",
    hidden=True,
    epilog="Reverses `done` (and a close made by `reconcile`). Softer options: "
    "`update` to correct a field without changing status, `note` to record "
    "something the close missed.",
)
def cmd_reopen(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    reason: str = typer.Option(
        ...,
        "--reason",
        "-R",
        help="Why the node is being reopened. Required: a close is evidenced by "
        "a merged PR, a reopen by nothing but your judgment.",
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help="Reopen even though a referenced PR is MERGED. Records a deliberate "
        "reopen of shipped work.",
    ),
) -> None:
    """Clear a node's completion, returning it to its underlying state.

    Every other lifecycle transition had an inverse (``defer``/``undefer``,
    ``supersede``/``unsupersede``, ``queue``/``unqueue``); ``done`` was terminal
    with none, so a node closed in error was corrected by hand-editing
    ``graph.json``, which a PreToolUse hook forbids for good reason.

    Refuses when a referenced PR is MERGED. That is ``done``'s gate inverted:
    the work is in main, and clearing the completion would make the graph assert
    that shipped work did not ship. The remedy is almost always to file the
    remaining work as its own node (``fno backlog idea``) rather than to reopen
    the record of the part that landed. ``--force`` records a deliberate reopen
    of shipped work, and is journaled as such.

    Ancestor epics the cascade auto-closed when this node closed are reopened
    alongside it, since an epic is done exactly when its children are. An epic
    closed on its own evidence is left done and named on stderr, because
    reopening it would discard a judgment this verb never made.

    Reopening does not reclaim, un-defer, or un-queue the node; see
    :func:`_clear_completion_fields` for what it deliberately leaves alone.

    Exit codes:
        0  success (node reopened), or a no-op warning on a node that is not done
        1  validation error (bad id, node not found in the graph or the archive)
        2  usage error (blank --reason)
        3  refused: a referenced PR is MERGED. Use --force --reason to override
        4  archived, or a gh outage. Both are retryable: unarchive first, or
           retry once gh is reachable. The node stays done either way
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph, read_graph
    from fno.graph._intake import _find_node
    from fno.graph._reconcile import (
        node_pr_refs,
        render_merge_evidence_failure,
        resolve_merge_evidence,
    )

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    # Validate at the CLI boundary the way cmd_defer validates its own reason, so
    # a direct call cannot land a reasonless reopen that the event then records
    # as an empty string.
    cleaned_reason = reason.strip()
    if not cleaned_reason:
        typer.echo("Error: --reason cannot be blank", err=True)
        raise typer.Exit(code=2)

    # -- Step 1: locate the node (outside the lock, like cmd_done) --
    entries = read_graph(_graph_path())
    node = _find_node(entries, task_id)
    if not node:
        archived = _archived_entry(task_id)
        if archived is not None:
            when = archived.get("completed_at") or archived.get("updated") or "unknown"
            typer.echo(
                f"Refused: {task_id} is archived (terminal since {when}), not in the "
                f"working graph. Run `fno backlog unarchive {task_id}` first, then "
                f"reopen it.",
                err=True,
            )
            raise typer.Exit(code=4)
        typer.echo(f"Error: feature {task_id} not found", err=True)
        raise typer.Exit(code=1)

    if not node.get("completed_at"):
        # Idempotent in the safe direction, matching cmd_unsupersede's
        # "was not superseded": there is nothing to reverse, and touching the
        # node's other fields to express that would be a mutation nobody asked
        # for.
        typer.echo(f"warning: {task_id} is not done; nothing to reopen", err=True)
        return

    # -- Step 2: the merged-PR gate (outside the lock, like cmd_done's) --
    #
    # Through `resolve_merge_evidence`, the SAME resolver `cmd_done` uses, and
    # over ALL refs rather than the primary. That is what makes this the same
    # gate inverted rather than a similar-looking one: a node can close on a
    # merged `additional_prs` entry while its primary `pr_number` sits closed
    # and unmerged, and a reopen that only queried the primary would permit
    # exactly the case the refusal exists to catch. It also carries the node's
    # `cwd`, which is how a foreign-repo ref reaches the right repository
    # instead of resolving PR #N in whatever checkout happens to be current.
    refs = node_pr_refs(node)
    pr_number: Optional[int] = None
    pr_state: Optional[str] = None
    # True only when --force actually bypassed the merged-PR refusal. The event
    # schema defines `forced` as "the work is in main and was reopened anyway",
    # so deriving it from the flag's presence would stamp that claim on an
    # ordinary --force reopen of a node with no PR, an open PR, or an
    # unreachable gh - a false audit receipt on the one field an auditor reads
    # to find the risky reopens.
    bypassed_merged = False
    if refs:
        evidence = resolve_merge_evidence(
            refs, cwd=node.get("cwd"), query=_done_gh_query
        )
        # Pair the number with the state it describes. The aggregate outcome can
        # come from any ref, so recording refs[0] beside a MERGED read from
        # additional_prs[0] would name PR #41 as the merge evidence when #42 is
        # what merged - the receipt pointing at the wrong PR.
        pr_number = _evidence_pr_number(evidence, refs)
        if evidence.outcome == "outage" and not force:
            typer.echo(render_merge_evidence_failure(task_id, evidence, stays="done"), err=True)
            raise typer.Exit(code=4)
        if evidence.outcome == "merged":
            pr_state = "MERGED"
            # The ref that actually evidences the merge, which is not
            # necessarily the primary - the message has to name the PR the
            # operator would go look at.
            merged_url = evidence.pr_url or ""
            if not force:
                typer.echo(
                    f"Refused: a referenced PR is MERGED, so {task_id}'s work is in "
                    f"main{f' ({merged_url})' if merged_url else ''}. "
                    f"Reopening would make the graph assert that shipped work did not ship.\n"
                    f"  If work remains, file it: fno backlog idea \"<what is left>\"\n"
                    f"  If the close itself was wrong: reopen --force --reason \"...\"",
                    err=True,
                )
                raise typer.Exit(code=3)
            bypassed_merged = True
            typer.echo(
                f"Warning: force-reopening {task_id} (reason: {cleaned_reason}). "
                f"A referenced PR is MERGED"
                f"{f' ({merged_url})' if merged_url else ''}.",
                err=True,
            )
        elif evidence.outcome == "awaiting_merge":
            pr_state = "OPEN"
        elif evidence.failure_kind:
            typer.echo(render_merge_evidence_failure(task_id, evidence, stays="done"), err=True)
            raise typer.Exit(code=evidence.exit_code)
        else:
            pr_state = "UNKNOWN"

    # -- Step 3: mutation under the lock --
    cascade_out: list[str] = []
    warned_out: list[str] = []
    canonical_id_box: list[str] = [task_id]
    raced_box: list[bool] = [False]

    def mutator(entries):
        n = _find_node(entries, task_id)
        if not n:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        if not n.get("completed_at"):
            # Raced with another reopen; the safe direction wins. Flagged rather
            # than silently returning, because the caller must NOT go on to
            # print a success line and emit a backlog_reopened event carrying
            # this reason for a mutation it did not make.
            raced_box[0] = True
            return entries
        # The CANONICAL id, not the argument: _find_node resolves a partial id
        # (`ab-9728` for `ab-9728f3c1`), while the cascade walks a parent map
        # keyed on full ids. Passing the argument through would make the cascade
        # silently find nothing for exactly the callers who typed the short form,
        # leaving an auto-closed epic done over a live child. cmd_unsupersede
        # carries the same box for the same reason.
        canonical_id_box[0] = n.get("id") or task_id
        _clear_completion_fields(n, reason=cleaned_reason)
        reopened, warned = _cascade_reopen_parents(entries, canonical_id_box[0])
        cascade_out.extend(reopened)
        warned_out.extend(warned)
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    if raced_box[0]:
        typer.echo(
            f"warning: {task_id} was reopened by another writer; nothing to do",
            err=True,
        )
        return

    for pid in warned_out:
        typer.echo(
            f"warning: parent {pid} is done on its own evidence and now has an open "
            f"child; `fno backlog reopen {pid} --reason \"...\"` if that is wrong",
            err=True,
        )
    typer.echo(
        f"Reopened {canonical_id_box[0]}"
        + (f" (cascade: {', '.join(cascade_out)})" if cascade_out else "")
    )

    try:
        from fno import events as _evts

        _evts.append_event(
            _evts.backlog_reopened(
                node_id=canonical_id_box[0],
                reason=cleaned_reason,
                forced=bypassed_merged,
                pr_number=pr_number,
                pr_state=pr_state,
                cascade_reopened=cascade_out,
            )
        )
    except Exception:  # noqa: BLE001 - the graph is already correct; a failed emit is not a failed reopen
        pass

    # Force the plan doc off terminal `done`, then recompute. Both halves are
    # cmd_unsupersede's tail and both are needed for the same reasons: the
    # forward-only projector will not leave a terminal on its own, and the graph
    # status was derived during the mutation while the plan still read `done`.
    for nid in [canonical_id_box[0], *cascade_out]:
        _project_plans_from_graph([nid], force_status_off_terminal_for=nid)
    locked_mutate_graph(_graph_path(), lambda entries: entries)


@cli.command("advance", hidden=True)
def cmd_advance(
    closed: Optional[str] = typer.Option(
        None,
        "--closed",
        help="The just-merged node id whose close triggered this advance (AC1-RACE keying).",
    ),
    epic: Optional[str] = typer.Option(
        None,
        "--epic",
        help="Advance (converge) an epic mission: fan out its ready leaf children across all projects (x-9608 K1). Mutually exclusive with --closed.",
    ),
    stop: bool = typer.Option(
        False,
        "--stop",
        help="With --epic: deactivate the mission (clear mission_active) and dispatch nothing.",
    ),
    continuation: bool = typer.Option(
        False,
        "--continuation",
        hidden=True,
        help="With --epic: K2 daemon-drain mode - never (re)activate the mission; retire an already-inactive one (dispatches nothing, reports deactivated).",
    ),
    max_dispatch: Optional[int] = typer.Option(
        None, "--max",
        help="With --epic: cap the total workers this epic advance dispatches (per-project cap is config.parallel.max_lanes).",
    ),
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Restrict next-node selection to this project."
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit the decision as JSON."
    ),
    verbose: bool = typer.Option(
        False, "--verbose", help="Print the dispatch decision to stderr."
    ),
    model: Optional[str] = typer.Option(
        None, "--model", "-m",
        help="Pin a model for the dispatched worker(s), overriding node annotations.",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider",
        help="Pin a provider for the dispatched worker(s). (No -p short: it is --project here.)",
    ),
) -> None:
    """Dispatch a fresh /target --no-merge worker for the next now-unblocked node.

    Merge-triggered auto-continue (ab-3cd195b6). Opt-in and non-fatal: when
    auto-continue is disabled it emits advance_skipped{disabled} and dispatches
    nothing. Driven by the merge event (reconcile / post-merge), so megawalk,
    /target, and /megatron all inherit it without driver-specific code. Always
    exits 0 (a dispatch decision is never an error to the host op).

    ``--epic <id>`` switches to the epic advance / converge path (x-9608 K1):
    mark the epic's mission active and fan out every currently-ready LEAF child
    across all projects. Idempotent; respects config.parallel.max_lanes per
    project + ``--max`` overall. ``--stop`` deactivates instead.
    """
    from fno.dispatch_flags import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )
    from fno.backlog.advance import advance as _advance
    from fno.backlog.advance import advance_dependents as _advance_deps

    # Validate the dispatch pins before any spawn; provider is resolved only when
    # given so an absent pin lets the spawn path keep its per-node/default choice.
    try:
        model = reject_empty_model(model)
        provider = resolve_dispatch_provider(provider)[0] if provider is not None else None
    except DispatchFlagError as exc:
        typer.echo(f"advance: {exc}", err=True)
        raise typer.Exit(code=2)

    # --epic routes to the epic-advance path; it is a distinct trigger from the
    # merge-advance --closed path (they never combine on one call).
    if epic is not None:
        if closed is not None:
            typer.echo("advance: --epic and --closed are mutually exclusive", err=True)
            raise typer.Exit(code=2)
        _run_advance_epic(
            epic, stop=stop, max_dispatch=max_dispatch, json_out=json_out,
            verbose=verbose, model=model, provider=provider,
            continuation=continuation,
        )
        return
    if stop or max_dispatch is not None or continuation:
        typer.echo("advance: --stop / --max / --continuation require --epic", err=True)
        raise typer.Exit(code=2)

    # RC2 (x-33b2): closed_project is the CLOSED NODE's own project, read from the
    # graph - NEVER the --project next-selection flag. --project restricts which
    # project advance() picks `next` from; it is normally OMITTED on a manual
    # `advance --closed A`, which left closed_project=None and defeated
    # advance_dependents' same-project guard, misrouting a same-project dependent
    # through the cross-project --cwd path onto a protected branch where the bg
    # worker dies. Mirror the reconcile path (cli.py reads the node's .project).
    closed_project: Optional[str] = None
    if closed:
        try:
            from fno.graph._intake import _find_node
            from fno.graph.store import read_graph

            _cn = _find_node(read_graph(_graph_path()), closed)
            closed_project = _cn.get("project") if _cn else None
        except Exception:  # noqa: BLE001 - non-fatal; advance_deps fails closed on None
            closed_project = None

    try:
        result = _advance(
            closed_node_id=closed, project=project, verbose=verbose,
            model=model, provider=provider,
        )
        # G1 (AC5-FR): follow this node's blocked_by edges into OTHER projects.
        # Only meaningful with --closed (an edge source); the project-scoped
        # next selection above never reaches a foreign dependent. Shares the
        # dispatch:<id> dedup with reconcile's call so a node seen by both the
        # reconcile sweep and this explicit verb dispatches at most once.
        if closed:
            _advance_deps(
                closed_node_id=closed, closed_project=closed_project, verbose=verbose,
                model=model, provider=provider,
            )
            # G4: route the closed node's contract dependents to a reconcile pass
            # (or a pending sentinel). Shares the dispatch:<id> dedup with the two
            # advance paths so a node seen by all three dispatches at most once.
            from fno.backlog.reconcile_dispatch import dispatch_reconcile_for_blocker
            dispatch_reconcile_for_blocker(closed_node_id=closed, verbose=verbose)
    except Exception as exc:  # noqa: BLE001 - the contract is "always exits 0"
        # advance() is designed non-fatal (every path emits + returns), but the
        # CLI entrypoint must never traceback on an unforeseen escape: a dispatch
        # decision is not an error to whoever invoked the verb. Report on stderr
        # and exit 0.
        typer.echo(f"advance: unexpected error (non-fatal): {exc}", err=True)
        return
    if json_out:
        typer.echo(
            json.dumps(
                {
                    "decision": result.decision,
                    "event": result.event,
                    "reason": result.reason,
                    "node_id": result.node_id,
                    "short_id": result.short_id,
                },
                indent=2,
            )
        )
    else:
        parts = [result.decision]
        if result.node_id:
            parts.append(result.node_id)
        if result.reason:
            parts.append(f"reason={result.reason}")
        if result.short_id:
            parts.append(f"short_id={result.short_id}")
        typer.echo(" ".join(parts))


@cli.command("reconcile-findings", hidden=True)
def cmd_reconcile_findings(
    apply: bool = typer.Option(
        False,
        "--apply",
        help="Close the addressed nodes (default: dry-run, mutate nothing).",
    ),
) -> None:
    """Close phantom retro-triage nodes a later commit already addressed.

    Retro files a node from a reviewer comment; on an autonomously-merged,
    bot-reviewed PR the fix often lands after the comment without the thread
    being resolved or replied to, so the node is filed for work already done
    (x-632c). This re-runs the harvest addressed-detection against each open
    retro node's source PR and closes the ones now addressed - the
    reconciliation counterpart to the harvest-side suppression. Dry-run by
    default; ``--apply`` closes via ``fno backlog done --force``. A PR whose
    review state can't be read is skipped, never closed on uncertainty.
    """
    import subprocess

    from fno.graph.store import read_graph
    from fno.retro.reconcile_findings import scan_addressed_findings

    entries = read_graph(_graph_path())
    warnings: list = []
    findings = scan_addressed_findings(entries, warnings=warnings)
    for w in warnings:
        typer.echo(w, err=True)

    if not findings:
        typer.echo("reconcile-findings: no addressed phantom retro nodes found")
        return

    for f in findings:
        typer.echo(f"{f.node_id}  PR #{f.pr_number}  comment {f.comment_id}  ({f.signal})")

    if not apply:
        typer.echo(
            f"\n{len(findings)} node(s) would close. Re-run with --apply to close them."
        )
        return

    closed = 0
    for f in findings:
        reason = (
            f"addressed on PR #{f.pr_number} ({f.signal}); retro reconcile-findings "
            f"re-check - fix landed without the thread being resolved/replied"
        )
        proc = subprocess.run(
            ["fno", "backlog", "done", f.node_id, "--force", "--reason", reason]
        )
        if proc.returncode == 0:
            closed += 1
        else:
            typer.echo(
                f"reconcile-findings: close of {f.node_id} failed (rc={proc.returncode})",
                err=True,
            )
    typer.echo(f"reconcile-findings: closed {closed}/{len(findings)} node(s)")


@cli.command(
    "reconcile",
    hidden=True,
    epilog="Paired verb: `fno backlog reopen <id> --reason ...` reverses a close "
    "this made. It refuses on a merged PR, which is what closed the node here, so "
    "an intentional correction of an auto-close needs --force.",
)
def cmd_reconcile(
    dry_run: bool = typer.Option(
        False,
        "--dry-run", "-N",
        help="Report candidates only; mutate nothing (graph stays byte-identical).",
    ),
    node: Optional[str] = typer.Option(
        None,
        "--node",
        help="Restrict the scan to a single node id (ab-XXXXXXXX).",
    ),
    json_out: bool = typer.Option(
        False,
        "--json", "-J",
        help="Emit structured JSON instead of a human summary.",
    ),
    pr_number: Optional[int] = typer.Option(
        None,
        "--pr-number",
        help="Bind every node named in this merged PR's exact Backlog-Closure "
        "trailer to the PR (filling an absent primary or appending to "
        "additional_prs) BEFORE the drift scan below runs, so a PR naming "
        "several nodes closes all of them in this one invocation rather than "
        "only the one node stamped at creation. All-or-nothing: an "
        "unknown, malformed, or cross-repo claim binds nothing.",
    ),
    repo: Optional[str] = typer.Option(
        None,
        "--repo",
        help="owner/repo scoping --pr-number's gh query and cross-repo claim "
        "check. Resolved from the checkout's origin remote when omitted.",
    ),
) -> None:
    """Close open backlog nodes whose PR has merged outside the ship gate.

    The completion ritual (stamp plan -> mark node ``done`` -> capture
    follow-ups) runs automatically only through ``/target``'s ship gate or
    ``scripts/lib/pr-merge.sh``. A PR merged any other way (manual GitHub
    merge, bare ``gh pr merge``) leaves the node open. This verb detects that
    drift and closes it mechanically: mark done, best-effort stamp the plan,
    and drop a retro sentinel so a later session captures follow-ups. It never
    auto-creates inbox lines or backlog nodes, never auto-resumes work, and
    never clobbers a node that is already done.

    Side effect: also runs claim GC (``reap_dead_claims``), archiving dead
    lockfiles under the claims store's ``.expired/``. ``--dry-run`` propagates
    to it (no archiving). This fires on every throttled auto-reconcile,
    including the SessionStart hook - not just a manual invocation.
    """
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import _find_node
    from fno.graph._reconcile import (
        _effective_reconcile_cwd,
        emit_gate_escape_for_record,
        emit_human_touch_for_record,
        emit_session_satisfied_for_record,
        node_is_open,
        ReconcileError,
        repo_slug_from_url,
        resolve_promise_evidence,
        scan_merge_drift,
        write_retro_sentinel,
    )
    from fno.paths import retro_pending_dir

    # --node + --pr-number together is refused rather than silently
    # mis-scoped: --pr-number's own binding step (below) binds EVERY node the
    # PR's trailer claims, unconditional on --node, but the scan/close scope
    # would then collapse to the single --node id - leaving newly-bound
    # sibling claims stamped with a live PR ref but never closed until some
    # later, unrelated sweep happens to revisit them (round-6/7 review,
    # flagged twice with no caller ever exercising this combination). Loud
    # refusal beats a latent gap a future caller could silently trip.
    if node is not None and pr_number is not None:
        raise typer.BadParameter(
            "--node and --pr-number are mutually exclusive: --pr-number "
            "already scopes the scan to every node its own trailer claims, "
            "which --node cannot narrow without silently stranding the "
            "other claimed nodes stamped-but-unclosed. Run them separately."
        )

    # A truly unscoped, no-args sweep (SessionStart, a bare manual run) - the
    # only shape allowed to touch the whole graph: revert detection, the
    # stranded-epic self-heal, and an unbounded forward/reverse scan. A
    # `--pr-number` call names one specific PR and must stay bounded to it
    # (x-59a6 review fix - it used to fall through to a full sweep on every
    # merge, since it left `node` at None just like the truly-bare case).
    _full_sweep = node is None and pr_number is None

    def _pr_touch_ids(
        _entries: list[dict], _pr_number: int, _claims: list[str], _our_repo: Optional[str],
    ) -> set[str]:
        """Every node id ``_pr_number`` could possibly close: the trailer's
        own claims, any node already carrying ``_pr_number`` as a ref
        (stamped at creation, before this feature existed), plus every OPEN,
        ref-less node whose own ``cwd`` matches THIS repo's root.

        The last group exists because ``scan_merge_drift`` passes this exact
        scope straight through to ``reverse_map_unstamped`` (its reverse
        branch-name-map pass), which is what closes a node whose session
        died before the pr_number stamp landed. Omitting ref-less nodes here
        would silently zero out that entire pass on every ``--pr-number``
        call - the node's own branch names it, but a scope with nothing in
        it matches nothing. It is NOT free to include every such node
        graph-wide, though: the graph is a single store shared across every
        project on the machine, and ``reverse_map_unstamped`` fires one gh
        call per DISTINCT node cwd in its scope (bounded by
        ``REVERSE_MAP_BUDGET_S``) - an unscoped sweep pays a gh call for
        every other project's ref-less nodes too, on every ordinary merge in
        this repo. Scoped below by matching each candidate's own ``cwd``
        against ``repo_root()`` (this checkout's own root, cheap and local -
        no gh call) - never by comparing a resolved project-NAME string
        against the node's stored ``project`` field: that field is written
        by a completely different resolver (settings-based project
        detection at intake) that can drift from an independently-derived
        name, and a node created via ``fno backlog new`` before being
        claimed by a plan legitimately carries ``project: null``, which a
        name comparison would misread as "not this repo" even when its cwd
        matches exactly.

        The graph is CROSS-PROJECT: a bare number match, scoped by nothing,
        would pull in a same-numbered PR belonging to a different repo's
        node. Scoped by repo like every other PR-matching path in this
        module - a ref with no parseable url is still accepted as
        best-effort, but an unresolvable OUR OWN repo refuses every
        number-based match rather than wildcarding it in, matching
        ``_find_pr_node_id``'s actual stance (it returns ``None`` outright
        when its own slug is unresolvable, rather than treating that as a
        pass for every candidate).
        """
        from fno.graph._intake import repo_root
        from fno.graph._reconcile import node_cwd_in_repo, node_pr_refs, repo_slug_from_url

        _our_root = repo_root()

        ids = set(_claims)
        for _e in _entries:
            _rid = _e.get("id")
            if not (isinstance(_rid, str) and node_is_open(_e) and not node_pr_refs(_e)):
                continue
            if not node_cwd_in_repo(_e, _our_root):
                continue
            ids.add(_rid)
        if _our_repo is None:
            return ids
        for _e in _entries:
            _nid = _e.get("id")
            if not isinstance(_nid, str):
                continue
            for _num, _url in node_pr_refs(_e):
                if _num != _pr_number:
                    continue
                _ref_repo = repo_slug_from_url(_url)
                if _ref_repo is None or _ref_repo.lower() == _our_repo.lower():
                    ids.add(_nid)
                break
        return ids

    def _bind_and_report(
        _entries: list[dict], _pr_number: int, _pr_url: Optional[str],
        _claims: list[str], _repo: Optional[str],
    ) -> tuple[Optional[str], list[str]]:
        """Bind ``_claims`` to ``_pr_number`` against ``_entries``.

        Persists via ``locked_mutate_graph`` unless ``dry_run``, in which case
        it mutates ``_entries`` in place ONLY (``bind_closure_claims`` writes
        through node dict references, never a copy) so the caller's own
        forward-scan preview reflects the bind without ever touching disk -
        the same trailer-only node a real run would close, not just the one
        already stamped. Returns (refusal, bound_ids).
        """
        from fno.pr.closure import bind_closure_claims

        if dry_run:
            result = bind_closure_claims(
                _entries, _claims, pr_number=_pr_number, pr_url=_pr_url, repo=_repo,
            )
            return (result.refusal, list(result.bound_ids))

        _box: dict = {"refusal": None, "bound": []}

        def _mutator(entries2):
            result = bind_closure_claims(
                entries2, _claims, pr_number=_pr_number, pr_url=_pr_url, repo=_repo,
            )
            if result.outcome == "refused":
                _box["refusal"] = result.refusal
            else:
                _box["bound"] = list(result.bound_ids)
            return entries2

        locked_mutate_graph(_graph_path(), _mutator)
        return (_box["refusal"], _box["bound"])

    # --pr-number: bind every exact Backlog-Closure claim on this PR to its
    # node BEFORE the scan below, so the forward scan (which needs a PR ref to
    # query) can see a node that was named in the body but never individually
    # stamped at creation. Reuses the unchanged scan/close pipeline that
    # follows - this only makes the newly-bound nodes visible to it.
    closure_claims: list[str] = []
    closure_bound: list[str] = []
    closure_refused: Optional[str] = None
    supersession_files_by_pr: dict[int, list[str]] = {}
    entries = read_graph(_graph_path())
    if pr_number is not None:
        from fno.pr.closure import ClosureQueryError, fetch_pr_closure_context, parse_closure_trailer

        try:
            pr_ctx = fetch_pr_closure_context(pr_number, repo=repo)
        except ClosureQueryError as exc:
            closure_refused = f"could not query PR #{pr_number}: {exc}"
            pr_ctx = None
            if not json_out:
                typer.echo(f"warning: reconcile --pr-number: {closure_refused}", err=True)

        # A plain `if pr_ctx is not None:` (not try/except/else) so mypy can
        # narrow pr_ctx to non-None for the rest of this block - it cannot
        # narrow across a reassignment to None in a sibling `except`.
        if pr_ctx is not None and pr_ctx.state != "MERGED":
            # A binding is a MERGE-time close signal, never a promise on
            # an OPEN or CLOSED-unmerged PR - the caller could later
            # abandon or close it unmerged, leaving a claimed node
            # holding a dead ref. The three real callers (the ritual,
            # fno do pr merge, the bare sweep) only ever reach this with an
            # already-merged PR; this guards a direct manual
            # `--pr-number` invocation against the same premature bind.
            #
            # Only worth refusing (and reporting) when the body actually
            # names a trailer: a state-read blip on a PR with NO trailer
            # at all has nothing to bind either way, and the node this
            # run is closing almost always closes fine via the ordinary
            # ref-based scan below - flagging that as a refusal read a
            # fully successful run as failed (round-8 review fix).
            if parse_closure_trailer(pr_ctx.body):
                closure_refused = (
                    f"PR #{pr_number} is not merged (state={pr_ctx.state})"
                )
                if not json_out:
                    typer.echo(
                        f"warning: reconcile --pr-number: {closure_refused}", err=True,
                    )
            pr_ctx = None

        if pr_ctx is not None:
            supersession_files_by_pr[pr_number] = list(pr_ctx.changed_files)
            closure_claims = parse_closure_trailer(pr_ctx.body)
            if closure_claims:
                closure_refused, closure_bound = _bind_and_report(
                    entries, pr_number, pr_ctx.url, closure_claims, repo,
                )
                if not dry_run:
                    entries = read_graph(_graph_path())  # real bind just persisted
                if closure_refused and not json_out:
                    typer.echo(
                        f"warning: reconcile --pr-number {pr_number}: refused "
                        f"binding: {closure_refused}",
                        err=True,
                    )
                elif closure_bound and not json_out:
                    typer.echo(
                        f"reconcile --pr-number {pr_number}: bound "
                        f"{', '.join(closure_bound)}",
                        err=True,
                    )

    # Open-PR binding heal (x-d3c6): an open PR whose branch names an open,
    # ref-less node leaves that node invisible to every graph-first reader
    # until something fills pr_number. One bounded open-PR listing per repo
    # BEFORE the scan, so the healed rows feed the forward scan and the status
    # derivation below. Full and explicit-node runs only: a --pr-number call
    # is bounded to its own PR and must not sweep unrelated repos.
    open_bound: list[dict] = []
    open_binding_advisories: list[str] = []
    if _full_sweep or node is not None:
        from fno.graph._reconcile import (
            bind_pr_rows,
            collect_open_binding_heals,
            node_pr_refs,
        )

        _open_heals, open_binding_advisories = collect_open_binding_heals(
            entries, node_id=node
        )
        if _open_heals:

            def _open_fill(_entries: list[dict]) -> list[dict]:
                _kept: list[dict] = []
                heal_counts: dict[str, int] = {}
                for candidate in _open_heals:
                    if isinstance(candidate.node_id, str):
                        heal_counts[candidate.node_id] = (
                            heal_counts.get(candidate.node_id, 0) + 1
                        )
                for h in _open_heals:
                    if not isinstance(h.node_id, str) or heal_counts.get(h.node_id) != 1:
                        continue
                    current = next(
                        (entry for entry in _entries if entry.get("id") == h.node_id),
                        None,
                    )
                    if current is None or not node_is_open(current) or node_pr_refs(current):
                        continue
                    result = bind_pr_rows(
                        _entries, [h.node_id],
                        pr_number=h.pr_number, pr_url=h.pr_url,
                    )
                    if result.outcome == "bound" and result.bound_ids:
                        _kept.append(
                            {"node": h.node_id, "pr": h.pr_number, "url": h.pr_url}
                        )
                return _kept

            if dry_run:
                # In-memory only (same contract as _bind_and_report's dry-run):
                # the forward-scan preview below sees the healed rows, disk
                # never moves.
                open_bound = [
                    dict(fill, would=True) for fill in _open_fill(entries)
                ]
            else:
                _box: list[dict] = []

                def _open_mutator(entries2):
                    _box.extend(_open_fill(entries2))
                    return entries2

                locked_mutate_graph(_graph_path(), _open_mutator)
                open_bound = _box
                # Real fills just persisted: reread so the scan below consumes
                # the healed rows rather than the pre-heal snapshot.
                entries = read_graph(_graph_path())
        if not json_out:
            for b in open_bound:
                typer.echo(
                    f"open_pr_bound: {b['node']} -> PR #{b['pr']} ({b['url']})",
                    err=True,
                )
            for advisory in open_binding_advisories:
                typer.echo(f"warning: {advisory}", err=True)

    # A --pr-number call scans only what THIS PR could touch - its own
    # stamped ref plus every exact trailer claim - never the whole graph
    # (x-59a6 review fix: this used to fall through to an unscoped scan on
    # every merge, since `node` stays None for a --pr-number-only call, the
    # same shape as a truly bare sweep). An explicit --node still wins.
    _scan_scope: Optional[Union[str, set[str]]]
    if node is not None:
        _scan_scope = node
    elif pr_number is not None:
        from fno.graph._reconcile import repo_slug_from_url as _repo_slug_from_url

        _our_repo = repo or (
            _repo_slug_from_url(pr_ctx.url) if pr_ctx is not None else None
        )
        if _our_repo is None and not json_out:
            # _pr_touch_ids refuses to repo-scope a bare-number ref match
            # when it cannot resolve OUR OWN repo (avoiding a false cross-repo
            # collision), so the scan below can silently see only trailer
            # claims and open ref-less nodes - the ordinary ref-stamped,
            # no-trailer close can go dark with nothing on stderr to say so.
            # Same condition leg_stamp already warns about before it calls
            # this same command with an explicit --repo; surface it here too
            # for a caller (a bare `fno backlog reconcile --pr-number`) that
            # never resolves one itself (round-11 review fix, x-59a6).
            typer.echo(
                f"warning: reconcile --pr-number {pr_number}: could not resolve "
                "this repo's slug; scoping to exact trailer claims and open "
                "ref-less nodes only, skipping any ref-stamped bare-number match",
                err=True,
            )
        _scan_scope = _pr_touch_ids(entries, pr_number, closure_claims, _our_repo)
    else:
        _scan_scope = None
    records = scan_merge_drift(entries, node_id=_scan_scope)

    # Auto-bind closure claims for every OTHER merged PR this sweep just
    # discovered on its own (x-59a6). --pr-number above covers the two paths
    # that already KNOW the PR number (the post-merge ritual, `fno do pr
    # merge`); a THIRD path merges with no caller ever naming a number at
    # all - an operator merging in the GitHub UI, or a king's automation -
    # and is caught only later by this bare sweep's own forward/reverse scan.
    # Full sweep only: a --pr-number call is scoped to its own PR above, and
    # discovering totally unrelated merged PRs is this bare sweep's job (it
    # auto-fires often), not a side effect of every single merge event.
    if _full_sweep:
        from fno.pr.closure import ClosureQueryError, fetch_pr_closure_context, parse_closure_trailer
        from fno.graph._reconcile import repo_slug_from_url, resolve_current_repo_slug

        # One record per discovered PR (not just the number): each record
        # already carries the pr_url/cwd this scan resolved it through, and
        # the graph is CROSS-PROJECT, so a single global `--repo` would
        # mis-scope a record belonging to a different project's repo.
        discovered: dict = {}
        for r in records:
            if r.closeable and r.pr_number != pr_number and r.pr_number not in discovered:
                discovered[r.pr_number] = r
        # Bounded two ways, not because more discovery is wrong, but because
        # each entry costs one sequential `gh pr view` (up to
        # GH_QUERY_TIMEOUT_S each) - an unbounded loop on a backlog with many
        # stale closeable records turns every SessionStart sweep into a
        # serial gh-latency tax. A COUNT cap alone still lets a degraded gh
        # (network blip, auth stall) burn up to count*GH_QUERY_TIMEOUT_S -
        # 20*30s = 10 minutes - turning a hook that used to fail fast into
        # one that hangs. A wall-clock budget bounds that regardless of the
        # count cap. A later sweep (SessionStart auto-fires often) picks up
        # whatever this run dropped either way.
        import time as _time

        _MAX_AUTO_DISCOVER = 20
        _AUTO_DISCOVER_BUDGET_S = 60.0
        _deadline = _time.monotonic() + _AUTO_DISCOVER_BUDGET_S
        _dropped = list(discovered.items())[_MAX_AUTO_DISCOVER:]
        if _dropped and not json_out:
            typer.echo(
                f"reconcile: auto-discovery capped at {_MAX_AUTO_DISCOVER}; "
                f"deferred {len(_dropped)} PR(s) to a later sweep: "
                f"{', '.join(str(n) for n, _ in _dropped)}",
                err=True,
            )
        auto_bound_any = False
        for _pr_num, _rec in list(discovered.items())[:_MAX_AUTO_DISCOVER]:
            if _time.monotonic() >= _deadline:
                if not json_out:
                    typer.echo(
                        f"reconcile: auto-discovery stopped at its "
                        f"{_AUTO_DISCOVER_BUDGET_S:.0f}s budget (gh is slow or "
                        "degraded); the rest defers to a later sweep",
                        err=True,
                    )
                break
            # The record's OWN url or cwd resolves the repo to scope this query
            # and bind to - the graph is CROSS-PROJECT, so a --repo passed for
            # the --pr-number leg above belongs to a DIFFERENT project and must
            # never scope an unrelated discovered PR. Neither resolving: skip
            # rather than guess, since a same-numbered PR can exist in the wrong
            # repo and a fresh (no-existing-ref) node would bind to it blind.
            _repo_for_pr = (
                repo_slug_from_url(_rec.pr_url)
                or (resolve_current_repo_slug(_rec.cwd) if _rec.cwd else None)
            )
            if _repo_for_pr is None:
                continue
            try:
                _ctx = fetch_pr_closure_context(_pr_num, repo=_repo_for_pr, cwd=_rec.cwd)
            except ClosureQueryError:
                continue  # best-effort discovery: the forward scan already has this PR's state
            _claims = parse_closure_trailer(_ctx.body)
            supersession_files_by_pr[_pr_num] = list(_ctx.changed_files)
            if not _claims:
                continue
            closure_claims = sorted(set(closure_claims) | set(_claims))
            _refusal, _bound = _bind_and_report(entries, _pr_num, _ctx.url, _claims, _repo_for_pr)
            if _bound:
                closure_bound.extend(_bound)
                auto_bound_any = True
            elif _refusal and not json_out:
                typer.echo(
                    f"warning: reconcile: auto-discovered PR #{_pr_num} refused "
                    f"binding: {_refusal}",
                    err=True,
                )
        if auto_bound_any:
            if not dry_run:
                entries = read_graph(_graph_path())  # real binds just persisted
            records = scan_merge_drift(entries, node_id=node)

    closeable = [r for r in records if r.closeable]
    failures = [r for r in records if r.error is not None]

    # Promise gate (x-5d34): a merged PR is not enough when the plan promised
    # more. Partition the closeable set so an unmet-promise node stays open
    # rather than closing silently on the unattended sweep. Reconcile is the
    # MAINSTREAM close (auto-fires on SessionStart), so without this leg a node
    # that cmd_done refused would close here on the next session anyway.
    promise_unmet: list[tuple[str, str]] = []
    promise_warnings: list[dict[str, str]] = []
    if closeable:
        gated: list = []
        for record in closeable:
            # Distinct name from the rollup loop's `node_obj`: that loop assigns
            # `_find_node(...)` (dict | None), so reusing the name here would pin
            # it to `dict` via the `or {...}` and trip mypy at the later assign.
            gate_node = _find_node(entries, record.node_id) or {
                "id": record.node_id,
                "plan_path": record.plan_path,
                "pr_number": record.pr_number,
                "pr_url": record.pr_url,
            }
            # _effective_reconcile_cwd, not the raw node cwd: an archived
            # worktree is a dead dir, and handing it to subprocess(cwd=) makes
            # the probe runner fail to launch - a fail-CLOSED refusal that would
            # hold the node open on every sweep forever.
            gate_cwd = _effective_reconcile_cwd(
                gate_node.get("cwd") or "", gate_node.get("project")
            )
            verdict = resolve_promise_evidence(
                gate_node, cwd=gate_cwd if os.path.isdir(gate_cwd) else None
            )
            if verdict.warning and not json_out:
                # Named, not silent: reconcile is the unattended close path, so a
                # gate that skipped itself must still leave a trace.
                typer.echo(f"warning: {verdict.warning}", err=True)
            if verdict.warning:
                promise_warnings.append(
                    {"node_id": record.node_id, "warning": verdict.warning}
                )
            if verdict.outcome == "promise_unmet":
                # First refusal line only: the full reason belongs to the verb
                # the operator runs to resolve it, not this one-line sweep roll.
                first_line = (verdict.reason or "").splitlines()[0]
                promise_unmet.append((record.node_id, first_line))
            else:
                gated.append(record)
        closeable = gated

    # Pre-existing stranded all-done epics to self-heal this sweep (codex P2 on
    # PR #69). Read-only check so a reconcile with no drift AND nothing to heal
    # still skips the lock entirely. ONLY on a full reconcile: a node-scoped
    # `reconcile --node <id>` must not close/dispatch unrelated epics (codex P2),
    # so the global sweep is suppressed there (the targeted node's own cascade
    # still fires).
    strandable = _strandable_epic_ids(entries) if _full_sweep else set()
    # Same self-heal role for contained nodes whose unit already shipped
    # (x-e957): neither the merge-time cascade nor scan_merge_drift can reach
    # them, so without this leg a back-filled `contained_in` on an
    # already-merged owner strands the node permanently. Full sweep only, for
    # the same reason as the epic sweep above.
    strandable_contained = _strandable_contained_ids(entries) if _full_sweep else set()
    # A pending supersession whose successor closed outside this sweep is owed a
    # verdict nothing else will ever deliver. Gather its evidence BEFORE the
    # lock: these are `gh` round trips, and the graph lock is not the place for
    # them. Full sweep only, matching the other self-heal legs.
    owed_evidence: dict[str, dict] = {}
    owed_evidence_failures: list[dict[str, str]] = []
    if _full_sweep and not dry_run:
        from fno.graph._reconcile import (
            _DEFAULT_QUERY_PR_MERGE_STATE,
            query_pr_merge_state,
            successors_owing_verification,
        )

        for successor_id, successor in successors_owing_verification(entries).items():
            successor_repo = repo_slug_from_url(successor.get("pr_url"))
            successor_cwd = successor.get("cwd") if successor_repo is None else None
            try:
                if query_pr_merge_state is _DEFAULT_QUERY_PR_MERGE_STATE:
                    merged = query_pr_merge_state(
                        successor["pr_number"],
                        repo=successor_repo,
                        cwd=successor_cwd,
                        include_files=True,
                    )
                else:
                    merged = query_pr_merge_state(
                        successor["pr_number"],
                        repo=successor_repo,
                        cwd=successor_cwd,
                    )
            except ReconcileError as exc:
                owed_evidence_failures.append(
                    {
                        "successor": successor_id,
                        "pr_number": str(successor["pr_number"]),
                        "error": str(exc),
                        "kind": exc.kind,
                        "remedy": exc.remedy_for(
                            pr_number=successor["pr_number"], repo=successor_repo
                        ),
                    }
                )
                continue
            if merged.state != "MERGED":
                continue
            owed_evidence[successor_id] = {
                "changed_files": list(merged.changed_files),
                "files_truncated": merged.files_truncated,
                "pr_number": successor["pr_number"],
                "merged_at": merged.merged_at,
            }

    closed: list[dict] = []
    healed_epics: list[str] = []
    contained_closed: list[str] = []
    contained_errors: list[dict] = []
    supersession_unverified: list[dict] = []
    # Set below only on the dry-run simulate branch; epics_waiting reuses it
    # (x-59a6 review fix) so its "still open" read agrees with the same
    # preview `candidates`/`healed_epics` already report as would-close.
    _sim: Optional[list[dict]] = None

    if not dry_run and (closeable or strandable or strandable_contained or owed_evidence):
        # Apply every close in ONE locked mutation rather than locking once
        # per node: locked_mutate_graph acquires a file lock and rewrites the
        # whole graph, so a per-node loop is O(N) lock+rewrite cycles. The
        # mutator collects the records it actually closed (idempotency: a node
        # closed or removed out-of-band between the read-only scan and the lock
        # is skipped) so the post-lock work only touches genuinely-closed nodes.
        actually_closed: list = []
        # Ancestor epics the cascade closed this sweep (x-33b2). Their OWN
        # dependents (a node blocked_by the epic) must be auto-continued too, so
        # we accumulate the ids here and run the same dispatch path for them
        # after the lock - else an epic-level dependent stalls.
        cascade_closed_acc: list = []
        # Nodes closed because they shipped inside a closed node's PR (x-e957).
        # Kept SEPARATE from cascade_closed_acc, which drives auto-continue
        # dispatch - contained nodes deliberately get none. This list is for
        # reporting only: a sweep that closes three nodes while saying it closed
        # one reads as "already in sync" to the next operator.
        contained_closed_acc: list = []
        # Cascade/sweep failures, carried into the --json payload. stderr alone
        # is invisible to the SessionStart hook, which runs `reconcile --json`
        # and discards stderr - so a repeatedly-failing cascade left contained
        # nodes open forever with no signal reaching any automated reader.
        contained_errors_acc: list = []
        supersession_unverified_acc: list[dict] = []

        # Ledger rollup, precomputed outside the lock (ledger I/O must not block
        # other graph mutations). Reconcile is the MAINSTREAM close: a session
        # lands its PR open, `done` exits 5 awaiting merge, and reconcile closes
        # it at the merge - so without the rollup here, session_id / cost /
        # points are never recorded on the normal path at all.
        reconcile_rollups: dict = {}
        try:
            from fno.done.cli import _rollup_from_ledger

            for record in closeable:
                node_obj = _find_node(entries, record.node_id)
                if node_obj:
                    reconcile_rollups[record.node_id] = _rollup_from_ledger(node_obj)
        except Exception:
            reconcile_rollups = {}

        from fno.graph._reconcile import verify_pending_supersessions

        def mutator(entries):
            actually_closed.clear()
            cascade_closed_acc.clear()
            supersession_unverified_acc.clear()
            for record in closeable:
                node_obj = _find_node(entries, record.node_id)
                if node_obj and not node_obj.get("completed_at"):
                    _apply_completion_fields(node_obj, merge_status="merged")
                    supersession_unverified_acc.extend(
                        verify_pending_supersessions(
                            entries,
                            successor=record.node_id,
                            changed_files=(
                                record.changed_files
                                or supersession_files_by_pr.get(record.pr_number, [])
                            ),
                            evidence_pr=record.pr_number,
                            verified_at=record.merged_at,
                            evidence_complete=not record.files_truncated,
                        )
                    )
                    if record.node_id in reconcile_rollups:
                        try:
                            from fno.done.cli import _apply_rollup

                            # Cost is fill-only, matching cmd_done. _apply_rollup
                            # merges cost_sessions and recomputes the total, but
                            # a prior stamp (fno backlog cost / a loop writer)
                            # timestamps its rows at recording time while the
                            # ledger row carries the completion time, so the same
                            # run reads as distinct and gets double-counted.
                            # Preserve any existing cost; the rollup's job here is
                            # session_id / points.
                            prior_cost = node_obj.get("cost_usd")
                            prior_sessions = list(node_obj.get("cost_sessions") or [])
                            # No env_session: reconcile is the detached
                            # SessionStart sweep, so CLAUDECODE_SESSION_ID names
                            # whoever started it, NOT the closed node's work.
                            # Trust the ledger's attribution.
                            _apply_rollup(
                                node_obj,
                                reconcile_rollups[record.node_id],
                                env_session=None,
                            )
                            if prior_cost is not None:
                                node_obj["cost_usd"] = prior_cost
                                node_obj["cost_sessions"] = prior_sessions
                        except Exception:
                            pass
                    # Backfill the PR ref for a reverse-mapped node (dead before
                    # the node<->PR stamp): the recovered number/url live only on
                    # the record, so without this the closed node stays
                    # pr_number: null - the board loses the shipped-PR link and
                    # detect_reverted_nodes() (which reads node_pr_refs) can never
                    # match a later revert. Only fill when absent so the forward
                    # path (node already stamped) is untouched.
                    if record.pr_number and not node_obj.get("pr_number"):
                        node_obj["pr_number"] = record.pr_number
                        node_obj["pr_url"] = record.pr_url
                    # Close every node that shipped inside this PR (x-e957),
                    # BEFORE the parent cascade: a contained node is a child of
                    # the delivery unit, so the epic above is only all-done once
                    # these are closed too. Running it after would leave the
                    # ancestor open for a sweep it should have closed now.
                    #
                    # A loud warning, never an abort. The merge already
                    # happened and the delivery unit's own close is the
                    # load-bearing write; a cascade that raised here would leave
                    # that unit open against a merged PR - strictly worse than
                    # the bug this fixes. Contained ids are NOT added to
                    # cascade_closed_acc: that accumulator drives auto-continue
                    # dispatch, which contained nodes deliberately do not get.
                    try:
                        contained_closed_acc.extend(
                            _cascade_close_contained(entries, record.node_id)
                        )
                    except Exception as _cc_exc:  # noqa: BLE001 - never abort a close
                        contained_errors_acc.append({
                            "owner": record.node_id,
                            "stage": "merge-cascade",
                            "error": str(_cc_exc)[:200],
                        })
                        typer.echo(
                            f"warning: closed {record.node_id} but the contained-node "
                            f"cascade failed: {_cc_exc}; any node with "
                            f"contained_in={record.node_id} is still open "
                            "(`fno backlog reconcile` retries on the next run)",
                            err=True,
                        )
                    # Cascade-close now-all-done ancestor epics (x-33b2), uniform
                    # across projects (follows the parent edge, not a filter).
                    cascade_closed_acc.extend(
                        _cascade_close_parents(entries, record.node_id)
                    )
                    actually_closed.append(record)
            # Self-heal pre-existing stranded all-done epics (codex P2): close any
            # open epic whose children are already all done, even with no drift
            # this sweep. Full reconcile only - a node-scoped run must not touch
            # unrelated epics. Going forward the cascade prevents new ones, so
            # this is a no-op once migrated. Their dependents auto-continue via
            # the same cascade_closed_acc dispatch loop.
            # Self-heal contained nodes whose unit shipped before the
            # containment record existed (codex P1): the merge-time cascade
            # above only fires while closing an owner, and an already-closed
            # owner never appears in `closeable`. Runs BEFORE the epic sweep so
            # an epic waiting on one of these sees it done in the same pass.
            # Full reconcile only, matching _sweep_close_done_epics.
            if _full_sweep:
                # Guarded for the same reason the merge-time cascade is: this
                # leg is a self-heal for a state that predates the invariant,
                # and letting it raise would abort the whole sweep - taking
                # every genuine PR-drift close with it. Strictly worse than the
                # stale rows it exists to clean up.
                try:
                    contained_closed_acc.extend(
                        _sweep_close_stranded_contained(entries)
                    )
                except Exception as _sw_exc:  # noqa: BLE001 - never abort the sweep
                    contained_errors_acc.append({
                        "owner": None,
                        "stage": "stranded-heal",
                        "error": str(_sw_exc)[:200],
                    })
                    typer.echo(
                        "warning: the stranded-contained self-heal failed: "
                        f"{_sw_exc}; nodes whose delivery unit already merged "
                        "stay open (`fno backlog reconcile` retries next run)",
                        err=True,
                    )
                cascade_closed_acc.extend(_sweep_close_done_epics(entries))
                # Same self-heal shape, and guarded the same way: a raise here
                # would abort a sweep whose real job is closing merged PRs.
                try:
                    for successor_id, evidence in owed_evidence.items():
                        supersession_unverified_acc.extend(
                            verify_pending_supersessions(
                                entries,
                                successor=successor_id,
                                changed_files=evidence["changed_files"],
                                evidence_pr=evidence["pr_number"],
                                verified_at=evidence["merged_at"],
                                evidence_complete=not evidence["files_truncated"],
                            )
                        )
                except Exception as _vs_exc:  # noqa: BLE001 - never abort the sweep
                    typer.echo(
                        "warning: the pending-supersession self-heal failed: "
                        f"{_vs_exc}; predecessors whose successor already shipped "
                        "stay blocked (`fno backlog reconcile` retries next run)",
                        err=True,
                    )
            return entries

        # Capture the POST-lock graph (codex P2): resolving a closed node's
        # project/cwd from the pre-lock `entries` snapshot risks stale routing if
        # a concurrent reparent/reproject landed between scan and lock, and a
        # cascade parent only reachable in the locked graph would resolve to None.
        post_entries = locked_mutate_graph(_graph_path(), mutator)
        supersession_unverified.extend(supersession_unverified_acc)

        def _auto_continue_after_close(node_id, project, root):
            """Merge-triggered auto-continue dispatch for one just-closed node:
            same-project `next`, cross-project dependents, and contract de-stub.
            Shared by directly-closed records AND cascade-closed ancestor epics
            (x-33b2) so an epic-level dependent is dispatched, not stranded."""
            from fno.backlog.advance import advance as _advance
            from fno.backlog.advance import advance_dependents as _advance_deps
            from fno.backlog.reconcile_dispatch import dispatch_reconcile_for_blocker
            _advance(closed_node_id=node_id, project=project, project_root=root)
            _advance_deps(closed_node_id=node_id, closed_project=project, project_root=root)
            dispatch_reconcile_for_blocker(closed_node_id=node_id, project_root=root)

        # Post-mutation work outside the lock (mirrors `done`): stamp the plan
        # and drop the retro sentinel for each node we actually closed.
        for record in actually_closed:
            # Reflect the real stamp outcome: the helper is best-effort and may
            # no-op (missing script) or fail, so don't claim it stamped. Pass the
            # merged PR url so a plan that never went through the ship gate is
            # stamped shipped->done rather than getting a graduate no-op
            # (ab-bd9f476c).
            stamped = bool(record.plan_path) and _stamp_and_graduate_plan(
                record.plan_path,
                url=record.pr_url,
                session_id=record.session_id,
            )

            # Sentinel write is best-effort: the node is already closed in the
            # graph, so a failed sentinel must not abort the loop and strand
            # later records. Warn and continue, mirroring _graduate_plan.
            sentinel_str = None
            try:
                sentinel_str = str(
                    write_retro_sentinel(record, sentinel_dir=retro_pending_dir())
                )
            except OSError as exc:
                typer.echo(
                    f"warning: closed {record.node_id} but failed to write its "
                    f"retro sentinel: {exc}",
                    err=True,
                )

            # Hand the auto-complete signal to the owning target session (Group 1
            # / ab-f7f8bc53): an out-of-band merge bypassed pr-merge.sh's emit, so
            # the owning session is still IN_PROGRESS and its stop hook would hard
            # re-block. Best-effort + non-fatal: a failure here logs and continues
            # (the defensive stop-hook probe is the backstop).
            emit_session_satisfied_for_record(record)

            # W4 touch telemetry: an out-of-band merge is a human steering
            # action no loop performed. Once per node - reconcile only ever
            # closes a node once (AC4-EDGE).
            emit_human_touch_for_record(record)

            # Ledger backstop (x-88df US3): the merge event is the one moment
            # the system is guaranteed to know (node, pr, project, merged_at),
            # yet the ledger's only writer is the origin's own finalize - a
            # killed/reaped origin leaks its row. Stamp a null-pr row or create a
            # minimal backstop for the transcript-gone tail; the direct-finalize
            # rung's full row supersedes it via the collapse rule. Best-effort
            # and non-fatal: a ledger failure must never abort the close (AC1-ERR).
            try:
                from fno.cost._register import upsert_ledger_pr

                _led_node = _find_node(post_entries, record.node_id)
                _led_project = (_led_node or {}).get("project")
                upsert_ledger_pr(
                    record.node_id,
                    record.pr_number,
                    record.pr_url,
                    _led_project,
                    record.merged_at,
                )
            except Exception as _led_exc:  # noqa: BLE001 - never abort the close
                typer.echo(
                    f"warning: ledger upsert for {record.node_id} (PR "
                    f"#{record.pr_number}) failed: {_led_exc}; node close unaffected",
                    err=True,
                )

            # Tier-1 gate_escape (x-f894): a STRICTER subset of the touch above -
            # only when a required review bot never reviewed the oob-merged PR
            # (the #222 boundary). Resolve the repo's required bots (empty on
            # most repos -> no-op) and let the helper apply the boundary + emit.
            # Fully fail-open: never abort the close.
            _required_bots: list = []
            if record.cwd:
                try:
                    from fno.config import load_settings_for_repo
                    _settings = load_settings_for_repo(Path(record.cwd))
                    # The review block lives under `config:` (SettingsModel ->
                    # config.review), NOT at the top level - reading
                    # `_settings.review` always missed and returned [], so the
                    # emit short-circuited and this telemetry never fired in the
                    # real CLI path (codex P2 on PR #232). github_apps is the bot
                    # half of the required gate; a local peer reviewer that never
                    # reviewed is NOT counted here. That under-reports (fail-safe
                    # direction) and is acceptable for a Tier-1 metric - dead-bot
                    # is the recurring escape this catches.
                    _required_bots = list(
                        _settings.review.github_apps or []
                    )
                except Exception:
                    _required_bots = []  # fail open: unresolvable config -> no emit
            emit_gate_escape_for_record(record, required_bots=_required_bots)

            closed.append({
                "node_id": record.node_id,
                "pr_number": record.pr_number,
                "pr_url": record.pr_url,
                "plan_stamped": stamped,
                "sentinel": sentinel_str,
            })

            # Merge-triggered auto-continue (ab-3cd195b6 / task 2.1): now that
            # this node's close has committed (AC1-RACE ordering: advance runs
            # only AFTER the locked_mutate_graph above), dispatch a fresh
            # /target --no-merge worker for the next now-unblocked node IF
            # auto-continue is armed for the project. advance gates on
            # enablement internally (a no-op advance_skipped{disabled} when
            # off) and is strictly non-fatal: a failed advance never fails the
            # reconcile sweep. Project-scoped per the closed node's project.
            try:
                _adv_node = _find_node(post_entries, record.node_id)
                _adv_project = _adv_node.get("project") if _adv_node else None
                # Resolve auto-continue state against the CLOSED NODE's project
                # context, not the reconcile's cwd (codex P2): a full-graph
                # reconcile run from project A can close a node belonging to
                # project B, and B's campaign-arm marker lives under B's root.
                _adv_cwd = _adv_node.get("cwd") if _adv_node else None
                # If the closed node's recorded cwd is an archived worktree,
                # route from its project root instead (x-3dd0): otherwise advance
                # probes the campaign-arm marker under a missing dir and strands
                # the next node. Re-resolved from the POST-lock node so a
                # concurrent reproject is still honored.
                if _adv_cwd:
                    _adv_cwd = _effective_reconcile_cwd(_adv_cwd, _adv_project)
                _adv_root = Path(_adv_cwd) if _adv_cwd else None
                # next (same-project) + cross-project dependents (G1) + contract
                # de-stub (G4), in one shared helper.
                _auto_continue_after_close(record.node_id, _adv_project, _adv_root)
            except Exception as _adv_exc:  # noqa: BLE001 - never abort the sweep
                typer.echo(
                    f"warning: auto-continue advance after closing "
                    f"{record.node_id} failed: {_adv_exc}",
                    err=True,
                )

        # Project every closed node (records + cascade-closed epic parents) onto
        # its plan (forward-only, stamps done_at). The per-record stamp above
        # only touches directly-closed records; the epic parents need this.
        _project_plans_from_graph(
            [r.node_id for r in actually_closed] + list(cascade_closed_acc)
        )

        # x-33b2: a cascade-closed parent epic unblocks its OWN dependents (a node
        # blocked_by the epic). The per-record loop above only dispatched the
        # directly-closed children, so run the same auto-continue for each
        # cascade-closed ancestor too - else an epic-level dependent stalls.
        # Deduped; project/cwd read from the (close-stable) graph.
        _seen_parents: set = set()
        for _pid in cascade_closed_acc:
            if _pid in _seen_parents:
                continue
            _seen_parents.add(_pid)
            try:
                _pn = _find_node(post_entries, _pid)
                _pproj = _pn.get("project") if _pn else None
                _proot = Path(_pn["cwd"]) if _pn and _pn.get("cwd") else None
                _auto_continue_after_close(_pid, _pproj, _proot)
            except Exception as _exc:  # noqa: BLE001 - never abort the sweep
                typer.echo(
                    f"warning: auto-continue after cascade-closing {_pid} failed: {_exc}",
                    err=True,
                )
        # Epics the cascade/sweep auto-closed this run, for user-visible accounting
        # (codex P3): the close summaries below otherwise only describe PR-drift
        # records and would report "in sync" even after healing epics.
        healed_epics = sorted(_seen_parents)
        contained_closed = sorted(set(contained_closed_acc))
        contained_errors = list(contained_errors_acc)
    elif dry_run and (closeable or strandable or strandable_contained):
        # Accurate --dry-run preview (codex P2): the heal set is NOT just the
        # pre-close `strandable` epics - closing a closeable last child cascade-
        # closes its parent, and the sweep fixpoint reaches ancestors. Simulate
        # the exact close + cascade + sweep on a THROWAWAY deep copy so the
        # preview matches a real run, mutating nothing real.
        import copy as _copy

        _sim = _copy.deepcopy(entries)
        _sim_acc: list = []
        _sim_contained: list = []
        for record in closeable:
            _sn = _find_node(_sim, record.node_id)
            if _sn and not _sn.get("completed_at"):
                _apply_completion_fields(_sn)
                # Same order as the real mutator: contained children close
                # first, so the simulated parent cascade sees the same
                # all-children-done world a real run would.
                # Guarded like the real mutator. Unguarded, a raise crashed the
                # PREVIEW with a traceback where a real run degrades to a
                # warning - the preview failing harder than the thing it
                # previews - and `contained_errors` stayed [] in the --json
                # payload, asserting no errors for a leg that never completed.
                try:
                    _sim_contained.extend(
                        _cascade_close_contained(_sim, record.node_id)
                    )
                except Exception as _sc_exc:  # noqa: BLE001 - preview never crashes
                    typer.echo(
                        f"warning: dry-run contained cascade for "
                        f"{record.node_id} failed: {_sc_exc}; the preview "
                        "under-reports contained closes",
                        err=True,
                    )
                    contained_errors.append({
                        "owner": record.node_id,
                        "stage": "merge-cascade (dry-run)",
                        "error": str(_sc_exc)[:200],
                    })
                _sim_acc.extend(_cascade_close_parents(_sim, record.node_id))
        if _full_sweep:
            try:
                _sim_contained.extend(_sweep_close_stranded_contained(_sim))
            except Exception as _ss_exc:  # noqa: BLE001 - preview never crashes
                typer.echo(
                    f"warning: dry-run stranded-contained heal failed: "
                    f"{_ss_exc}; the preview under-reports contained closes",
                    err=True,
                )
                contained_errors.append({
                    "owner": None,
                    "stage": "stranded-heal (dry-run)",
                    "error": str(_ss_exc)[:200],
                })
            _sim_acc.extend(_sweep_close_done_epics(_sim))
        healed_epics = sorted(set(_sim_acc))
        contained_closed = sorted(set(_sim_contained))

    # W4 causal links: best-effort revert stamp, full sweep only. A merged
    # "Revert ..." PR referencing a PR carried by a graph node flips that
    # node's `reverted` flag so survival math stops counting it. Strictly
    # non-fatal; misses fall back to `fno backlog update --reverted`.
    reverted_stamped: list[dict] = []
    if _full_sweep:
        try:
            from fno.graph._reconcile import (
                ReconcileError,
                detect_reverted_nodes,
                fetch_recent_merged_prs,
            )

            try:
                merged_prs = fetch_recent_merged_prs()
            except ReconcileError:
                # gh unauthed/offline: reconcile auto-fires on SessionStart,
                # so a degraded gh must stay quiet (manual --reverted remains).
                merged_prs = []
            pairs = detect_reverted_nodes(merged_prs, entries)
            if pairs and not dry_run:
                def _stamp_reverts(entries2):
                    for nid, _rpr in pairs:
                        n = _find_node(entries2, nid)
                        if n is not None and not n.get("reverted"):
                            n["reverted"] = True
                    return entries2

                locked_mutate_graph(_graph_path(), _stamp_reverts)
            reverted_stamped = [
                {"node_id": nid, "revert_pr": rpr} for nid, rpr in pairs
            ]
        except Exception as exc:  # noqa: BLE001 - never abort the sweep
            typer.echo(f"warning: revert detection skipped: {exc}", err=True)

    # Canonical-sync catch-up. reconcile auto-fires on SessionStart, so
    # this is the leg that breaks the circularity: when the pr-watch daemon is
    # dead AGAIN, the next interactive session catches the canonical up instead
    # of the outage waiting for a human to notice. Same self-heal posture as
    # hooks/groom-self-heal-session-start.sh. Skipped under --dry-run (a preview
    # must mutate nothing) and strictly non-fatal to the sweep.
    sync_catchup: dict = {"outcome": "not-run"}
    if not dry_run:
        try:
            from fno.pr._sync_canonical import run_sync_catchup

            _cu = run_sync_catchup()
            sync_catchup = {
                "outcome": _cu.outcome,
                "pr_number": _cu.pr_number,
                "swept": _cu.swept,
                "detail": _cu.detail,
            }
            if _cu.outcome not in ("disabled", "fresh") and not json_out:
                typer.echo(f"sync catch-up: {_cu.outcome}", err=True)
        except Exception as _cu_exc:  # noqa: BLE001 - never abort the sweep
            sync_catchup = {"outcome": "error", "detail": str(_cu_exc)[:200]}
            if not json_out:
                typer.echo(f"warning: sync catch-up skipped: {_cu_exc}", err=True)

    # Claim GC. This call site reaches every path that fires reconcile - the
    # SessionStart reconcile hook (scripts/lib/reconcile-throttle.sh) and a
    # manual invocation - so a reaper hook on any one caller instead would be
    # a guard on one of N reachable paths. The eval-sweep SessionStart hook
    # sources reconcile-throttle.sh too, but only to reuse its
    # _reconcile_resolve_fno helper; it never calls reconcile_maybe_fire, so
    # it does not reach this reaper. --dry-run
    # propagates to the reaper (one mode contract); best-effort, same posture
    # as sync_catchup above: a reap error is reported and never fails the
    # sweep.
    claim_reap: dict = {"outcome": "not-run"}
    try:
        from fno.claims.cli import _abandonment_probe, _node_settlement
        from fno.claims.core import reap_dead_claims

        # The probe travels with the sweep, not only with the hand-typed verb.
        # This is the UNATTENDED reaper: without it the positive-finding
        # abandonment reap existed on exactly one path an operator has to type,
        # and the SessionStart sweep that actually runs kept every abandoned
        # claim until its TTL. A producer on one of N paths is the same defect
        # as a guard on one of N, from the other side. Same for the
        # node_settlement (x-94f8).
        _reap = reap_dead_claims(
            apply=not dry_run,
            abandonment_probe=_abandonment_probe(),
            node_settlement=_node_settlement(),
        )
        claim_reap = {"outcome": "ok", **_reap}
        _count = _reap["would_reap"] if dry_run else _reap["reaped"]
        _failed = _reap.get("reap_failed") or []
        if not json_out:
            if _count:
                _verb = "would archive" if dry_run else "archived"
                typer.echo(f"claim reap: {_verb} {_count} dead claim(s)", err=True)
            if _failed:
                # A provably-dead claim whose archive move never completed is
                # not the same as zero dead claims found - the reaped=0 count
                # above stays silent about it, so this must not be gated on
                # _count (AC5's "positive marker" rule applies to output too).
                _paths = ", ".join(p for p, _ in _failed[:3])
                _more = "..." if len(_failed) > 3 else ""
                typer.echo(
                    f"warning: claim reap left {len(_failed)} dead claim(s) "
                    f"un-archived (move did not complete): {_paths}{_more}",
                    err=True,
                )
    except Exception as _reap_exc:  # noqa: BLE001 - never abort the sweep
        claim_reap = {"outcome": "error", "detail": str(_reap_exc)[:200]}
        if not json_out:
            typer.echo(f"warning: claim reap skipped: {_reap_exc}", err=True)

    # x-59a6: for a multi-node feature where this run's --pr-number closure
    # claims land under a still-open parent epic, name exactly which sibling
    # ship(s) keep it open - not just that the epic did not close. Without
    # this, a PR that ships one of two required nodes reads as silent
    # (`closed=[thisone]`) rather than naming the outstanding one, and an
    # operator cannot tell "genuinely unfinished" from "closure never fired"
    # for the epic itself. Read-only: never mutates.
    epics_waiting: list[dict] = []
    if closure_claims:
        # On a dry run, `_sim` (when built) already carries the SIMULATED
        # close - `entries` only carries the bind, not completed_at - so
        # reading `entries` here would report a just-claimed node as still
        # outstanding even though `candidates`/`healed_epics` say it would
        # close. Prefer `_sim`; `entries` is the correct fallback when
        # nothing was simulated (e.g. the claim refused, so nothing closes).
        if dry_run:
            _ew_entries = _sim if _sim is not None else entries
        else:
            _ew_entries = read_graph(_graph_path())
        _ew_epics: set = set()
        for _cid in closure_claims:
            _cn = _find_node(_ew_entries, _cid)
            _pid = _cn.get("parent") if _cn else None
            if isinstance(_pid, str):
                _ew_epics.add(_pid)
        for _eid in sorted(_ew_epics):
            _epic = _find_node(_ew_entries, _eid)
            if _epic is None or not node_is_open(_epic):
                continue  # unknown, or already closed/superseded - nothing outstanding
            _outstanding = sorted(
                e["id"] for e in _ew_entries
                if isinstance(e, dict) and e.get("parent") == _eid and node_is_open(e)
            )
            if _outstanding:
                epics_waiting.append({"epic": _eid, "outstanding": _outstanding})
                if not json_out:
                    typer.echo(
                        f"{_eid} still open, waiting on: {', '.join(_outstanding)}",
                        err=True,
                    )

    if json_out:
        payload = {
            "dry_run": dry_run,
            # --pr-number closure binding (x-59a6), reported separately from
            # `closed` below: zero closes must never masquerade as zero claims.
            # `closure_claims` is every id the trailer named; `closure_bound` is
            # the subset newly bound THIS run (already-bound/already-done ids are
            # claimed but not counted here); `closure_refused` names the reason
            # the WHOLE binding was refused, or null.
            "closure_claims": closure_claims,
            "closure_bound": closure_bound,
            "closure_refused": closure_refused,
            # Open-PR binding heals (x-d3c6), reported separately from
            # closure_*: these FILL pr_number/pr_url for visibility and never
            # close a node; advisories name ambiguity / gh read failure.
            "open_pr_bound": open_bound,
            "open_binding_advisories": open_binding_advisories,
            "supersession_unverified": supersession_unverified,
            # Parent epics of this run's closure claims that are still open,
            # each naming its still-open sibling children exactly (x-59a6).
            "epics_waiting": epics_waiting,
            "candidates": [
                {
                    "node_id": r.node_id,
                    "pr_number": r.pr_number,
                    "pr_url": r.pr_url,
                    "plan_path": r.plan_path,
                }
                for r in closeable
            ],
            "closed": closed,
            # Auto-closed container epics (cascade + self-heal sweep); on --dry-run
            # this is the simulated preview of what a real run would heal (codex P3).
            "healed_epics": healed_epics,
            # Nodes closed because they shipped inside a closed node's PR
            # (x-e957). Reported separately from `closed`, whose entries all
            # carry their own pr_number - a contained node has none.
            "contained_closed": contained_closed,
            # Cascade/sweep failures. In the payload because the SessionStart
            # hook reads --json and discards stderr: a leg whose failure is
            # unobservable is indistinguishable from one that never ran.
            "contained_errors": contained_errors,
            # Nodes whose ship a merged revert PR names (stamped unless --dry-run).
            "reverted": reverted_stamped,
            # Canonical-sync catch-up outcome. In the JSON payload rather than
            # only on stderr because the SessionStart hook invokes reconcile with
            # --json and discards stderr - a leg whose result is unobservable is
            # the exact failure mode this feature exists to end.
            "sync_catchup": sync_catchup,
            "claim_reap": claim_reap,
            "failures": [
                {
                    "node_id": r.node_id,
                    "pr_number": r.pr_number,
                    "error": r.error,
                    "kind": r.error_kind,
                    "remedy": r.remedy,
                }
                for r in failures
            ],
            # Closeable records held open by the promise gate (x-5d34): a merged
            # PR whose plan promised work that has not all shipped. In the JSON
            # payload because the SessionStart hook reads --json and discards
            # stderr - a held-open node that prints only to stderr is invisible.
            "promise_unmet": [
                {"node_id": nid, "reason": reason} for nid, reason in promise_unmet
            ],
            "promise_warnings": promise_warnings,
            "supersession_evidence_failures": owed_evidence_failures,
        }
        typer.echo(json.dumps(payload, indent=2))
        # Unresolved PR queries are a partial failure: signal it so unattended
        # callers can detect it from the exit code, not just the JSON body.
        if failures or owed_evidence_failures:
            raise typer.Exit(code=4)
        return

    if (
        not closeable
        and not failures
        and not strandable
        and not strandable_contained
        and not healed_epics
        and not contained_closed
        and not reverted_stamped
        and not promise_unmet
        and not promise_warnings
        and not owed_evidence_failures
        and not closure_claims
    ):
        typer.echo("No merged-PR drift found. Backlog is in sync.")
        return

    if dry_run:
        if closeable:
            typer.echo(
                f"Would close {len(closeable)} node(s) (dry-run, nothing mutated):"
            )
        for r in closeable:
            typer.echo(f"  {r.node_id}  PR #{r.pr_number} MERGED  {r.pr_url or ''}".rstrip())
        if contained_closed:
            # "those PRs" only reads correctly when a PR actually drifted this
            # run. On a heal-only sweep `closeable` is empty, so the 0-header
            # above says nothing-to-do directly over a line saying otherwise.
            _whose = "those PRs" if closeable else "already-merged delivery units"
            typer.echo(
                f"Would close {len(contained_closed)} contained node(s) shipped "
                f"inside {_whose}: " + ", ".join(contained_closed)
            )
        if healed_epics:
            typer.echo(
                f"Would self-heal {len(healed_epics)} container epic(s): "
                + ", ".join(healed_epics)
            )
    else:
        # Suppressed ONLY on a heal-only sweep (no drift candidates at all),
        # where a bare "Closed 0 node(s):" sits above a line saying nodes were
        # closed. With candidates present, "Closed 0" is real signal - it says
        # every one of them was already closed between the scan and the lock.
        if closed or closeable:
            typer.echo(f"Closed {len(closed)} node(s):")
        for c in closed:
            stamp_note = " (plan stamped)" if c["plan_stamped"] else ""
            typer.echo(f"  {c['node_id']}  PR #{c['pr_number']}{stamp_note}")
        if closed:
            typer.echo(f"Retro sentinels written under {retro_pending_dir()}")
        if contained_closed:
            # Same wording rule as the dry-run branch: with no drift this sweep
            # there are no "those PRs" to point at, and "Also" implies a
            # preceding close that did not happen.
            _lead = "Also closed" if closed else "Closed"
            _whose = "those PRs" if closed else "already-merged delivery units"
            typer.echo(
                f"{_lead} {len(contained_closed)} contained node(s) shipped "
                f"inside {_whose} (cost stays on the delivery unit): "
                + ", ".join(contained_closed)
            )
        if healed_epics:
            typer.echo(
                f"Auto-closed {len(healed_epics)} container epic(s) "
                f"(all children complete): " + ", ".join(healed_epics)
            )

    if promise_unmet:
        # Held open, not failed: the PR merged but the plan promised more. The
        # operator resolves it through `fno backlog done <id> --force --reason`
        # (a deliberate half-ship) or by shipping/filing the remainder.
        held = "Holding" if dry_run else "Held"
        typer.echo(
            f"{held} {len(promise_unmet)} node(s) open (merged PR, unmet plan promise):",
            err=True,
        )
        for nid, reason in promise_unmet:
            typer.echo(f"  {nid}: {reason}", err=True)

    if promise_warnings:
        typer.echo("Promise ship-count warnings:", err=True)
        for warning in promise_warnings:
            typer.echo(f"  {warning['node_id']}: {warning['warning']}", err=True)

    if owed_evidence_failures:
        typer.echo("Supersession evidence reads failed:", err=True)
        for failure in owed_evidence_failures:
            typer.echo(
                f"  {failure['successor']} PR #{failure['pr_number']}: "
                f"{failure['error']}\n    {failure['remedy']}",
                err=True,
            )

    if reverted_stamped:
        verb = "Would stamp" if dry_run else "Stamped"
        typer.echo(f"{verb} {len(reverted_stamped)} node(s) reverted:")
        for rev in reverted_stamped:
            typer.echo(f"  {rev['node_id']}  revert PR #{rev['revert_pr']}")

    if failures:
        typer.echo(f"{len(failures)} node(s) could not be resolved:", err=True)
        for r in failures:
            typer.echo(f"  {r.node_id}  PR #{r.pr_number}: {r.error}", err=True)
            if r.remedy:
                typer.echo(f"    {r.remedy}", err=True)
        # Partial reconcile: non-zero exit so callers can detect it.
        raise typer.Exit(code=4)

    if owed_evidence_failures:
        raise typer.Exit(code=4)


# -- maintain (recurring backlog + kanban hygiene sweep) --

def _validity_rg_search(symbol: str) -> Optional[int]:
    """Bounded git-grep file count for a named symbol under the repo root.

    Returns the number of tracked files mentioning ``symbol`` (a validity signal:
    0 files -> the symbol likely no longer exists), or ``None`` when the source
    is unavailable (rg/git missing, timeout, not a repo) so the sweep records it
    unavailable rather than reading a spurious zero. 5 s cap (Locked Decision #7).
    """
    import subprocess

    from fno.paths import resolve_repo_root

    try:
        root = str(resolve_repo_root())
    except Exception:
        return None
    try:
        proc = subprocess.run(
            ["git", "-C", root, "grep", "-l", "--fixed-strings", "-e", symbol],
            capture_output=True, text=True, timeout=_maintain_source_timeout(),
        )
    except (OSError, subprocess.SubprocessError):
        return None
    # git grep exits 1 with no output when there are no matches (not an error).
    if proc.returncode not in (0, 1):
        return None
    return sum(1 for line in proc.stdout.splitlines() if line.strip())


def _maintain_source_timeout() -> float:
    from fno.graph import maintain as _m

    return _m.EVIDENCE_SOURCE_TIMEOUT_S


# Bounds for the retro enrichment items (Discretion #1/#2): a few lines of the
# merged file around the comment, and a truncated diff_hunk. Kept small so both
# sit comfortably inside PACKET_MAX_BYTES alongside the base packet.
_RETRO_REGION_WINDOW = 8
_RETRO_REGION_MAX_BYTES = 1200
_RETRO_HUNK_MAX_BYTES = 400


def _fetch_retro_comment(
    source_pr: int,
    finding_hash: str,
    root: str,
    *,
    repo: Optional[str] = None,
) -> Optional[dict]:
    """Fetch PR ``source_pr``'s inline comments (resolved from ``root``) and return
    the one whose body hash-joins ``finding_hash``, or ``None`` on any failure.

    The hash is the canonical ``content_hash`` ``land`` wrote (function-local import
    so ``graph`` keeps no module-level ``fno.retro`` dependency - that edge runs
    retro -> graph). The gh path is templated with a numeric PR slot, so an injected
    trailer value cannot escape the current repo or the ``<int>`` slot (Pessimist B).
    """
    import subprocess

    from fno.retro.dedup import content_hash  # function-local: no graph->retro cycle

    path = (
        f"repos/{repo}/pulls/{source_pr}/comments"
        if repo
        else f"repos/:owner/:repo/pulls/{source_pr}/comments"
    )
    try:
        proc = subprocess.run(
            ["gh", "api", path, "--paginate", "--slurp"],
            capture_output=True, text=True, cwd=root,
            timeout=_maintain_source_timeout(),
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        raw = json.loads(proc.stdout) if proc.stdout.strip() else []
    except (json.JSONDecodeError, ValueError):
        return None
    # --slurp wraps paginated pages as [[page1...],[page2...]]; flatten, tolerating
    # a non-slurped flat list too (defensive, mirrors fetch_review_comments).
    flat: list[dict] = []
    if isinstance(raw, list):
        for elem in raw:
            if isinstance(elem, list):
                flat.extend(x for x in elem if isinstance(x, dict))
            elif isinstance(elem, dict):
                flat.append(elem)
    for c in flat:
        if content_hash(str(c.get("body", ""))) == finding_hash:
            return c
    return None


def _summarize_review_comment(path: object, line: object, diff_hunk: object) -> str:
    """One-line, bounded summary of the originating ask: cited path/line plus a
    truncated diff_hunk (the shape of the ask, not the whole hunk - Discretion #2)."""
    loc = f"{path}:{line}" if line else str(path)
    hunk = str(diff_hunk or "").strip()
    if len(hunk) > _RETRO_HUNK_MAX_BYTES:
        hunk = hunk[:_RETRO_HUNK_MAX_BYTES] + "…[truncated]"
    return f"reviewer asked at {loc}; diff_hunk: {hunk}" if hunk else f"reviewer asked at {loc}"


def _read_merged_region(root: str, path: str, line: object) -> str:
    """A bounded excerpt of ``path`` in the live merged tree around ``line``.

    Reads ``git show HEAD:<path>`` (Locked Decision #3: live HEAD, not the merge
    SHA); on git failure falls back to a path-safe filesystem read gated by
    ``contained_path_exists`` (CWE-22). Returns "" when neither resolves. The line
    is clamped to the file's bounds (Boundaries: a comment at/past EOF)."""
    import subprocess

    from fno.graph import maintain as _m

    # Defense in depth (CWE-22): `path` originates from a GitHub comment, so reject
    # a non-str or a parent-escaping / absolute path BEFORE any read. `git show
    # HEAD:<path>` already cannot escape the tree object and the fs fallback is
    # gated by contained_path_exists, but guarding up front keeps every reader safe
    # and avoids a TypeError on a non-str root/path.
    if not isinstance(root, str) or not isinstance(path, str):
        return ""
    norm = os.path.normpath(path)
    if os.path.isabs(norm) or norm == ".." or norm.startswith(".." + os.sep):
        return ""

    content: Optional[str] = None
    try:
        proc = subprocess.run(
            ["git", "-C", root, "show", f"HEAD:{path}"],
            capture_output=True, text=True, timeout=_maintain_source_timeout(),
        )
        if proc.returncode == 0:
            content = proc.stdout
    except (OSError, subprocess.SubprocessError):
        content = None
    if content is None:
        if not _m.contained_path_exists(root, path):
            return ""
        try:
            with open(os.path.join(root, path), encoding="utf-8", errors="replace") as fh:
                content = fh.read()
        except OSError:
            return ""
    file_lines = content.splitlines()
    if not file_lines:
        return ""
    anchor = line if isinstance(line, int) and line >= 1 else 1
    anchor = min(anchor, len(file_lines))  # clamp EOF
    lo = max(0, anchor - 1 - _RETRO_REGION_WINDOW)
    hi = min(len(file_lines), anchor + _RETRO_REGION_WINDOW)
    excerpt = "\n".join(file_lines[lo:hi])
    return excerpt[:_RETRO_REGION_MAX_BYTES]


def _validity_retro_source(node: dict) -> dict[str, str]:
    """Per-retro-node enrichment seam (Locked Decision #1): the originating review
    comment (`pr:review-comment`) + the cited file's merged region
    (`git:merged-region:<path>`), for the validity classifier. Returns allowlisted
    `pr:`/`git:` items, or ``{}`` on any failure at any step (fail open, node kept).
    Wired only here in the CLI edge; the hermetic core injects a stub under test."""
    from fno.graph import maintain as _m

    parsed = _m.parse_retro_trailer(node.get("details"))
    if parsed is None:
        return {}
    source_pr, finding_hash = parsed
    if source_pr is None:  # postmortem-sourced: no fetchable PR comment
        return {}
    root = node.get("cwd")
    if not isinstance(root, str) or not os.path.isdir(root):
        return {}
    root_p = os.path.abspath(os.path.expanduser(root))
    comment = _fetch_retro_comment(source_pr, finding_hash, root_p)
    if comment is None:
        return {}
    items: dict[str, str] = {}
    path = comment.get("path")
    line = comment.get("line") or comment.get("original_line")  # outdated diff -> original_line
    items["pr:review-comment"] = _summarize_review_comment(path, line, comment.get("diff_hunk"))
    # Ordered after the comment item so a cap drops the precision add (merged
    # region) before the higher-value ask (Discretion #4 / AC4-EDGE).
    if isinstance(path, str) and path:
        region = _read_merged_region(root_p, path, line)
        if region:
            items[f"git:merged-region:{path}"] = region
    return items


@cli.command("maintain", hidden=True)
def cmd_maintain(
    apply: bool = typer.Option(
        False,
        "--apply",
        help=(
            "Apply the DETERMINISTIC legs (re-scope drift, prune pytest leaks, "
            "backfill url-less pr_url). "
            "The judgment legs (dedup, drain-stale, cap-Now) are ALWAYS "
            "proposal-only regardless of this flag."
        ),
    ),
    json_out: bool = typer.Option(
        False,
        "--json", "-J",
        help="Emit structured JSON instead of a human summary.",
    ),
    recheck: bool = typer.Option(
        False,
        "--recheck",
        help="Validity sweep: re-review watermarked ideas (ignore prior decks).",
    ),
    no_validity: bool = typer.Option(
        False,
        "--no-validity",
        help="Skip the validity sweep (the leg that calls the analyzer).",
    ),
    suspect_reverts: bool = typer.Option(
        False,
        "--suspect-reverts",
        help=(
            "Read-only retro sweep: print drained nodes that carry evidence of "
            "a human curation decision, then exit. Runs no other leg, mutates "
            "nothing, and emits no undefer command - the operator rules on the "
            "list themselves."
        ),
    ),
) -> None:
    """Keep graph.json + the kanban board clean by composing existing verbs.

    Deterministic legs apply under ``--apply``: re-scope project/cwd drift,
    prune pytest-temp leak nodes, and backfill a derived ``pr_url`` onto rows
    carrying a ``pr_number`` with no url. Three are
    judgment calls and only ever PROPOSE (never mutate, regardless of
    ``--apply``): surface near-duplicate idea titles, propose a reversible
    ``defer`` for stale ideas, and report a Now column over its WIP cap. The
    last leg appends a summary to health-history so ``triage trend`` shows the
    board trending cleaner.

    Loop form: ``/loop 1d fno backlog maintain --apply``.

    Best-effort: a malformed row is skipped, a single failed apply does not
    abort the rest, and an empty graph is a clean no-op.
    """
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph.statuses import recompute_statuses
    from fno.graph._intake import _find_node
    from fno.graph.render import make_kanban_column
    from fno.graph.render_html import _load_wip_caps
    from fno.graph import maintain as _maintain

    # Read once and derive status so the judgment legs see accurate states
    # (read_graph applies defaults but does not run the cascade).
    entries = recompute_statuses(read_graph(_graph_path()))

    if suspect_reverts:
        # Short-circuit (x-7dcb): a retro sweep, not another leg. Runs before
        # the claim check and every other detector below - it reads and
        # prints only, so it needs none of their machinery.
        reverts = _maintain.detect_suspect_reverts(entries)
        typer.echo(f"reversals: {len(reverts)} of the drained pile carry evidence of a human decision")
        for r in reverts:
            title = r.title[:60]
            typer.echo(f"  {r.node_id}  {r.priority}  {r.deferred_at[:10]}  {r.signal}  {title}")
        typer.echo("(read-only: no node was changed. Rule on these yourself with `fno backlog undefer <id>...`)")
        return

    # Apply legs must never touch a node a live target session is driving.
    claimed = (
        _require_live_claimed_node_ids("backlog maintain --apply")
        if apply
        else _live_claimed_node_ids()
    )

    # --- detect (all read-only) ---
    workspaces = _maintain.load_workspaces()
    rescope_fixes = _maintain.detect_rescope_fixes(entries, workspaces)
    prune_ids = _maintain.detect_temp_leaks(entries)
    pr_url_fixes = _maintain.detect_url_less_prs(entries)
    pr_url_writable = [f for f in pr_url_fixes if f.pr_url]
    pr_url_unresolvable = [f for f in pr_url_fixes if not f.pr_url]
    dup_groups = _maintain.detect_dup_groups(entries)
    plan_cost_violations = _maintain.detect_shared_plan_cost_violations(entries)
    # Propose-only in v1 even under --apply: a bulk reparent has no human
    # reading a receipt the way intake's one-at-a-time auto-link does.
    try:
        rollup_cands = _maintain.detect_rollup_candidates(entries)
    except Exception:  # noqa: BLE001 - advisory leg; maintain must not break
        rollup_cands = []

    try:
        from fno.config import load_settings

        _maintain_cfg = load_settings().backlog.maintain
        staleness_days = _maintain_cfg.staleness_days
        max_failed_attempts = _maintain_cfg.max_failed_attempts
    except Exception:
        staleness_days = 30
        max_failed_attempts = 3
    stale = _maintain.detect_stale_ideas(entries, staleness_days)

    # G1 stale-ready quarantine leg (x-3236): the propose-only mirror of the
    # failure-defer leg over READY rows abandoned past
    # config.backlog.staleness_days (default 21, distinct from the idea-stage
    # staleness above). --apply defers each with the quarantine reason. Reuses
    # the SAME blast cap so a mass-quarantine can never defer half the board.
    try:
        from fno.config import load_settings

        ready_staleness_days = load_settings().backlog.staleness_days
    except Exception:
        ready_staleness_days = 21
    stale_ready_cands = _maintain.detect_stale_ready(entries, ready_staleness_days)
    stale_ready_truncated = 0
    if len(stale_ready_cands) > _maintain.AUTO_DEFER_BLAST_CAP:
        stale_ready_cands = sorted(
            stale_ready_cands, key=lambda s: (-s.age_days, s.node_id)
        )
        stale_ready_truncated = len(stale_ready_cands) - _maintain.AUTO_DEFER_BLAST_CAP
        stale_ready_cands = stale_ready_cands[: _maintain.AUTO_DEFER_BLAST_CAP]

    now_cap = _load_wip_caps().get("now", 20)
    column_for = make_kanban_column(entries)
    overflow = _maintain.now_overflow(
        entries,
        now_cap,
        column_for,
    )

    # Leg 7: auto-defer failure-prone nodes (#34). Derive the streak from the
    # walker's existing node_failed/node_closed events (Locked Decision #4).
    from fno.graph import failure as _failure

    events = _failure.read_events()
    defer_cands = _maintain.detect_failure_defers(entries, events, max_failed_attempts)
    # Blast-radius guard (Open Question #2): cap per-run auto-defers so a
    # provider-outage mass-failure cannot defer half the board. Truncate the
    # lowest-streak candidates and ALWAYS log the drop (no silent cap).
    defer_truncated = 0
    if len(defer_cands) > _maintain.AUTO_DEFER_BLAST_CAP:
        defer_cands = sorted(defer_cands, key=lambda d: (-d.streak, d.node_id))
        defer_truncated = len(defer_cands) - _maintain.AUTO_DEFER_BLAST_CAP
        defer_cands = defer_cands[: _maintain.AUTO_DEFER_BLAST_CAP]

    # --- apply (deterministic legs only) ---
    applied_rescope: list[str] = []
    applied_prune: list[str] = []
    applied_defers: list[dict] = []
    applied_stale_ready: list[dict] = []
    applied_pr_urls: list[dict] = []
    skipped_claimed: list[str] = []

    if apply and (
        rescope_fixes or prune_ids or defer_cands or stale_ready_cands or pr_url_writable
    ):
        # Batch every change under ONE locked mutation so the board renders once,
        # not per node (Domain Pitfall). Each item is guarded so one failure
        # never strands the rest (AC1-ERR).
        def mutator(ents):
            current_claimed = claimed | _require_live_claimed_node_ids(
                "backlog maintain --apply"
            )
            applied_rescope.clear()
            applied_prune.clear()
            applied_defers.clear()
            applied_stale_ready.clear()
            applied_pr_urls.clear()
            skipped_claimed.clear()
            prune_set: set[str] = set()
            for fix in rescope_fixes:
                if fix.node_id in current_claimed:
                    skipped_claimed.append(fix.node_id)
                    continue
                try:
                    n = _find_node(ents, fix.node_id)
                    if not n:
                        continue
                    # Only project/cwd are ever touched - never priority/status.
                    n["project"] = fix.new_project
                    n["cwd"] = fix.new_cwd
                    applied_rescope.append(fix.node_id)
                except Exception as exc:  # noqa: BLE001 - one bad row must not abort
                    typer.echo(
                        f"warning: re-scope of {fix.node_id} failed: {exc}", err=True
                    )
            for nid in prune_ids:
                if nid in current_claimed:
                    skipped_claimed.append(nid)
                    continue
                prune_set.add(nid)
                applied_prune.append(nid)
            if prune_set:
                # Mirror `remove`: drop the node AND clean dangling blocked_by refs.
                for e in ents:
                    blocked = e.get("blocked_by")
                    if blocked:
                        e["blocked_by"] = [b for b in blocked if b not in prune_set]
                ents = [e for e in ents if e.get("id") not in prune_set]
            # Leg 2b: backfill a derived pr_url onto url-less pr_number rows.
            # Re-check inside the lock so a url written since the pre-lock scan
            # is never overwritten (a present url always outranks a derived one).
            for fix in pr_url_writable:
                if fix.node_id in current_claimed:
                    skipped_claimed.append(fix.node_id)
                    continue
                try:
                    n = _find_node(ents, fix.node_id)
                    if not n or n.get("pr_url") or n.get("pr_number") != fix.pr_number:
                        continue
                    n["pr_url"] = fix.pr_url
                    applied_pr_urls.append(
                        {"node_id": fix.node_id, "pr_url": fix.pr_url}
                    )
                except Exception as exc:  # noqa: BLE001 - one bad row must not abort
                    typer.echo(
                        f"warning: pr_url backfill of {fix.node_id} failed: {exc}",
                        err=True,
                    )
            # Leg 7: auto-defer failure-prone nodes (#34). Mirrors cmd_defer's
            # field-set. Re-check live state INSIDE the lock (Concurrency): a
            # node done or deferred between the read and now must not be touched.
            # Auto-defer is a state change (unlike rescope/prune's project-cwd
            # touch-ups), so it also RE-SAMPLES live claims inside the lock - a
            # node a session claimed between the pre-lock read and now must not
            # be deferred ("claimed between read and write", Failure Modes /
            # Concurrency). The strict in-lock snapshot above covers every leg.
            defer_claimed = current_claimed
            for cand in defer_cands:
                if cand.node_id in defer_claimed:
                    skipped_claimed.append(cand.node_id)
                    continue
                try:
                    n = _find_node(ents, cand.node_id)
                    if not n:
                        continue
                    if n.get("completed_at") or n.get("deferred_at"):
                        continue  # raced to done/deferred; leave it
                    reason = (
                        f"{_failure.AUTO_FAILURE_SENTINEL} {cand.streak} "
                        f"consecutive failed attempts"
                    )
                    # Mirror cmd_defer: clear claim/completion so the cascade
                    # derives status: deferred, then set the deferred fields.
                    n["locked_by"] = None
                    n["claimed_at"] = None
                    n["completed_at"] = None
                    n["deferred_at"] = datetime.now(timezone.utc).isoformat()
                    n["deferred_reason"] = reason
                    applied_defers.append(
                        {"node_id": cand.node_id, "streak": cand.streak, "reason": reason}
                    )
                except Exception as exc:  # noqa: BLE001 - one bad row must not abort
                    typer.echo(
                        f"warning: auto-defer of {cand.node_id} failed: {exc}", err=True
                    )
            # G1 stale-ready quarantine (x-3236): the reversible defer for a ready
            # node abandoned past the threshold. Same in-lock re-sample as the
            # failure leg (a node claimed/done/deferred since the read is left
            # alone - the "quarantine racing a live claim must lose" race rule).
            sr_claimed = current_claimed
            for cand in stale_ready_cands:
                if cand.node_id in sr_claimed:
                    skipped_claimed.append(cand.node_id)
                    continue
                try:
                    n = _find_node(ents, cand.node_id)
                    if not n:
                        continue
                    if n.get("completed_at") or n.get("deferred_at"):
                        continue  # raced to done/deferred; leave it
                    # Re-run the predicate under the lock: a candidate that
                    # gained a movement signal since the pre-lock scan (e.g. a
                    # PR attached -> now in-review, or a fresh plan edit) is no
                    # longer stale, and deferring it would sink active work into
                    # the pile (deferred outranks in-review). Re-check on `n`.
                    if not _maintain.is_stale_ready(
                        n, datetime.now(timezone.utc), ready_staleness_days
                    ):
                        continue
                    n["locked_by"] = None
                    n["claimed_at"] = None
                    n["completed_at"] = None
                    n["deferred_at"] = datetime.now(timezone.utc).isoformat()
                    n["deferred_reason"] = _maintain.STALE_QUARANTINE_REASON
                    applied_stale_ready.append({
                        "node_id": cand.node_id,
                        "age_days": cand.age_days,
                        "reason": _maintain.STALE_QUARANTINE_REASON,
                    })
                except Exception as exc:  # noqa: BLE001 - one bad row must not abort
                    typer.echo(
                        f"warning: stale-ready defer of {cand.node_id} failed: {exc}",
                        err=True,
                    )
            return ents

        locked_mutate_graph(_graph_path(), mutator)

    # --- leg 8: validity sweep (proposal-only, ALWAYS - never mutates) ---
    # Runs even under --apply as proposal-only; a single analyzer call reviews the
    # oldest stale ideas and writes an immutable evidence deck. Self-limiting:
    # once the pile is watermarked, later runs find 0 eligible and skip the call.
    validity_result = None
    if not no_validity:
        try:
            from fno.config import load_settings

            _vcfg = load_settings().backlog.maintain
            v_days, v_batch = _vcfg.validity_days, _vcfg.validity_batch_size
        except Exception:
            v_days, v_batch = _maintain.VALIDITY_DAYS_DEFAULT, _maintain.VALIDITY_BATCH_DEFAULT

        from fno import paths as _paths

        try:
            _deck_dir = _paths.state_dir() / "validity-decks"
        except Exception:
            _deck_dir = None

        if _deck_dir is not None:
            def _exists_factory(node):
                root = node.get("cwd")
                if not isinstance(root, str) or not os.path.isdir(root):
                    return None  # repo unavailable -> path evidence recorded unavailable
                root_p = os.path.abspath(os.path.expanduser(root))
                # `rel` is extracted from untrusted node text; contained_path_exists
                # rejects an absolute or `../` escape from the repo root (CWE-22).
                return lambda rel: _maintain.contained_path_exists(root_p, rel)

            # Re-read seam: the sweep calls this AFTER the analyzer returns, so a
            # node that raced to claimed/done/deferred DURING analysis voids its
            # recommendation (AC4-EDGE).
            def _reread():
                return recompute_statuses(read_graph(_graph_path()))

            validity_result = _maintain.run_validity_sweep(
                entries,
                validity_days=v_days,
                batch_size=v_batch,
                out_dir=_deck_dir,
                claimed_ids=frozenset(
                    claimed
                    | (
                        _require_live_claimed_node_ids("backlog maintain --apply")
                        if apply
                        else _live_claimed_node_ids()
                    )
                ),
                recheck=recheck,
                exists_factory=_exists_factory,
                search=_validity_rg_search,
                retro_source=_validity_retro_source,
                reread=_reread,
            )
            if validity_result.error and not json_out:
                typer.echo(f"validity: {validity_result.error}", err=True)
                raise typer.Exit(code=1)

    # --- report leg: append a summary to health-history (best-effort) ---
    report = {
        "scope": "maintain",
        "applied": apply,
        "rescoped": len(applied_rescope) if apply else len(rescope_fixes),
        "pruned": len(applied_prune) if apply else len(prune_ids),
        "pr_url_backfilled": len(applied_pr_urls) if apply else len(pr_url_writable),
        "pr_url_unresolvable": len(pr_url_unresolvable),
        "dedup_groups": len(dup_groups),
        "shared_plan_cost_violations": len(plan_cost_violations),
        "rollup_candidates": len(rollup_cands),
        "stale_ideas": len(stale),
        "now_overflow": list(overflow) if overflow else None,
        "skipped_claimed": len(skipped_claimed),
        "auto_deferred": len(applied_defers) if apply else len(defer_cands),
        # Carry node + reason so a sweep's auto-defers are never silent (a
        # silent auto-defer is a design bug, per UI State Machines).
        "auto_deferred_nodes": applied_defers
        if apply
        else [{"node_id": c.node_id, "streak": c.streak} for c in defer_cands],
        "auto_defer_truncated": defer_truncated,
        "stale_ready": len(applied_stale_ready) if apply else len(stale_ready_cands),
        "stale_ready_nodes": applied_stale_ready
        if apply
        else [{"node_id": c.node_id, "age_days": c.age_days} for c in stale_ready_cands],
        "stale_ready_truncated": stale_ready_truncated,
    }
    try:
        from fno.health_monitor import append_history

        append_history(report, [])
    except Exception as exc:  # noqa: BLE001 - report leg is non-fatal
        typer.echo(f"warning: maintain health-history append failed: {exc}", err=True)

    if json_out:
        payload = {
            "applied": apply,
            "rescope": {
                "applied": applied_rescope if apply else [],
                "candidates": [
                    {
                        "node_id": f.node_id,
                        "new_project": f.new_project,
                        "new_cwd": f.new_cwd,
                    }
                    for f in rescope_fixes
                ],
            },
            "prune": {
                "applied": applied_prune if apply else [],
                "candidates": prune_ids,
            },
            "pr_url_backfill": {
                "applied": applied_pr_urls if apply else [],
                "candidates": [
                    {"node_id": f.node_id, "pr_number": f.pr_number, "pr_url": f.pr_url}
                    for f in pr_url_writable
                ],
                "unresolvable": [
                    {"node_id": f.node_id, "pr_number": f.pr_number, "cwd": f.cwd}
                    for f in pr_url_unresolvable
                ],
            },
            "dedup_groups": dup_groups,
            "shared_plan_cost_violations": [
                {"plan_path": v.plan_path, "nodes": v.nodes} for v in plan_cost_violations
            ],
            "rollup_candidates": [
                {"node_id": n, "epic_id": e, "score": sc} for n, e, sc in rollup_cands
            ],
            "stale_ideas": [{"node_id": s.node_id, "age_days": s.age_days} for s in stale],
            "now_overflow": list(overflow) if overflow else None,
            "skipped_claimed": skipped_claimed,
            "auto_defer": {
                "applied": applied_defers if apply else [],
                "candidates": [
                    {"node_id": c.node_id, "streak": c.streak} for c in defer_cands
                ],
                "truncated": defer_truncated,
            },
            "stale_ready": {
                "applied": applied_stale_ready if apply else [],
                "candidates": [
                    {"node_id": c.node_id, "age_days": c.age_days}
                    for c in stale_ready_cands
                ],
                "truncated": stale_ready_truncated,
            },
        }
        if validity_result is not None:
            payload["validity"] = {
                "eligible": validity_result.eligible,
                "counts": validity_result.counts,
                "deck": validity_result.deck_md,
                "degraded": validity_result.degraded,
                "stale": validity_result.stale,
                "error": validity_result.error,
            }
        typer.echo(json.dumps(payload, indent=2))
        if validity_result is not None and validity_result.error:
            raise typer.Exit(code=1)
        return

    # --- human per-leg summary (a no-op run is visibly distinct, AC1-UI) ---
    # AC1-UI: every category prints its count, zero included - a silent
    # category reads as "nothing to do" when it may be "nothing resolved".
    if apply:
        # "written N of M", never a bare N: the in-lock loop skips a row that
        # raced to a url or a different pr_number, so 120-of-159 would read
        # exactly like a complete run.
        typer.echo(
            f"pr-url written {len(applied_pr_urls)} of {len(pr_url_writable)} | "
            f"pr-url unresolvable {len(pr_url_unresolvable)}"
        )
    else:
        typer.echo(
            f"pr-url proposed {len(pr_url_writable)} | "
            f"pr-url unresolvable {len(pr_url_unresolvable)}"
        )
    for f in pr_url_unresolvable:
        typer.echo(
            f"  unresolvable pr_url {f.node_id} (PR #{f.pr_number}, cwd={f.cwd or 'unset'})"
        )

    if apply:
        typer.echo(
            f"re-scoped {len(applied_rescope)} | pruned {len(applied_prune)} | "
            f"auto-deferred {len(applied_defers)} | "
            f"stale-ready-deferred {len(applied_stale_ready)} | "
            f"dedup-groups {len(dup_groups)} | rollup-candidates "
            f"{len(rollup_cands)} | stale-ideas {len(stale)} | "
            f"now-overflow {'yes' if overflow else 'no'} | "
            f"skipped-claimed {len(skipped_claimed)}"
        )
    else:
        typer.echo(
            f"re-scope candidates {len(rescope_fixes)} | prune candidates "
            f"{len(prune_ids)} | auto-defer candidates {len(defer_cands)} | "
            f"stale-ready candidates {len(stale_ready_cands)} | "
            f"dedup-groups {len(dup_groups)} | rollup-candidates "
            f"{len(rollup_cands)} | stale-ideas "
            f"{len(stale)} | now-overflow {'yes' if overflow else 'no'}  "
            f"(run with --apply to apply the deterministic legs)"
        )

    for rf in rescope_fixes:
        verb = "re-scoped" if (apply and rf.node_id in applied_rescope) else "would re-scope"
        typer.echo(f"  {verb} {rf.node_id} -> project={rf.new_project} cwd={rf.new_cwd}")
    for nid in prune_ids:
        verb = "pruned" if (apply and nid in applied_prune) else "would prune (temp-cwd leak)"
        typer.echo(f"  {verb} {nid}")
    if apply:
        for d in applied_defers:
            typer.echo(
                f"  auto-deferred {d['node_id']} ({d['streak']} consecutive "
                f"failures): {d['reason']}"
            )
    else:
        for c in defer_cands:
            typer.echo(
                f"  would auto-defer {c.node_id} ({c.streak} consecutive failures, "
                f">= {max_failed_attempts}): fno backlog undefer {c.node_id} to recover"
            )
    if defer_truncated:
        typer.echo(
            f"  NOTE: auto-defer blast cap hit - {defer_truncated} further "
            f"candidate(s) NOT deferred this run "
            f"(cap {_maintain.AUTO_DEFER_BLAST_CAP}); re-run to continue"
        )
    if apply:
        for d in applied_stale_ready:
            typer.echo(
                f"  stale-ready deferred {d['node_id']} ({d['age_days']}d "
                f"unmoved): {d['reason']}"
            )
    else:
        for sc in stale_ready_cands:
            typer.echo(
                f"  would quarantine stale-ready {sc.node_id} ({sc.age_days}d "
                f"unmoved, >{ready_staleness_days}d): fno backlog undefer "
                f"{sc.node_id} to recover"
            )
    if stale_ready_truncated:
        typer.echo(
            f"  NOTE: stale-ready blast cap hit - {stale_ready_truncated} further "
            f"candidate(s) NOT quarantined this run "
            f"(cap {_maintain.AUTO_DEFER_BLAST_CAP}); re-run to continue"
        )
    for group in dup_groups:
        typer.echo(f"  near-duplicate ideas (merge/supersede by hand): {', '.join(group)}")
    for v in plan_cost_violations:
        typer.echo(
            f"  shared-plan cost double-count {v.plan_path}: {', '.join(v.nodes)} "
            f"all carry cost_usd (a plan is one PR is one node; one node is the "
            f"delivery unit, the rest are contained). Read-only: pick the unit "
            f"and `fno backlog update <other> --plan-path null` by hand."
        )
    for nid, epic_id, score in rollup_cands:
        typer.echo(
            f"  rollup candidate {nid} -> {epic_id} ({score:.2f}): "
            f"fno backlog update {nid} --parent {epic_id}"
        )
    # Bounded stale-idea receipt: the per-candidate echo scaled to one line per
    # stale idea (hundreds on a mature graph), swamping the one-screen report.
    # Summary + 10 oldest + one drain command instead. The drain lands the whole
    # batch in one locked write via the variadic `defer` (one read/backup/write/
    # render regardless of id count); --no-validity skips the analyzer call that
    # made an earlier `maintain -J` hang past 120s.
    if stale:
        ages = sorted(s.age_days for s in stale)
        oldest = sorted(stale, key=lambda s: s.age_days, reverse=True)[:10]
        typer.echo(
            f"  stale ideas: {len(stale)} (age {ages[0]}-{ages[-1]}d) - "
            f"drain in one locked write:"
        )
        for s in oldest:
            typer.echo(f"    {s.node_id} ({s.age_days}d)")
        if len(stale) > 10:
            typer.echo(f"    (showing 10 of {len(stale)} oldest)")
        typer.echo(
            f"    fno backlog maintain --no-validity -J "
            f"| jq -r '.stale_ideas[].node_id' "
            f"| xargs fno backlog defer -R 'stale >{staleness_days}d, drained by maintain'"
        )
    else:
        typer.echo("  stale ideas: 0")
    if overflow:
        count, cap = overflow
        typer.echo(
            f"  Now over WIP cap ({count} > {cap}): run `fno backlog triage propose` "
            f"to demote lower-priority work (never auto-reprioritized)"
        )
    if skipped_claimed:
        typer.echo(
            f"  skipped {len(skipped_claimed)} live-claimed node(s): "
            f"{', '.join(skipped_claimed)}"
        )

    if validity_result is not None:
        for w in validity_result.warnings:
            typer.echo(f"  validity config: {w}", err=True)
        if validity_result.eligible == 0:
            typer.echo("validity: 0 eligible ideas")
        else:
            counts = validity_result.counts
            tag = " (DEGRADED: analyzer unavailable)" if validity_result.degraded else ""
            stale_note = f", {validity_result.stale} stale" if validity_result.stale else ""
            typer.echo(
                f"validity: reviewed {validity_result.eligible} ideas{tag} -> "
                f"promote {counts.get('promote', 0)} | keep {counts.get('keep', 0)} | "
                f"supersede {counts.get('supersede', 0)} | needs-human "
                f"{counts.get('needs-human', 0)}{stale_note}"
            )
            typer.echo(f"  deck: {validity_result.deck_md}")


# -- reprioritize --

@cli.command("reprioritize", hidden=True)
def cmd_reprioritize(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    priority: str = typer.Argument(..., help="New priority: p0|p1|p2|p3"),
) -> None:
    from fno.graph._constants import PRIORITY_ORDER, has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    old_holder: list = [None]

    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        old_holder[0] = node.get("priority", "p2")
        node["priority"] = priority
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"Reprioritized {task_id}: {old_holder[0]} -> {priority}")


# -- rank --

@cli.command("rank")
def cmd_rank(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX) to rank"),
    top: bool = typer.Option(
        False, "--top", help="Pin to the front of its (column, project) lane"
    ),
    bottom: bool = typer.Option(
        False, "--bottom", help="Send to the back of the ranked band in its lane"
    ),
    before: Optional[str] = typer.Option(
        None, "--before", help="Place just before a ranked anchor in the same lane"
    ),
    after: Optional[str] = typer.Option(
        None, "--after", help="Place just after a ranked anchor in the same lane"
    ),
    clear: bool = typer.Option(
        False, "--clear", help="Clear the rank (rejoin the unranked priority flow)"
    ),
) -> None:
    """Curate a node's position within its (column, project) board lane.

    Rank is a nullable float ordered ahead of the shared epic-aware work-order
    suffix within a lane; it never changes a node's column. ``--before`` /
    ``--after`` require a *ranked* anchor in the same lane - seed one with
    ``--top`` first. Float midpoints mean inserts never renumber siblings.
    """
    import math

    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node
    from fno.graph.render import make_kanban_column, _project_key

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    chosen = [
        name
        for name, on in (
            ("--top", top),
            ("--bottom", bottom),
            ("--before", before is not None),
            ("--after", after is not None),
            ("--clear", clear),
        )
        if on
    ]
    if len(chosen) != 1:
        typer.echo(
            "Error: pass exactly one of --top / --bottom / --before <id> / "
            "--after <id> / --clear",
            err=True,
        )
        raise typer.Exit(code=1)

    anchor_id = before if before is not None else after
    if anchor_id is not None and not has_node_id_prefix(anchor_id):
        typer.echo(f"Error: anchor must be a <prefix>-<4..8 hex> node id, got '{anchor_id}'", err=True)
        raise typer.Exit(code=1)

    result: dict = {}

    def _is_ranked(e: dict) -> bool:
        # Match render._rank_band: a non-finite OR huge-int rank (from a
        # hand-edited graph.json) is treated as unranked, so a poisoned peer
        # can't corrupt the --top/--bottom/midpoint arithmetic or persist a
        # NaN/inf rank. float() guards the OverflowError a giant int raises.
        r = e.get("rank")
        if isinstance(r, bool) or not isinstance(r, (int, float)):
            return False
        try:
            return math.isfinite(float(r))
        except (OverflowError, ValueError):
            return False

    def mutator(entries):
        try:
            column_for = make_kanban_column(entries, strict_claims=True)
        except Exception as exc:
            typer.echo(
                "Error: live claim state is unavailable; rank refused without "
                "changing the graph.",
                err=True,
            )
            raise typer.Exit(code=1) from exc

        def _lane(e: dict) -> tuple:
            return (column_for(e), _project_key(e))

        def _lane_label(e: dict) -> str:
            col, proj = _lane(e)
            return f"{col or '(off-board)'}/{proj}"

        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        # _find_node fuzzy-resolves partial ids (e.g. `ab-9728`); compare on
        # the RESOLVED id everywhere below so the target is excluded from its
        # own peer set and the self-anchor guard fires for partial input.
        tid = node.get("id") or task_id

        if clear:
            node["rank"] = None
            result.update(action="--clear", rank=None, lane=_lane_label(node), id=tid)
            return entries

        target_lane = _lane(node)
        # Lane peers exclude the target; ranked peers (anchor included) sorted
        # ascending give us the band to insert into.
        peers = [
            e for e in entries if e.get("id") != tid and _lane(e) == target_lane
        ]
        ranked = sorted((e for e in peers if _is_ranked(e)), key=lambda e: e["rank"])

        if top:
            new_rank = (ranked[0]["rank"] - 1.0) if ranked else 0.0
            action = "--top"
        elif bottom:
            new_rank = (ranked[-1]["rank"] + 1.0) if ranked else 0.0
            action = "--bottom"
        else:
            anchor = _find_node(entries, anchor_id)
            if not anchor:
                typer.echo(f"Error: anchor {anchor_id} not found", err=True)
                raise typer.Exit(code=1)
            if anchor.get("id") == tid:
                typer.echo("Error: cannot rank a node relative to itself", err=True)
                raise typer.Exit(code=1)
            if _lane(anchor) != target_lane:
                typer.echo(
                    f"Error: cross-lane rank rejected: {task_id} is in "
                    f"{_lane_label(node)} but anchor {anchor_id} is in "
                    f"{_lane_label(anchor)}. Rank is scoped per (column, project) lane.",
                    err=True,
                )
                raise typer.Exit(code=1)
            if not _is_ranked(anchor):
                typer.echo(
                    f"Error: anchor {anchor_id} is unranked; rank it first "
                    f"(e.g. `fno backlog rank {anchor_id} --top`) or use --top/--bottom.",
                    err=True,
                )
                raise typer.Exit(code=1)
            anchor_rank = float(anchor["rank"])
            if before is not None:
                lowers = [e["rank"] for e in ranked if e["rank"] < anchor_rank]
                lo = max(lowers) if lowers else None
                new_rank = (anchor_rank - 1.0) if lo is None else (lo + anchor_rank) / 2.0
                action = f"--before {anchor_id}"
            else:
                highers = [e["rank"] for e in ranked if e["rank"] > anchor_rank]
                hi = min(highers) if highers else None
                new_rank = (anchor_rank + 1.0) if hi is None else (anchor_rank + hi) / 2.0
                action = f"--after {anchor_id}"

        node["rank"] = new_rank
        result.update(action=action, rank=new_rank, lane=_lane_label(node), id=tid)
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    if result.get("action") == "--clear":
        typer.echo(
            f"Cleared rank on {result['id']} (rejoined the unranked flow in {result['lane']})"
        )
    else:
        typer.echo(
            f"Ranked {result['id']} {result['action']} (rank={result['rank']}) in {result['lane']}"
        )
    _project_plans_from_graph([result["id"]])


# -- archive --

_ARCHIVE_SKIP_REASONS = (
    "referenced-by-open-node",
    "related-peer-not-archived",
    "too-recent",
    "no-parseable-timestamp",
)


def _archive_bucket_counts(skipped: list) -> dict[str, int]:
    """Tally ``skipped`` by ``_skip`` reason, zero-filled for every known reason.

    Zero-filled so the receipt always names all four buckets (x-a023): a run
    that holds back 0 for a reason reads as "checked, none held", not as an
    absent line a reader has to interpret as either "zero" or "not measured".
    A reason not in ``_ARCHIVE_SKIP_REASONS`` still lands in the dict and the
    stdout receipt prints it, so a new ``_skip`` reason reads in the receipt
    (and in groom's regex sum over these lines) instead of vanishing.
    """
    held = {reason: 0 for reason in _ARCHIVE_SKIP_REASONS}
    for s in skipped:
        held[s["_skip"]] = held.get(s["_skip"], 0) + 1
    return held


def _receipt_reason_order(held: dict[str, int]) -> list[str]:
    extras = set(held) - set(_ARCHIVE_SKIP_REASONS)
    return list(_ARCHIVE_SKIP_REASONS) + sorted(extras)


@cli.command(
    "archive",
    hidden=True,
    epilog="Paired verb: `fno backlog unarchive <id>` moves one node back into "
    "the working graph. Unlike `remove`, archiving keeps the node readable.",
)
def cmd_archive(
    apply: bool = typer.Option(
        False, "--apply", help="Move the entries (default: dry-run, report only)."
    ),
    older_than_days: int = typer.Option(
        30, "--older-than-days", help="Only archive terminal nodes older than N days."
    ),
    roadmap_id: Optional[str] = typer.Option(
        None, "--roadmap-id", help="Restrict the sweep to this roadmap group."
    ),
) -> None:
    """Sweep old terminal (done/superseded) nodes into graph-archive.json.

    Dry-run by default: prints how many would move and why some are held back.
    ``--apply`` mutates under the graph lock (archive written first, then the
    working graph, so a crash duplicates rather than loses). Never archives a
    node an OPEN node still references through a hard edge (blocker, parent,
    supersede target). A SOFT edge (the open node's ``related`` peer or
    ``source_node_id`` origin) does not hold the target: the reference is
    stripped from the open side at apply time and the node leaves; the
    read-through fallback keeps its id resolvable.

    Every run's receipt names all four held-back buckets plus the soft-edge
    strip count, and every run emits a ``graph_archive_swept`` event, dry-run
    included: a leg that runs daily and reports bare "ok" is indistinguishable
    from one that never ran (x-a023) - the count that matters is often the
    held-back one, not the moved one.
    """
    from datetime import datetime, timezone

    from fno.graph.store import (
        _apply_graph_defaults,
        _read_json,
        _write_json,
        read_graph,
        locked_mutate_graph,
        GraphCorruptError,
    )
    from fno.graph.archive import (
        merge_into_archive,
        partition_for_archive,
        release_soft_edges,
        stamp_archived_at,
    )

    now = datetime.now(timezone.utc)

    def _split(entries):
        # Guard against the FULL graph so an open node in another roadmap that
        # references one of these terminal nodes (blocker/parent/supersede) is
        # still protected; only the archive SET is roadmap-restricted.
        to_archive, _remaining_pool, skipped = partition_for_archive(
            entries, older_than_days, now
        )
        if roadmap_id:
            to_archive = [e for e in to_archive if e.get("roadmap_id") == roadmap_id]
            # `skipped` is restricted with the same predicate so the receipt's
            # held counts describe what THIS run considered; the full-graph
            # guard above already protects to_archive from cross-roadmap
            # references, and counting other roadmaps' holds here would pin
            # them on this run's gate in the receipt and the swept event.
            skipped = [e for e in skipped if e.get("roadmap_id") == roadmap_id]
        arch_ids = {e["id"] for e in to_archive if isinstance(e, dict) and e.get("id")}
        remaining = [e for e in entries if e.get("id") not in arch_ids]
        return to_archive, remaining, skipped

    def _echo_receipt(moved: int, held: dict[str, int], stripped: int = 0) -> None:
        for reason in _receipt_reason_order(held):
            typer.echo(f"  held back ({reason}): {held[reason]}")
        typer.echo(f"  soft edges stripped from open nodes: {stripped}")

    def _emit_swept_event(
        moved: int, held: dict[str, int], stripped: int = 0, mode: str = "apply"
    ) -> None:
        try:
            from fno.events import _build, append_event
            from fno.paths import state_dir

            event = _build(
                "graph_archive_swept",
                "backlog",
                {
                    "moved": moved,
                    "held_referenced": held["referenced-by-open-node"],
                    "held_related": held["related-peer-not-archived"],
                    "held_too_recent": held["too-recent"],
                    "held_no_timestamp": held["no-parseable-timestamp"],
                    "soft_edges_stripped": stripped,
                    "mode": mode,
                    "older_than_days": older_than_days,
                },
            )
            append_event(event, state_dir() / "events.jsonl")
        except Exception:  # noqa: BLE001 - the sweep itself must not fail on a bad event write
            pass

    if not apply:
        to_archive, _rem, skipped = _split(read_graph(_graph_path()))
        typer.echo(
            f"[dry-run] would archive {len(to_archive)} terminal node(s) "
            f"older than {older_than_days}d to {_archive_path()}"
        )
        _echo_receipt(len(to_archive), _archive_bucket_counts(skipped))
        typer.echo("Re-run with --apply to move them.")
        # Every run emits, dry-run included: a leg that went silent must stay
        # distinguishable from one that never ran, and the dry-run leg (the
        # daily groom rehearsal) is the one most likely to break quietly.
        _emit_swept_event(len(to_archive), _archive_bucket_counts(skipped), mode="dry-run")
        return

    receipt: dict = {"moved": 0, "held": _archive_bucket_counts([]), "stripped": 0}

    def mutator(entries):
        to_archive, remaining, skipped = _split(entries)
        receipt["held"] = _archive_bucket_counts(skipped)
        if not to_archive:
            return entries
        receipt["moved"] = len(to_archive)

        # Soft-edge release BEFORE the archive write: strip the soon-archived
        # ids from staying nodes' related lists / source_node_id, so the working
        # graph never keeps a soft pointer at an archived id. One write, under
        # the same lock as everything else here.
        arch_ids = {
            e["id"] for e in to_archive if isinstance(e, dict) and e.get("id")
        }
        remaining, stripped = release_soft_edges(remaining, arch_ids)
        receipt["stripped"] = stripped

        # Archive-first: append (deduped) and write the archive BEFORE returning
        # `remaining` for the graph write, so a crash leaves a duplicate (healed
        # on the next sweep) rather than a lost node.
        archive_path = _archive_path()
        try:
            existing = _apply_graph_defaults(_read_json(archive_path))
        except GraphCorruptError:
            typer.echo(f"Warning: {archive_path} corrupt, starting fresh archive", err=True)
            existing = []
        archive_path.parent.mkdir(parents=True, exist_ok=True)
        stamped = stamp_archived_at(to_archive, now.strftime("%Y-%m-%dT%H:%M:%SZ"))
        _write_json(merge_into_archive(existing, stamped), archive_path)
        return remaining

    locked_mutate_graph(_graph_path(), mutator)
    if receipt["moved"]:
        typer.echo(f"Archived {receipt['moved']} terminal node(s) to {_archive_path()}")
    else:
        typer.echo("No terminal nodes eligible to archive.")
    _echo_receipt(receipt["moved"], receipt["held"], receipt["stripped"])
    _emit_swept_event(receipt["moved"], receipt["held"], receipt["stripped"])


@cli.command(
    "archive-dedupe-ids",
    hidden=True,
    epilog="The id generator once checked only the working graph, so a freed "
    "id could be reminted while the archive still held a different node under "
    "it. mint_node_id now reads the archive too; this repairs what predates "
    "that fix.",
)
def cmd_archive_dedupe_ids(
    apply: bool = typer.Option(
        False,
        "--apply",
        help="Remint the colliding archive entries (default: dry-run, report only).",
    ),
) -> None:
    """Remint archive-side ids that collide with a live working-graph id.

    Reminting the working-graph side would break every open reference to it
    today (blockers, parents, branches, worktrees, open PRs); the archived
    side is passive history, so IT moves, keeping its old id as
    ``previous_id`` -- `fno backlog get <old-id>` still resolves it after.
    """
    from fno.graph.store import (
        _apply_graph_defaults,
        _read_json,
        _write_json,
        read_graph,
        locked_mutate_graph,
        GraphCorruptError,
    )
    from fno.graph.archive import remint_archive_collisions

    archive_path = _archive_path()

    def _read_archive_or_exit() -> list:
        try:
            return _apply_graph_defaults(_read_json(archive_path)) if archive_path.exists() else []
        except GraphCorruptError:
            typer.echo(f"Error: {archive_path} is corrupt", err=True)
            raise typer.Exit(code=1)

    if not apply:
        working_ids = {
            nid for e in read_graph(_graph_path())
            if isinstance(e, dict) and isinstance(nid := e.get("id"), str)
        }
        _, remap = remint_archive_collisions(working_ids, _read_archive_or_exit())
        typer.echo(f"[dry-run] would remint {len(remap)} archive id(s):")
        for old, new in sorted(remap.items()):
            typer.echo(f"  {old} -> {new}")
        if remap:
            typer.echo("Re-run with --apply to write it.")
        return

    remap_holder: dict = {}

    def mutator(entries):
        # Never mutates the working graph; runs under its lock only to
        # serialize the archive read-modify-write against a concurrent
        # `archive --apply`, which also writes archive.json under this same
        # lock. Reading the archive HERE (not before the lock) is load-bearing:
        # a pre-lock read would go stale under that race and the write below
        # would clobber whatever the concurrent sweep just archived.
        working_ids = {
            nid for e in entries
            if isinstance(e, dict) and isinstance(nid := e.get("id"), str)
        }
        patched, remap = remint_archive_collisions(working_ids, _read_archive_or_exit())
        remap_holder.update(remap)
        if remap:
            archive_path.parent.mkdir(parents=True, exist_ok=True)
            _write_json(patched, archive_path)
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    if not remap_holder:
        typer.echo("No colliding archive ids found.")
        return
    typer.echo(f"Reminted {len(remap_holder)} archive id(s):")
    for old, new in sorted(remap_holder.items()):
        typer.echo(f"  {old} -> {new}")

    try:
        from fno.events import _build, append_event
        from fno.paths import state_dir

        # Its own event type, not a zeroed graph_archive_swept: a repair run
        # recorded as a sweep with age gate 0 corrupts both "did the daily
        # sweep run" and per-gate hold statistics - the structured channel
        # lying the same way the bare "ok" stdout did.
        event = _build(
            "graph_archive_ids_reminted",
            "backlog",
            {"remint_count": len(remap_holder), "remap": remap_holder},
        )
        append_event(event, state_dir() / "events.jsonl")
    except Exception:  # noqa: BLE001 - the repair itself must not fail on a bad event write
        pass


@cli.command(
    "album",
    hidden=True,
    epilog="Read-only browse over graph-archive.json: the memento book of "
    "shipped work. A card with no gift says so - 43% of archived done nodes "
    "carry no PR, and a gap in the record is itself record. `fno backlog get "
    "<id>` still resolves one archived node by id; `fno backlog unarchive "
    "<id>` brings one back into the working graph.",
)
def cmd_album(
    limit: int = typer.Option(20, "--limit", help="Cards per page."),
    offset: int = typer.Option(0, "--offset", help="Skip the first N cards."),
    project: Optional[str] = typer.Option(None, "--project", help="Filter to one project."),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the page as a JSON card array."
    ),
) -> None:
    """Browse shipped work: done nodes from the archive, newest first.

    Every archive reader before this was a fallback on a lookup miss - given
    an id you could retrieve one archived node, but nothing let you page
    through what shipped. This is that one read verb, and it adds nothing to
    the archive's shape: the sweep already writes every field a card shows.
    Done nodes only - the album is merged work, and superseded entries (the
    214 in the archive against 1741 done) are not ships. Card fields: title,
    id, completed_at, and pr_url present only when one was recorded.
    """
    from fno.tracker import active_backend_name

    # The album renders the local archive's shipped work: guarded local-store
    # display, refused (named) under an external selection.
    if active_backend_name() != "graph":
        typer.echo("fno backlog album: the album renders the local archive; unavailable under an external tracker backend", err=True)
        raise typer.Exit(code=2)

    from fno.graph.store import read_graph

    archive_path = _archive_path()
    entries = (
        [
            e
            for e in read_graph(archive_path)
            if isinstance(e, dict)
            # Terminal facts, not the persisted field: legacy archived rows
            # predate status stamping and carry only completed_at, which the
            # archive subsystem itself treats as done (archive._is_done).
            and (
                str(e.get("status") or "") == "done"
                or bool(e.get("completed_at"))
            )
            # Superseded is derived from superseded_by (graph/types.py), so a
            # row can carry both; the album shows shipped work only.
            and not e.get("superseded_by")
        ]
        if archive_path.exists()
        else []
    )
    if project:
        entries = [e for e in entries if e.get("project") == project]

    def _sort_key(e: dict) -> str:
        return e.get("completed_at") or e.get("updated") or e.get("created_at") or ""

    entries.sort(key=_sort_key, reverse=True)
    page = entries[max(offset, 0) : max(offset, 0) + max(limit, 0)]

    if json_output:
        cards = []
        for e in page:
            card = {
                "id": e.get("id"),
                "title": e.get("title"),
                "completed_at": e.get("completed_at"),
            }
            # Same honesty rule as the text mode: a url-less pr_number is a
            # real gift (reconcile records the pair independently), so the
            # machine surface must not drop it when the url is absent.
            if e.get("pr_number"):
                card["pr_number"] = e["pr_number"]
            if e.get("pr_url"):
                card["pr_url"] = e["pr_url"]
            cards.append(card)
        typer.echo(json.dumps(cards, indent=2))
        return

    if not entries:
        typer.echo("The album is empty.")
        return

    if not page:
        # An offset past the last card is out of range, not an inverted range;
        # an empty page at a valid offset is the zero-width limit, not the offset.
        if max(offset, 0) >= len(entries):
            typer.echo(f"album: {len(entries)} shipped, offset {max(offset, 0)} is past the end")
        else:
            typer.echo(f"album: {len(entries)} shipped, --limit {limit} shows nothing")
        return

    typer.echo(
        f"album: {len(entries)} shipped, showing {max(offset, 0) + 1}-{max(offset, 0) + len(page)}"
    )
    for e in page:
        ts = _sort_key(e)
        date = ts[:10] if ts else "?"
        title = e.get("title") or e.get("slug") or e.get("id")
        if e.get("pr_number"):
            gift = f"PR #{e['pr_number']}"
        elif e.get("pr_url"):
            gift = f"PR {str(e['pr_url']).rstrip('/').rsplit('/', 1)[-1]}"
        else:
            gift = "no gift"
        typer.echo(f"{date}  {e.get('id')}  {title}  {gift}")

    remaining = len(entries) - len(page) - max(offset, 0)
    if remaining > 0:
        typer.echo(f"... {remaining} more. Raise --limit or page with --offset.")


@cli.command(
    "unarchive",
    hidden=True,
    epilog="Reverses `archive` for one node. Follow it with `fno backlog reopen "
    "<id> --reason ...` if the node also needs to stop being done.",
)
def cmd_unarchive(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
) -> None:
    """Move one node from graph-archive.json back into the working graph.

    ``archive`` is a bulk hygiene sweep with no way back, so a node swept early
    (or swept correctly and then needed again) could only be recovered by
    hand-editing, which a PreToolUse hook forbids. It is the fourth instance of
    the same shape as the missing ``reopen``, and the audit that produced this
    verb found it in ten minutes.

    Write order mirrors ``archive`` inverted, for its reason: the working graph
    is written FIRST, so a crash between the two writes leaves a duplicate that
    the next sweep dedupes rather than a lost node. Read-through
    (``entries_with_archive``) already tolerates the window.

    Refuses to guess:
        0  moved, or a warning that the node is already in the working graph
        1  the id is in neither the working graph nor the archive
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph._intake import _find_node
    from fno.graph.store import (
        GraphCorruptError,
        _apply_graph_defaults,
        _read_json,
        _write_json,
        locked_mutate_graph,
        read_graph,
    )

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    if _find_node(read_graph(_graph_path()), task_id) is not None:
        typer.echo(f"warning: {task_id} is already in the working graph", err=True)
        return

    archive_path = _archive_path()
    if not archive_path.exists():
        typer.echo(
            f"Error: {task_id} is in neither the working graph nor {archive_path}",
            err=True,
        )
        raise typer.Exit(code=1)

    # TWO locked passes, and the split is the whole safety argument.
    #
    # `archive` writes the archive inside its mutator because archive-FIRST is
    # safe for it: a crash leaves a duplicate. Inverting the verb inverts the
    # safe order, and the mutator cannot express it - `locked_mutate_graph`
    # persists the returned entries only AFTER the mutator returns, so an
    # archive shrink written inside the mutator lands BEFORE the working graph
    # and a crash between them loses the node from both files. That is the one
    # outcome neither verb may produce, and doing it there quietly guaranteed
    # the ordering the comment claimed to prevent.
    #
    # So: pass 1 adds the row to the working graph and persists it. Pass 2 takes
    # the lock again, re-reads the archive fresh (never a list read before the
    # first write, which a concurrent `archive --apply` could have grown), and
    # drops the row only after confirming the node is really live. A crash
    # between the passes leaves a duplicate, which read-through resolves working
    # -first and the next sweep dedupes.
    row_box: list[Optional[dict]] = [None]

    def add_to_working(entries):
        try:
            archived = _apply_graph_defaults(_read_json(archive_path))
        except GraphCorruptError:
            typer.echo(f"Error: {archive_path} is corrupt; cannot unarchive", err=True)
            raise typer.Exit(code=1)

        # Fuzzy-resolve, matching the working-graph lookup above and the archive
        # probe `reopen` uses: an exact compare made the short id form fail, and
        # `reopen`'s refusal prints this verb as the remedy.
        row = _find_node(archived, task_id)
        if row is None:
            # Same previous_id fallback cmd_get uses: a reminted archive entry
            # keeps its old id as previous_id, and the operator holding the
            # old id must recover the node, not read a plain miss. Without
            # this, `get` resolved the id one command earlier and `unarchive`
            # - the remedy the dedupe verb names - refused it.
            row = next(
                (
                    e
                    for e in archived
                    if isinstance(e, dict) and e.get("previous_id") == task_id
                ),
                None,
            )
        if row is None:
            typer.echo(
                f"Error: {task_id} is in neither the working graph nor {archive_path}",
                err=True,
            )
            raise typer.Exit(code=1)
        row_box[0] = row
        rid = row.get("id")
        # Idempotent under a race: another unarchive may have landed it already.
        if any(isinstance(e, dict) and e.get("id") == rid for e in entries):
            return entries
        return [*entries, row]

    locked_mutate_graph(_graph_path(), add_to_working)

    resolved = (row_box[0] or {}).get("id") or task_id
    archive_write_error: list[str] = []

    def drop_from_archive(entries):
        # Confirm against the just-persisted working graph, not against the
        # mutator's own return value: if the node is somehow not live, shrinking
        # the archive would delete the only copy.
        if not any(isinstance(e, dict) and e.get("id") == resolved for e in entries):
            archive_write_error.append("node not present in the working graph after the write")
            return entries
        try:
            archived_now = _apply_graph_defaults(_read_json(archive_path))
        except GraphCorruptError:
            archive_write_error.append(f"{archive_path} unreadable")
            return entries
        remaining = [
            e for e in archived_now if not (isinstance(e, dict) and e.get("id") == resolved)
        ]
        if len(remaining) != len(archived_now):
            try:
                _write_json(remaining, archive_path)
            except OSError as exc:
                archive_write_error.append(str(exc))
        return entries

    locked_mutate_graph(_graph_path(), drop_from_archive)

    for exc in archive_write_error:
        typer.echo(
            f"warning: {resolved} is back in the working graph, but the archive copy "
            f"could not be removed ({exc}); the next `archive` sweep dedupes it",
            err=True,
        )

    typer.echo(f"Unarchived {resolved}")


# -- Internal helpers for intake / update (avoid circular imports) --

def _collect_intake_paths_typer(plan_paths: list[str], from_list: Optional[str]) -> list[str]:
    """Build the path list for intake from positional args + --from."""
    paths: list[str] = []
    if from_list:
        if from_list == "-":
            import sys
            raw = sys.stdin.read()
        else:
            try:
                raw = Path(from_list).read_text()
            except OSError as e:
                typer.echo(f"Error: --from {from_list}: {e}", err=True)
                raise typer.Exit(code=1)
        for line in raw.splitlines():
            s = line.strip()
            if not s or s.startswith("#"):
                continue
            paths.append(s)
    for p in plan_paths or []:
        if "," in p and not os.path.exists(p):
            for part in p.split(","):
                part = part.strip()
                if part:
                    paths.append(part)
        else:
            paths.append(p)
    return paths


def _do_intake_multi(args, all_paths: list[str], *, roadmap_id, dry_run) -> None:
    """Multi-path intake flow delegating to intake helpers."""
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import (
        _prepare_intake, _build_intake_node, _validate_cli_deps,
    )
    from fno.graph.depends import _derive_title

    cli_deps: list[str] = (
        [d.strip() for d in args.deps.split(",") if d.strip()] if args.deps else []
    )
    _refuse_create_on_external_backend()
    _validate_cli_deps(cli_deps, read_graph(_graph_path()))

    resolved: list[dict] = []
    for raw in all_paths:
        if not os.path.exists(raw):
            resolved.append({"path": raw, "files": [], "status": "missing"})
            continue
        resolved.append({"path": raw, "files": [raw], "status": "ready"})

    concrete_files = [f for r in resolved if r["status"] == "ready" for f in r["files"]]
    if not concrete_files:
        for r in resolved:
            if r["status"] == "missing":
                typer.echo(f"warning: not found, skipped: {r['path']}", err=True)
        typer.echo(
            f"Error: nothing to intake (0 of {len(all_paths)} paths resolved)",
            err=True,
        )
        raise typer.Exit(code=4)

    preview_entries = read_graph(_graph_path())
    if roadmap_id and not args.force_new_roadmap:
        has_roadmap = any(e.get("roadmap_id") == roadmap_id for e in preview_entries)
        if not has_roadmap:
            typer.echo(
                f"unknown roadmap_id: {roadmap_id} "
                "(use /megawalk vision.md to create a roadmap first, "
                "pass --force-new-roadmap, or omit --roadmap-id to intake to the backlog)",
                err=True,
            )
            raise typer.Exit(code=2)

    if dry_run:
        typer.echo(f"Multi-intake preview (dry-run, no changes): {len(all_paths)} paths:")
        for r in resolved:
            if r["status"] == "missing":
                typer.echo(f"  warning: not found, skipped: {r['path']}")
                continue
            for f in r["files"]:
                t = _derive_title(Path(f), args.title) if os.path.isfile(f) else os.path.basename(f.rstrip(os.sep))
                typer.echo(f'  would intake: "{t}"  (plan: {f})')
        typer.echo(f"{len(concrete_files)} plans would be intaked. Run without --dry-run to apply.")
        return

    typer.echo(f"Multi-intake {len(concrete_files)} plans:")
    tallies = {"intaked": 0, "already": 0}
    cli_project = getattr(args, "project", None)
    landed_projects: set[str] = set()
    new_ids: list[str] = []

    def mutator(es):
        for r in resolved:
            if r["status"] != "ready":
                typer.echo(f"  warning: not found, skipped: {r['path']}")
                continue
            for f in r["files"]:
                prep = _prepare_intake(
                    f, es,
                    roadmap_id=roadmap_id, cli_title=args.title,
                    cli_priority=args.priority, cli_deps=cli_deps,
                    cli_points=args.points,
                    cli_project=cli_project,
                )
                if prep["status"] == "already":
                    tallies["already"] += 1
                    typer.echo(f'  already intaked {prep["id"]}: "{prep["title"]}"  ({f})')
                    continue
                node = _build_intake_node(prep["node_spec"], es)
                es.append(node)
                tallies["intaked"] += 1
                new_ids.append(node["id"])
                typer.echo(f'  intake {node["id"]}: "{node["title"]}"  ({f})')
                if isinstance(node.get("project"), str):
                    landed_projects.add(node["project"])
        return es

    locked_mutate_graph(_graph_path(), mutator)

    # Filing-time dedup net (plan x-6ac7): per just-born node, warn if it
    # resembles an existing one. Fresh post-write read; non-fatal.
    try:
        from fno.graph._intake import _find_node, _warn_similar_nodes
        post_entries = read_graph(_graph_path())
        for nid in new_ids:
            node = _find_node(post_entries, nid)
            if node is not None:
                _warn_similar_nodes(node, post_entries, intake_hint=True)
    except Exception as e:  # noqa: BLE001 - dedup never breaks the batch
        _safe_stderr_warn(f"warning: post-intake dedup check skipped: {e}\n")

    from fno.graph._intake import _warn_unknown_project, _list_known_projects
    known = _list_known_projects()
    for proj in sorted(landed_projects):
        _warn_unknown_project(proj, known=known)

    missing = sum(1 for r in resolved if r["status"] == "missing")
    typer.echo(
        f'\n{tallies["intaked"]} newly intaked, '
        f'{tallies["already"]} already intaked, '
        f'{missing} skipped.'
    )
    if tallies["intaked"] + tallies["already"] == 0:
        raise typer.Exit(code=4)


# -- find --


@cli.command("find")
def cmd_find(
    query: str = typer.Argument(..., help="ab-id / id-prefix / slug / bare-hex / free-text description"),
    domain: Optional[str] = typer.Option(None, "--domain", "-d", help="Filter by domain"),
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Filter by project"),
    status: Optional[str] = typer.Option(None, "--status", "-s", help="Filter by status"),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON array"),
) -> None:
    """Search graph entries: exact id/slug/bare-hex, else high-recall over title+slug+details.

    The describe-it candidate generator (ab-f82e8083): a free-text query matches
    across title, slug, AND details so the model has the recall it needs to rank
    a fuzzy description. An ``ab-`` query keeps the existing id/prefix resolution
    (resolve_id) byte-for-byte so `done`/intake callers are unaffected.
    """
    from fno.graph.fuzzy import resolve_id, resolve_node, search_entries
    from fno.graph.slug import format_handle
    from fno.graph.store import read_graph

    entries = _display_entries("find")
    q = (query or "").strip()

    def _resolve_against(pool: list[dict]) -> list[dict]:
        # Exact resolution first (id / slug / bare-hex). Trying resolve_node
        # BEFORE the ab- prefix branch is deliberate: a title can slugify to an
        # `ab-`-led slug (e.g. "AB test cleanup" -> `ab-test-cleanup`), which
        # resolve_id would reject as a malformed id; the exact-slug tier catches
        # it so `find` and `get` resolve the same slug (codex P2).
        node = resolve_node(query, pool)
        if node.kind == "exact":
            return list(node.candidates)
        if q.startswith("ab-"):
            # Canonical id / id-prefix path - unchanged (resolve_id owns it).
            match = resolve_id(query, pool)
            if match.kind == "ambiguous":
                return list(match.candidates)
            if match.kind in {"exact", "fuzzy", "branch_derived"}:
                return [e for e in pool if e.get("id") == match.id]
            return []
        # High-recall describe-it search over title+slug+details.
        return search_entries(query, pool, fields=("title", "slug", "details"))

    def _passes_filters(e: dict) -> bool:
        if domain is not None and e.get("domain") != domain:
            return False
        if project is not None and e.get("project") != project:
            return False
        if status is not None and e.get("status") != status:
            return False
        return True

    matched = [e for e in _resolve_against(entries) if _passes_filters(e)]

    # Read-through fallback to the archive: a node the sweep drained out of the
    # working graph must still surface here, or archiving done nodes silently
    # destroys the dedup recall `/think` + `/blueprint` depend on. Mirrors
    # `backlog get`'s fallback: working graph first, archive read lazily only on
    # a miss, results stamped `_archived`. A corrupt/absent archive is a miss,
    # never a crash (design "Errors").
    if not matched:
        from fno.paths import graph_archive_json
        from fno.tracker import active_backend_name

        # The archive is default-backend storage; no read-through behind an
        # external selection (stale local rows are the leak the seam closes).
        archive_path = (
            graph_archive_json()
            if active_backend_name() == "graph"
            else None
        )
        if archive_path is not None and archive_path.exists():
            # Guard the whole read + resolve + filter: a corrupt archive OR a
            # malformed archived entry must degrade to a miss, never propagate a
            # crash to the caller (design "Errors").
            try:
                archived = read_graph(archive_path)
                hits = [
                    {**e, "_archived": True}
                    for e in _resolve_against(archived)
                    if _passes_filters(e)
                ]
            except Exception:
                hits = []
            matched.extend(hits)

    if not matched:
        typer.echo(f"fno backlog find: no matches for {query!r}", err=True)
        raise typer.Exit(code=1)

    if json_output:
        typer.echo(json.dumps(matched, indent=2))
        return

    for e in matched:
        # Lead with the slug-forward handle (`slug (ab-id)`, or `(ab-id)` when
        # unslugged); the canonical hex stays present and copyable (ab-f82e8083).
        typer.echo(
            "\t".join([
                format_handle(e),
                e.get("status", "?"),
                e.get("domain", "?"),
                e.get("project", "-") or "-",
                e.get("title", ""),
            ])
        )


# -- new --


@cli.command(
    "new",
    hidden=True,
    epilog="Paired verb: `fno backlog remove <id>` deletes it.",
)
def cmd_new(
    title: str = typer.Argument(..., help="Title of the new entry"),
    domain: str = typer.Option("code", "--domain", help="Domain (fuzzy-suggested against history)"),
    project: Optional[str] = typer.Option(
        None, "--project",
        help="Project name. Defaults to current git repo's basename; pass --unscoped to skip auto-scope.",
    ),
    priority: str = typer.Option("p2", "--priority", help="p0|p1|p2|p3"),
    unscoped: bool = typer.Option(
        False, "--unscoped",
        help="Create with project=null and cwd=null. Default auto-scopes to current git repo.",
    ),
    force_domain: bool = typer.Option(
        False, "--force-domain",
        help="Skip the fuzzy domain suggestion and use --domain verbatim.",
    ),
    source_kind: str = typer.Option(
        "organic", "--source-kind",
        help="organic|from_inbox|from_observation|from_supervisor",
    ),
    source_project: Optional[str] = typer.Option(None, "--source-project", help="Source project name"),
    source_session_id: Optional[str] = typer.Option(None, "--source-session-id", help="Source session ID"),
    source_inbox_msg: Optional[str] = typer.Option(None, "--source-inbox-msg", help="Source inbox message ID"),
) -> None:
    """Create a new graph entry without a plan file.

    Auto-scopes project and cwd from the current git repo by default. Pass
    --unscoped to opt out (e.g. for cross-project ideas with no clear home).
    --project always overrides the auto-detected name when both are present.
    """
    _refuse_create_on_external_backend()
    from fno.graph._constants import PRIORITY_ORDER, mint_node_id
    from fno.graph.fuzzy import suggest_domain
    from fno.graph.store import read_graph, locked_mutate_graph

    _VALID_SOURCE_KINDS = {"organic", "from_inbox", "from_observation", "from_supervisor"}
    if source_kind not in _VALID_SOURCE_KINDS:
        typer.echo(
            f"Error: invalid --source-kind '{source_kind}'. "
            f"Must be one of: {', '.join(sorted(_VALID_SOURCE_KINDS))}",
            err=True,
        )
        raise typer.Exit(code=1)

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    entries = read_graph(_graph_path())

    if not force_domain:
        sugg = suggest_domain(domain, entries)
        if sugg.confidence == "fuzzy" and sugg.match != domain:
            typer.echo(
                f"fno backlog new: did you mean --domain {sugg.match}? "
                f"Pass --domain {sugg.match} or add --force-domain to keep {domain!r}.",
                err=True,
            )
            raise typer.Exit(code=2)
        # 'exact' and 'new' pass through silently.

    # Auto-scope from current git repo unless --unscoped is set. --project
    # always overrides the auto-detected basename. Skipping the auto-scope
    # gives us back the pre-fix behavior for the rare global-idea case.
    #
    # Uses the shared resolve_git_roots() helper: linked worktrees record the
    # canonical main checkout as cwd (a durable node outlives its worktree)
    # while keeping the canonical repo basename as project (so all worktrees
    # of the same repo share one project name).
    #
    # When --project is explicit, derive cwd from the work-map first regardless
    # of --unscoped. An explicit project is a stronger signal than the
    # auto-scope default.
    resolved_project = project
    resolved_cwd: Optional[str] = None
    if project is not None:
        from fno.graph._intake import project_root_from_settings
        resolved_cwd = project_root_from_settings(project)
        # resolved_project stays as-is (the explicit flag value)
    if resolved_cwd is None and not unscoped:
        from fno.graph._intake import resolve_git_roots
        derived_name, canonical_root = resolve_git_roots()
        if canonical_root:
            resolved_cwd = canonical_root
            if resolved_project is None:
                resolved_project = derived_name

    new_id_holder: list[Optional[str]] = [None]

    def mutator(es: list[dict]) -> list[dict]:
        live_ids = {e.get("id") for e in es}
        new_id = mint_node_id(live_ids)
        new_id_holder[0] = new_id
        # This verb builds its node inline rather than through
        # _build_backlog_node, so ambient origin capture has to be wired
        # explicitly or `new` stays the one creation path outside the contract.
        prov = _session_provenance(known_ids=live_ids)
        node = {
            "id": new_id,
            "parent": None,
            "title": title,
            "type": "feature",
            "project": resolved_project,
            "cwd": resolved_cwd,
            "priority": priority,
            "domain": domain,
            "blocked_by": [],
            "session_id": None,
            "claimed_at": None,
            "completed_at": None,
            "has_brief": False,
            "roadmap_id": None,
            "vision_path": None,
            "details": None,
            "size": None,
            "batch": None,
            "cost_usd": None,
            "cost_sessions": [],
            "plan_path": None,
            "pr_number": None,
            "pr_url": None,
            "merge_status": None,
            "artifact_url": None,
            "completion_note": None,
            "source": "fno-new",
            "created_at": datetime.now(timezone.utc).isoformat(),
            "source_kind": source_kind,
            "source_project": source_project,
            "source_session_id": source_session_id or prov["source_session_id"],
            "source_harness": prov["source_harness"],
            "source_cwd": prov["source_cwd"],
            "source_node_id": prov["source_node_id"],
            "source_plan_path": prov["source_plan_path"],
            "source_inbox_msg": source_inbox_msg,
        }
        es.append(node)
        return es

    locked_mutate_graph(_graph_path(), mutator)

    # Filing-time dedup net (plan x-6ac7): `fno backlog new` is a reachable plan-less
    # birth path with its own mutator, so it gets the same post-write warn as
    # idea/add/intake (codex P2). Non-fatal; this verb's stdout is the bare id,
    # not JSON, so the stderr receipt cannot corrupt a machine-readable payload.
    if new_id_holder[0] is not None:
        try:
            from fno.graph._intake import _find_node, _warn_similar_nodes

            post_entries = read_graph(_graph_path())
            node = _find_node(post_entries, new_id_holder[0] or "")
            if node is not None:
                _warn_similar_nodes(node, post_entries, intake_hint=False)
        except Exception as e:  # noqa: BLE001 - dedup never breaks a filing
            _safe_stderr_warn(f"warning: post-file dedup check skipped: {e}\n")

    typer.echo(new_id_holder[0])


# -- rehash --

@cli.command("rehash", hidden=True)
def cmd_rehash(
    revert: bool = typer.Option(
        False,
        "--revert",
        help="Restore graph.json from the latest backup instead of rehashing.",
    ),
) -> None:
    """Acknowledge an external edit to graph.json by rehashing the sidecar (default).

    With --revert: locate the most recent graph.json.bak.* backup and restore it,
    then update the sidecar to match.
    """
    import hashlib
    import tempfile

    path = _graph_path()

    if revert:
        # Find the most-recent .bak.* file
        backups = sorted(path.parent.glob(f"{path.name}.bak.*"))
        if not backups:
            typer.echo(
                f"No backups found for {path}. Cannot revert.", err=True
            )
            raise typer.Exit(code=1)
        latest_backup = backups[-1]
        # Atomic restore: temp + rename
        tmp_fd, tmp_path_str = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
        try:
            with os.fdopen(tmp_fd, "wb") as f:
                f.write(latest_backup.read_bytes())
            os.replace(tmp_path_str, str(path))
        except Exception:
            Path(tmp_path_str).unlink(missing_ok=True)
            raise
        typer.echo(f"Reverted graph.json from {latest_backup.name}")

    # Rehash sidecar to match current (possibly just-restored) content
    if not path.exists():
        typer.echo(f"graph.json not found at {path}", err=True)
        raise typer.Exit(code=1)

    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    sidecar = Path(str(path) + ".sha256")
    tmp_fd2, tmp_path2 = tempfile.mkstemp(dir=path.parent, suffix=".sha256.tmp")
    try:
        with os.fdopen(tmp_fd2, "w") as f:
            f.write(digest + "\n")
        os.replace(tmp_path2, str(sidecar))
    except Exception:
        Path(tmp_path2).unlink(missing_ok=True)
        raise
    typer.echo(f"Reconciled to hash {digest[:8]}")


# ---------------------------------------------------------------------------
# collisions sub-app: file-overlap detection between plans
# ---------------------------------------------------------------------------

collisions_app = typer.Typer(
    name="collisions",
    help="Plan collision queries (file-overlap detection)",
    no_args_is_help=True,
)


@collisions_app.command("check")
def cmd_collisions_check(
    plan_path: Path = typer.Argument(..., help="Plan file or folder to check"),
    self_id: Optional[str] = typer.Option(
        None, "--self-id", help="Skip this node ID when comparing (excludes self-collision)",
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit structured JSON instead of human text"),
) -> None:
    """Check a plan against all pending nodes for file collisions.

    Severity thresholds resolve from project then user ``settings.yaml`` and
    fall back to v1 defaults. Recommended actions are inferred deterministically
    from set relationships and plan ages.
    """
    from dataclasses import asdict

    from fno.graph.collision import find_collisions, has_file_surface

    # A plan-vs-graph collision check is local-store machinery: guarded read,
    # refused (named) under an external selection.
    entries = _display_entries("collisions.check")
    evaluated = has_file_surface(plan_path)
    collisions = find_collisions(plan_path, entries, self_id=self_id) if evaluated else []

    if json_output:
        # Drop the private _other_created_at field from JSON output.
        payload = []
        for c in collisions:
            d = asdict(c)
            d.pop("_other_created_at", None)
            payload.append(d)
        typer.echo(json.dumps({
            "status": "ok" if evaluated else "unevaluated",
            "collisions": payload,
        }, indent=2))
        return

    if not evaluated:
        typer.echo(
            f"UNEVALUATED: {plan_path} states no file surface, so nothing was "
            "compared. Add a '## Files to Modify' or '## File Ownership Map' table.",
            err=True,
        )
        return

    if not collisions:
        typer.echo(f"No collisions found for {plan_path}")
        return

    for c in collisions:
        typer.echo(f"[{c.severity.upper()}] {c.with_node_id} ({c.with_node_title})")
        typer.echo(f"  shared: {', '.join(c.shared_files)}")
        typer.echo(f"  recommended: {c.recommended_action}")
        typer.echo(f"  rationale: {c.rationale}")
        typer.echo("")


cli.add_typer(collisions_app, name="collisions", hidden=True)


# ---------------------------------------------------------------------------
# supersede: record a proposed replacement until its merged PR proves coverage
# ---------------------------------------------------------------------------


_SUPERSEDE_EXAMPLE = (
    'fno backlog supersede <new> --replaces <old> '
    '--cause "<what the old node was for>" --surface <path/it/owned>'
)


@cli.command("supersede", hidden=True)
def cmd_supersede(
    new_id: str = typer.Argument(..., help="The new node ID that replaces the old"),
    replaces: str = typer.Option(..., "--replaces", help="The old node ID being superseded"),
    cause: Optional[str] = typer.Option(
        None, "--cause", help="REQUIRED. Inherited cause being replaced"
    ),
    surface: list[str] = typer.Option(
        [], "--surface", help="REQUIRED, repeatable. Repo-relative cause surface"
    ),
    reason: Optional[str] = typer.Option(None, "--reason", "-R", help="Optional human rationale"),
    force: bool = typer.Option(
        False, "--force", "-F", help="Supersede even if the target still has live children (orphaning them)"
    ),
) -> None:
    """Record that ``new_id`` proposes to replace ``replaces``.

    Sets the compatibility edge plus a pending structured evidence record on
    the old node. The old row stays active until a merged PR covers every
    declared surface. Refuses if ``replaces`` still has live children unless
    ``--force`` is given; under ``--force`` the live children's ``parent`` is
    cleared so they stay dispatchable instead of stranding under a dead unit.
    Reverse with ``unsupersede``.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(new_id):
        typer.echo(f"Error: new_id must be a <prefix>-<4..8 hex> node id, got '{new_id}'", err=True)
        raise typer.Exit(code=1)
    if not has_node_id_prefix(replaces):
        typer.echo(f"Error: --replaces must be a <prefix>-<4..8 hex> node id, got '{replaces}'", err=True)
        raise typer.Exit(code=1)
    if new_id == replaces:
        typer.echo("Error: cannot supersede self", err=True)
        raise typer.Exit(code=1)

    cleaned_cause = (cause or "").strip()
    if not cleaned_cause:
        typer.echo(
            "Error: --cause is required and cannot be blank.\n"
            "A supersede now carries the evidence that closes it: what the old\n"
            "node was for, and which repo paths must change to prove the new one\n"
            "replaced it. The old node stays open until a merged PR covers every\n"
            "declared surface.\n"
            f"  {_SUPERSEDE_EXAMPLE}",
            err=True,
        )
        raise typer.Exit(code=1)
    if not surface:
        typer.echo(
            "Error: at least one --surface is required.\n"
            "Name the repo-relative paths the old node owned; a merged PR\n"
            "touching all of them is what verifies this supersede.\n"
            f"  {_SUPERSEDE_EXAMPLE}",
            err=True,
        )
        raise typer.Exit(code=1)
    normalized_surfaces: list[str] = []
    for raw_surface in surface:
        candidate = str(raw_surface).strip().replace("\\", "/")
        parts = candidate.split("/")
        if not candidate or candidate.startswith("/") or any(part in ("", ".", "..") for part in parts):
            typer.echo(
                f"Error: --surface must be a non-empty repo-relative path: {raw_surface!r}",
                err=True,
            )
            raise typer.Exit(code=1)
        if candidate not in normalized_surfaces:
            normalized_surfaces.append(candidate)
    _freed_box: list[list] = [[]]
    _parent_freed_box: list[list] = [[]]
    _proj_kids_box: list[list] = [[]]
    def mutator(entries):
        new_node = _find_node(entries, new_id)
        old_node = _find_node(entries, replaces)
        if new_node is None:
            typer.echo(f"Error: new node {new_id} not found", err=True)
            raise typer.Exit(code=1)
        if old_node is None:
            typer.echo(f"Error: old node {replaces} not found", err=True)
            raise typer.Exit(code=1)

        # Refuse to supersede a shipped or already-superseded node. Without
        # this guard, the mutation would silently clear the old node's
        # completed_at to let the precedence cascade flip to superseded -
        # erasing the ship timestamp on a shipped plan and destroying
        # forensic history. Use a follow-up node instead.
        if old_node.get("completed_at") or old_node.get("status") == "done":
            typer.echo(
                f"Error: cannot supersede {replaces}: it is already shipped "
                f"(status=done). Open a follow-up node instead.",
                err=True,
            )
            raise typer.Exit(code=1)
        if old_node.get("superseded_by"):
            typer.echo(
                f"Error: cannot supersede {replaces}: it is already superseded "
                f"by {old_node['superseded_by']}. Resolve the existing supersede chain first.",
                err=True,
            )
            raise typer.Exit(code=1)
        if old_node.get("deferred_at"):
            # A pre-existing deferral is an independent park supersede would
            # overwrite, and unsupersede cannot tell that park from its own, so
            # the deferral would be lost on reversal. Resolve it first - same
            # shape as the done/already-superseded refusals above.
            typer.echo(
                f"Error: cannot supersede {replaces}: it is deferred. Undefer it "
                f"first (`fno backlog undefer {replaces}`); superseding would "
                f"overwrite that pause and an unsupersede could not restore it.",
                err=True,
            )
            raise typer.Exit(code=1)

        # Guard the death transition: a unit with live children cannot
        # be killed without orphaning them, and orphaning is a deliberate act.
        # Refuse unless forced, naming the children so the operator sees what
        # would strand under a dead unit. Gates on liveness, not type - the
        # `type` field is not maintained reliably enough to guard on (the epic
        # that prompted this was itself typed `feature`).
        # Use the canonical resolved id, not the raw `replaces` argument: an
        # abbreviated id (ab-9728) resolves via _find_node but would never
        # equal a child's full canonical `parent`, silently bypassing the guard.
        live_kids = _live_child_ids(entries, old_node.get("id"))
        if live_kids and not force:
            typer.echo(
                f"Error: cannot supersede {replaces}: it still has "
                f"{len(live_kids)} live child(ren): {', '.join(live_kids)}. "
                f"Superseding would strand them under a dead unit. Re-run with "
                f"--force to supersede anyway (their parent is cleared so they "
                f"stay dispatchable).",
                err=True,
            )
            raise typer.Exit(code=1)

        # Store the canonical id, not the raw (possibly abbreviated) --replaces:
        # unsupersede removes the backref by canonical id, so a stored
        # abbreviation would survive the reverse as a stale forward edge.
        canonical_old = old_node["id"]
        supersedes = list(new_node.get("supersedes") or [])
        if canonical_old not in supersedes:
            supersedes.append(canonical_old)
        new_node["supersedes"] = supersedes
        # Canonical replacement id, not the raw (possibly abbreviated) new_id:
        # an abbreviation stored here can later turn ambiguous and make the
        # replacer unresolvable on unsupersede, leaving a stale edge.
        canonical_new = new_node["id"]
        old_node["superseded_by"] = canonical_new
        old_node["locked_by"] = None
        old_node["claimed_at"] = None
        old_node["supersession"] = {
            "successor": canonical_new,
            "cause": cleaned_cause,
            # Kept, not dropped. --reason is accepted and documented as the
            # human rationale, so discarding it silently lost the one field the
            # operator wrote by hand.
            "reason": (reason or "").strip() or None,
            "surfaces": normalized_surfaces,
            "verified_at": None,
            "evidence_pr": None,
            "matched_surfaces": [],
        }
        old_node["deferred_at"] = None
        old_node["deferred_reason"] = None
        # Release anything that was shipping inside it (x-e957, sigma). Same
        # trap `cmd_remove` was fixed for, one step short of deletion: a
        # superseded unit will never merge, so `_strandable_contained_ids`
        # (which keys on completed_at) never heals its children, while
        # selection_guards keeps refusing them and the redirect keeps pointing
        # at a node that is not going to ship. Unbuildable, uncloseable, and
        # invisible to every sweep. Un-contained rather than closed: superseding
        # the unit is not a claim that its children shipped.
        _freed_box[0] = _release_contained_children(entries, old_node.get("id"))
        # Release the membership axis too. contained_in covers nodes shipping
        # inside this unit's PR; epic children carry `parent`, a different field.
        # Without this they stay parented to a unit that will never ship, and any
        # one later revived strands under the dead-ancestor selection guard - the
        # same unbuildable, uncloseable, invisible shape the contained release
        # exists to end. Non-done children only: done keeps the link as history.
        parent_freed = _release_parented_children(entries, old_node.get("id"))
        _parent_freed_box[0] = parent_freed
        # Projection targets: freed children that have their OWN plan doc. A
        # child sharing the owner's plan_path (an adopted delivery child) is
        # excluded: the owner is already a target, and re-projecting the one
        # shared doc per child would let the last child's mirrored metadata
        # overwrite the owner's.
        owner_plan = old_node.get("plan_path")
        by_id = {e.get("id"): e for e in entries if isinstance(e, dict)}
        _proj_kids_box[0] = [
            k for k in parent_freed
            if (by_id.get(k, {}).get("plan_path") != owner_plan)
        ]
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"superseded {replaces} with {new_id}")
    _echo_freed(_freed_box[0], replaces)
    if _parent_freed_box[0]:
        typer.echo(
            f"Cleared parent on {len(_parent_freed_box[0])} child(ren) of {replaces} "
            f"(revive-safe; a later undefer/unsupersede cannot strand them): "
            f"{', '.join(_parent_freed_box[0])}"
        )
    # Repaint the orphaned children's OWN plan docs (the converger expands
    # ancestors, not descendants, so naming only old + replacement would leave a
    # member child's parent/parent_slug/wave stale). _proj_kids already excluded
    # any child sharing the owner's plan_path, so this cannot rewrite the
    # owner's doc per child.
    _project_plans_from_graph([replaces, new_id] + _proj_kids_box[0])


# ---------------------------------------------------------------------------
# unsupersede: reverse a supersede (the death transition was reversible only
# by hand-editing graph.json before this; cmd_undefer clears deferred_at but
# leaves superseded_by set, so a superseded node stayed superseded)
# ---------------------------------------------------------------------------


@cli.command("unsupersede", hidden=True)
def cmd_unsupersede(
    node_id: str = typer.Argument(..., help="The superseded node ID to revive"),
) -> None:
    """Reverse a supersede on ``node_id``. Idempotent in the safe direction.

    Clears ``superseded_by``, ``deferred_at``, and ``deferred_reason`` (the
    latter two were set by ``cmd_supersede`` and must go together, else the
    status precedence drops the node from ``superseded`` straight into
    ``deferred``) and removes ``node_id`` from the replacer's ``supersedes``
    list. The status then recomputes to the node's underlying state, and the
    plan doc is forced off terminal ``superseded`` (the forward-only projector
    will not leave a terminal on its own).

    A node that is merely deferred (no ``superseded_by``) is left untouched:
    reactivating parked work is ``undefer``'s job, and clearing a deferral here
    would silently make deferred work dispatchable.

    Reactivation is a separate verb from ``undefer`` on purpose: reviving a
    plan that another plan supplanted is a conscious act. ``recompute_statuses``
    has always named this verb as the only route back from ``superseded`` - it
    just did not exist.

    Does NOT re-contain or re-parent children released when the node was
    superseded: re-adoption is decompose's job, the same policy ``cmd_undefer``
    applies to un-containment.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(node_id):
        typer.echo(
            f"Error: node_id must be a <prefix>-<4..8 hex> node id, got '{node_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    was_superseded_holder: list[bool] = [False]
    canonical_id_box: list[str] = [node_id]

    def mutator(entries):
        node = _find_node(entries, node_id)
        if node is None:
            typer.echo(f"Error: node {node_id} not found", err=True)
            raise typer.Exit(code=1)
        replacer = node.get("superseded_by")
        was_superseded_holder[0] = bool(replacer)
        canonical_id_box[0] = node.get("id", node_id)
        if not replacer:
            # Not superseded: nothing to reverse. Return WITHOUT touching
            # deferred_at, so a node that is merely deferred (not superseded)
            # keeps its park. Clearing it here would reactivate parked work.
            return entries
        # Drop the backref so the replacer's `supersedes` list no longer claims
        # a node it no longer supersedes - a stale claim would make the chain
        # read as live after we just broke it. Compare against the canonical id,
        # not the (possibly abbreviated) argument: the backref stores the full id.
        new_node = _find_node(entries, replacer)
        if new_node is not None:
            new_node["supersedes"] = [
                s for s in (new_node.get("supersedes") or []) if s != node["id"]
            ]
        node["superseded_by"] = None
        node["supersession"] = None
        node["deferred_at"] = None
        node["deferred_reason"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    if not was_superseded_holder[0]:
        typer.echo(f"warning: {node_id} was not superseded", err=True)
    else:
        # Give a revived node the same streak-reset boundary cmd_undefer gives a
        # human-recovered one: a superseded node is often auto-deferred first, so
        # without this the next maintain pass could re-defer it from stale
        # failure history without a single fresh failure. Best-effort, like undefer.
        from fno.graph.failure import emit_undefer_boundary

        emit_undefer_boundary(canonical_id_box[0])
    typer.echo(f"Unsuperseded {node_id}")
    # Force the revived node's plan status off terminal `superseded` (the
    # forward-only projector refuses to leave a terminal): without this the
    # graph is active while the plan doc stays superseded. Scoped to the revived
    # node only, not the ancestors/siblings the converger also repaints.
    cid = canonical_id_box[0]
    _project_plans_from_graph([cid], force_status_off_terminal_for=cid)
    # Recompute+persist after projection. The graph status was derived during
    # the mutation while the plan still read `superseded`, so it must follow the
    # now-corrected plan or `backlog get`/the board keep reporting `ready` and a
    # fail-closed `design` never makes the node non-dispatchable. Always, not
    # only on a fresh reverse: an interrupted earlier unsupersede can leave the
    # plan reading `superseded` after superseded_by is already clear, and this
    # rerun heals that plan through the no-replacer branch too. A no-op mutator
    # still triggers the recompute+write; idempotent on a clean call.
    locked_mutate_graph(_graph_path(), lambda entries: entries)


def _relevant_exec_scope(root: str, by_id: dict) -> set[str]:
    """The node set the execution graph is compiled over, order-independent.

    A fixpoint (not a single pass) so the result never depends on graph.json
    row order: root, its transitive ``blocked_by`` ancestors, the transitive
    dependents that name any in-scope node as a blocker, and the verifier /
    evidence-producer nodes referenced by in-scope nodes (so verification and
    data edges are not silently dropped for lack of a blocked_by link).
    """
    scope: set[str] = set()
    if root in by_id:
        scope.add(root)
        stack = [root]
        while stack:  # up: transitive blocked_by ancestors
            for dep in (by_id[stack.pop()].get("blocked_by") or []):
                if dep in by_id and dep not in scope:
                    scope.add(dep)
                    stack.append(dep)

    changed = True
    while changed:
        changed = False
        for nid, e in by_id.items():
            if nid in scope:
                # verifier + evidence producers referenced by an in-scope node
                v = str(e.get("verifier") or "").strip()
                if v and v in by_id and v not in scope:
                    scope.add(v)
                    changed = True
                for ev in (e.get("requires_evidence") or []):
                    for src, se in by_id.items():
                        if src not in scope and ev in (se.get("produces_evidence") or []):
                            scope.add(src)
                            changed = True
                continue
            # down: a dependent of anything already in scope
            if any(dep in scope for dep in (e.get("blocked_by") or [])):
                scope.add(nid)
                changed = True
    return scope


def _exec_liveness(state: str) -> str:
    """Map a claim_status state to the ExecNode liveness enum."""
    return {"live": "live", "suspect": "unknown", "stale": "unknown",
            "corrupted": "unknown", "free": ""}.get(state, "")


# -- task 4.2: the external-backend verb classification -----------------------
#
# Every registered backlog verb is classified exactly ONCE, here, against the
# LIVE registry (never a frozen count): tracker-owned verbs wrap their
# registered callback with the shared external refusal BEFORE any graph
# read/write, and footnote-owned verbs carry the read-side marker the
# consumer census pins (scripts/diagnostics/tracker-consumers.py --verbs).
# A verb missing from both lists fails the import, and a listed verb the
# registry no longer carries fails it too (no tombstones, no renames smuggled
# past the classification). Misclassifying a read as tracker-owned only
# refuses it externally; misclassifying a mutation as footnote-owned is the
# dangerous direction, so unsure verbs sit tracker-owned.

_TRACKER_OWNED_VERBS = frozenset({
    # node lifecycle + creation
    "add", "idea", "new", "intake", "decompose", "update", "note", "remove",
    "reopen", "supersede", "unsupersede",
    # board/rank/queue state
    "rank", "reprioritize", "defer", "undefer", "queue", "unqueue", "pick",
    "unclaim",
    # storage + sweep machinery
    "archive", "unarchive", "archive-dedupe-ids", "rehash", "maintain",
    "groom",
    # orchestration that stamps nodes
    "advance", "reconcile", "reconcile-findings", "lanes", "lane-fill",
    "dispatch-lanes",
    # footnote-owned DATA with a graph-resident write path (refused until the
    # write moves to the sidecar seam)
    "cost", "session add", "session close", "session reap-open", "decide", "decisions",
    "decide-retract", "decide-reindex",
    # sub-app mutations
    "triage apply", "capture promote",
    "batch join", "batch prepare", "batch ship", "batch ship-closeable",
})

_FOOTNOTE_OWNED_VERBS = frozenset({
    # seam reads / renders
    "get", "status", "view", "find", "next", "ready", "queued", "provenance",
    "roadmap", "bases", "album", "project-root", "board", "undispatched",
    # completion works on any backend by design (task 4.1)
    "done",
    # footnote-owned sidecar files, no graph write
    "relatedness build", "relatedness get", "epic status",
    # capture-pile file machinery (no graph writes; promote is tracker-owned)
    "capture add", "capture archive", "capture capture-pass", "capture dismiss",
    "capture empty-pass", "capture list", "capture scan", "capture tidy",
    # triage read/propose surfaces (apply is tracker-owned)
    "triage consistency", "triage context", "triage health", "triage projects",
    "triage propose", "triage rank", "triage trend", "triage validate",
    # batch read surfaces
    "batch open", "batch status", "batch metrics",
    # graph-store integrity check (read-only)
    "collisions check",
})


def _refuse_tracker_owned_on_external_backend(label: str) -> None:
    """The shared external-backend refusal for a tracker-owned backlog verb.

    One guard at every reachable tracker-owned entry point (the wrapper below
    installs it on the registered callback), firing before any graph read or
    write. The message names the verb and the backend."""
    from fno.tracker import active_backend_name

    backend = active_backend_name()
    if backend != "graph":
        typer.echo(
            f"fno backlog {label}: this verb owns graph state; under the "
            f"{backend} tracker backend it is refused. Track the item in the "
            f"tracker by its id.",
            err=True,
        )
        raise typer.Exit(code=1)


def iter_backlog_registry():
    """The (group-label, typer-app) pairs carrying every backlog verb.

    The ONE structural list: the verb classifier below, the consumer census
    (scripts/diagnostics/tracker-consumers.py), and the classification tests
    all walk it, so a new sub-app is registered exactly here, beside its
    add_typer call - never re-copied into an instrument that would then
    certify a registry it never saw.
    """
    return [
        (None, cli),
        ("triage", _triage_cli),
        ("capture", _capture_cli),
        ("batch", _batch_cli),
        ("relatedness", _relatedness_cli),
        ("epic", _epic_cli),
        ("session", session_app),
        ("collisions", collisions_app),
    ]


def _classify_backlog_verbs() -> None:
    import functools

    apps = iter_backlog_registry()
    seen: set[str] = set()
    for group, app in apps:
        for info in app.registered_commands:
            name = info.name or ""
            label = f"{group} {name}" if group else name
            seen.add(label)
            callback = info.callback
            if callback is None:
                raise RuntimeError(f"backlog verb {label!r} has no callback")
            if label in _TRACKER_OWNED_VERBS:

                @functools.wraps(callback)
                def _guarded(*args, _orig=callback, _label=label, **kwargs):
                    _refuse_tracker_owned_on_external_backend(_label)
                    return _orig(*args, **kwargs)

                setattr(_guarded, "_fno_tracker_owned", True)
                info.callback = _guarded
            elif label in _FOOTNOTE_OWNED_VERBS:
                setattr(callback, "_fno_footnote_owned", True)
            else:
                raise RuntimeError(
                    f"unclassified backlog verb {label!r}: classify it in "
                    "_TRACKER_OWNED_VERBS or _FOOTNOTE_OWNED_VERBS "
                    "(graph/cli.py) so the external-backend census holds"
                )
    unknown = (_TRACKER_OWNED_VERBS | _FOOTNOTE_OWNED_VERBS) - seen
    if unknown:
        raise RuntimeError(
            f"classified verbs missing from the live registry (renamed or "
            f"removed?): {sorted(unknown)}"
        )


_classify_backlog_verbs()
