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
from typing import Any, List, Literal, Optional

import typer

from fno.loops import refuse_if_paused
from fno.tombstones import tombstone_group_cls
# the external-backend verb classification lives beside its data
from fno.graph._verb_classification import (
    _NO_GRAIN_ON_EXTERNAL_BACKEND,
    classify_backlog_verbs,
)
from fno.graph.api import wire_rows  # noqa: F401 - re-export for lazy importers
from fno.graph.node_builder import (  # noqa: F401 - re-export for lazy importers
    DESCRIPTION_HELP,
    ENCOUNTER_EVIDENCE_HELP,
    ORIGIN_EVIDENCE_HELP,
    RELATED_HELP,
    SOURCE_KIND_HELP,
    SOURCE_NODE_HELP,
    TAG_HELP,
    _build_backlog_node,
    _session_provenance,
)
from fno.graph.node_builder import register as _register_node_builder
from fno.graph.api import cmd_version as _cmd_version
# The roster renderer lives in its own module: this file is shrink-only and
# the provenance change touches it. The alias keeps the historical name
# importable.
from fno.graph.provenance_view import lifecycle_roster as _lifecycle_roster
from fno.graph.provenance_view import pr_block, render_pr_line
from fno.provenance.registry_liveness import registry_status_index

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

_register_node_builder(cli)
from fno.graph.worked import cmd_worked as _cmd_worked  # noqa: E402

def _triage_forward(ctx) -> None:
    """The native door owns every triage action; the wheel keeps the route.

    The whole argv rides `fno-agents backlog triage` with stdio inherited
    and its exit code returned, so the front door lists the group and
    serves it on installs whose `fno` resolves to this wheel.
    """
    import subprocess

    from fno import rust_binary

    binary = rust_binary.resolve_binary()
    if binary is None:
        typer.echo(
            "Error: the triage group is served by the native door; no fno-agents binary found.",
            err=True,
        )
        raise typer.Exit(code=2)
    proc = subprocess.run([str(binary), "backlog", "triage", *ctx.args])
    raise typer.Exit(code=proc.returncode)


cli.command("triage", context_settings={"allow_extra_args": True, "ignore_unknown_options": True})(
    _triage_forward
)

cli.command("worked", hidden=True)(_cmd_worked)
cli.command("version", hidden=True)(_cmd_version)


# Nested capture sub-app: `fno backlog capture <verb>`. The capture tier below
# idea nodes (markdown fu-* items, NOT graph nodes). Distinct from
# `fno agents mail`. The retired `inbox` spelling lives in fno.tombstones.
from fno.backlog.capture import cli as _capture_cli  # noqa: E402

cli.add_typer(_capture_cli, name="capture", hidden=True)

# Nested batch sub-app: `fno backlog batch <verb>`. Batch-lane state
# (.fno/batches/<domain>.json) — coalesce same-domain nodes into one PR.
from fno.backlog.batch import cli as _batch_cli  # noqa: E402
from fno.backlog.advance import refuse_unknown_source as _refuse_unknown_source  # noqa: E402

cli.add_typer(_batch_cli, name="batch", hidden=True)

# Decision records are node/PR metadata; their backlog leaves (decide,
# decisions, decide-retract, decide-reindex) are the grouped dispatcher's
# native arms since the decide family ported. No Python mount remains.


# Node-lifecycle sub-apps folded under backlog (unit 6 of the  reorg):
# annotate findings, carve-out records, and the retro harvest are all node
# metadata operations. The old top-level spellings stay one-release shims
# (fno.verb_moves); the mounts below are the canonical homes.
from fno.annotate.cli import annotate_app as _annotate_app  # noqa: E402
from fno.carveout.cli import carveout_app as _carveout_app  # noqa: E402
from fno.retro.cli import retro_app as _retro_app  # noqa: E402

cli.add_typer(_annotate_app, name="annotate", hidden=True)
cli.add_typer(_carveout_app, name="carveout", hidden=True)
cli.add_typer(_retro_app, name="retro", hidden=True)


# Selection-time enforcement (): a node another session is actively
# driving holds a live ``node:<id>`` claim and must be skipped so two sessions
# never pick up the same node. The implementation is homed in graph/statuses.py
# (so the board renderers can share it without a cli<->render cycle); re-exported
# under the original module-global name that existing tests monkeypatch.
from fno.graph.statuses import derived_status, live_claimed_node_ids as _live_claimed_node_ids  # noqa: E402


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
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
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
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    return {
        p
        for e in entries
        if isinstance(e, dict)
        and isinstance((p := e.get("parent")), str)
        and e.get("contained_in") != p
    }


@cli.callback()
def _graph_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag

    merge_json_flag(ctx, json_output)


def _graph_path() -> Path:
    """Return the active graph.json path (monkeypatch-friendly)."""
    from fno.graph._constants import GRAPH_JSON

    return GRAPH_JSON


def _display_entries(reader: str, *, strict: bool = False) -> list[dict]:
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
        return read_entries(reader, strict=strict)
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


# -- relatedness sidecar (`fno backlog relatedness build|get`) --
# A node-to-node relatedness map read by 's offer path and /triage.
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
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Restrict the corpus to this project."
    ),
    judge: bool = typer.Option(
        False, "--judge", help="Haiku pairwise refinement (v2, opt-in); v1 is deterministic-only."
    ),
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
        typer.echo(
            "note: --judge (haiku refinement) not implemented in v1; wrote deterministic map.",
            err=True,
        )
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
    """Print the top related nodes for one node (the  consumer API).

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

    Retained for the decompose surface's adopt guard; the contain verb's
    probe moved to the native leg, which reads the same lockfiles.
    """
    from fno.claims.core import claim_status

    key = f"node:{node_id}"
    try:
        info = claim_status(key)
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
        root = (
            (project_root_from_settings(proj) if proj else None)
            or c.get("_resolved_cwd")
            or c.get("cwd")
        )
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


def _merge_unconfirmed(child: dict) -> bool:
    """True when a done child carries a PR that GitHub never confirmed merged.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    return (
        child.get("status") == "done"
        and bool(child.get("pr_number"))
        and child.get("merge_status") != "merged"
    )


def _verify_merge(child: dict) -> Optional[str]:
    """Ask GitHub what actually happened to a flagged child's PR.

    Only ever called for rows :func:`_merge_unconfirmed` already flagged, so
    the probe is bounded by that set (16 rows fleet-wide on 2026-09-01), never
    by the child count. Scoped to the child's own checkout because the graph is
    cross-project and one global ``--repo`` would mis-scope a sibling's PR.

    Returns the resolved note, or None when the probe could not answer. None is
    the important case: the caller leaves the row FLAGGED. A probe that failed
    is not evidence that the PR merged, and downgrading the row on a network
    blip would turn this instrument into the thing it exists to catch.
    """
    import subprocess

    from fno.graph._reconcile import GH_QUERY_TIMEOUT_S
    from fno.graph._intake import project_root_from_settings

    proj = child.get("project")
    cwd = (
        (project_root_from_settings(proj) if proj else None)
        or child.get("_resolved_cwd")
        or child.get("cwd")
    )
    if not cwd:
        return None
    try:
        proc = subprocess.run(
            ["gh", "pr", "view", str(child["pr_number"]), "--json", "state,mergedAt"],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            timeout=GH_QUERY_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0 or not (proc.stdout or "").strip():
        return None
    try:
        payload = json.loads(proc.stdout)
    except ValueError:
        return None
    if payload.get("mergedAt") or payload.get("state") == "MERGED":
        # Merged, but nothing ever stamped merge_status. The graph was right
        # about the outcome and wrong about the evidence.
        return f"merged (unstamped) #{child['pr_number']}"
    state = payload.get("state") or "UNKNOWN"
    return f"{state} #{child['pr_number']}"


def _child_note(child: dict, events: list[dict], worker: Optional[str]) -> str:
    """The inline note for a child row: streak (deferred), receipt (idle ready),
    merge-unconfirmed (done with an unstamped PR), or ``-``. Never blank for an
    idle ready child."""
    from fno.graph import failure

    node_id = child["id"]
    status = child.get("status")
    if status == "deferred":
        return f"streak {failure.consecutive_failures(node_id, events)}"
    if status == "ready" and not worker:
        return _latest_receipt(node_id, events) or "no receipt found"
    if _merge_unconfirmed(child):
        return f"merge unconfirmed #{child['pr_number']}"
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
    return f"scope growth: {len(growth.follow_up_ids)} follow-ups (origin capture {pct})  |  {cost}"


@_epic_cli.command("status")
def cmd_epic_status(
    ctx: typer.Context,
    epic: str = typer.Argument(..., help="Epic node id or slug."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON."),
    verify_merges: bool = typer.Option(
        False,
        "--verify-merges",
        help=(
            "Ask GitHub what happened to each merge-unconfirmed child's PR "
            "(one `gh pr view` per FLAGGED row only). Off by default: the "
            "table is a local read."
        ),
    ),
) -> None:
    """One table over an epic's children: status, worker, PR, and an inline
    dispatch receipt (or breaker streak) so an idle/deferred child is never a
    silent blank. Refuses a non-container node by name.

    A child reads `done` at finalize, not at merge, so a done child whose PR
    GitHub never confirmed merged is flagged `merge unconfirmed` - the state in
    which the graph lies at a wave gate. `--verify-merges` resolves those rows
    against GitHub; without it the whole table is local and needs no network."""
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
        unconfirmed = _merge_unconfirmed(c)
        note = _child_note(c, events, worker)
        if unconfirmed and verify_merges:
            # A probe that could not answer leaves the row flagged; only a real
            # answer replaces the note.
            note = _verify_merge(c) or note
        rows.append(
            {
                "id": node_id,
                "slug": c.get("slug") or "",
                "project": c.get("project") or "",
                "status": _status_of(c) or "",
                "worker": worker,
                "pr_number": pr,
                "merge_unconfirmed": unconfirmed,
                "receipt": note,
            }
        )

    unconfirmed_total = sum(1 for r in rows if r["merge_unconfirmed"])

    from fno.graph.rollup import scope_growth
    from fno.graph.store import entries_with_archive

    # Read through the archive for the METRIC only (the same read-only fallback
    # `get` uses): without it a swept child stops counting and the number
    # quietly changes with unrelated grooming. The children table stays
    # working-graph only.
    growth = scope_growth(entries_with_archive(entries), epic_id)

    if json_mode(ctx):
        typer.echo(
            json.dumps(
                {
                    "epic": epic_id,
                    "slug": epic_node.get("slug"),
                    "children_total": total,
                    "children_done": done,
                    # Done children whose PR GitHub never confirmed merged. A
                    # subset of children_done, not a separate bucket: the point
                    # is that some of that "done" is unevidenced.
                    "children_merge_unconfirmed": unconfirmed_total,
                    "merges_verified": verify_merges,
                    "children": rows,
                    # follow_ups is reported only when coverage clears the floor; the
                    # coverage block ships regardless so a suppressed figure explains
                    # itself instead of just being absent.
                    "scope_growth": {
                        "follow_ups": len(growth.follow_up_ids or ())
                        if growth.reportable
                        else None,
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
                },
                indent=2,
            )
        )
        return

    header = f"epic: {epic_id} ({epic_node.get('slug') or ''})  {done}/{total} done"
    if unconfirmed_total:
        # Qualifies the done count in the same breath rather than a line below
        # it: "3/5 done" beside an unqualified number is what let the graph
        # read as landed at a wave gate.
        header += f"  ({unconfirmed_total} merge unconfirmed)"
    typer.echo(header)
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


def _stamp_ship_on_pr_link(node_id: str) -> None:
    """Stamp the ship lifecycle row when a node is first PR-linked.

    The PR link is ship's START (the PR is open, awaiting review/merge), so the
    row carries started_at only - no ended_at, since merge is recorded elsewhere
    or not at all. The row records whoever ran the link - a role or an ambient
    session can be that - not the implementer or the merger, and no terminal
    ever closes it, so readers must treat it as a link event, never occupancy.
    ``fno do pr bind-created`` is the second such site and calls this too.
    Best-effort: an unresolvable identity or a graph failure skips with a named
    stderr reason and never fails the update. Idempotent: append_session_record
    collapses a re-stamp of the same (phase, harness, session_id).
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
            _graph_path(),
            node_id,
            phase="ship",
            harness=harness,
            session_id=session_id,
            started_at=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
    except (Exception, SystemExit) as exc:
        typer.echo(
            f"update: ship provenance stamp skipped for {node_id}: {exc}",
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




def _refuse_create_on_external_backend() -> None:
    """Refuse node-creation verbs on a non-default backend.

    On an external backend, item creation lives in the tracker (GitHub Issues,
    Linear, ...); minting an ab- entry in graph.json would create a phantom item
    the tracker has no record of. Called from EVERY Python creation entry
    point (cmd_new, cmd_decompose, cmd_intake, and cmd_tree; add and idea answer
    natively and carry their own guard) so the guard is not decorative. A guard on only some reachable
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


def _prompt_difficulty_value(value: str) -> str:
    """``typer.prompt`` value_proc for the difficulty band: re-ask on a bad
    answer instead of crashing. click re-prompts only on ``UsageError``, while
    ``normalize_difficulty`` raises a bare ``ValueError`` that would surface as
    a traceback straight out of the prompt."""
    import click
    from fno.graph._constants import DIFFICULTY_HELP, normalize_difficulty

    try:
        return normalize_difficulty(value) or ""
    except ValueError as exc:
        raise click.UsageError(f"{exc}. {DIFFICULTY_HELP}") from exc


def _encounter_provenance(harness: str | None) -> dict[str, str]:
    """Model/effort provenance for an encounter record, or nothing.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    provenance: dict[str, str] = {}
    if harness != "claude":
        return provenance
    effort = (os.environ.get("CLAUDE_EFFORT") or "").strip()
    if effort:
        provenance["effort"] = effort
    if (os.environ.get("ANTHROPIC_BASE_URL") or "").strip():
        model = (os.environ.get("ANTHROPIC_MODEL") or "").strip()
        if model:
            provenance["model"] = model
    return provenance


def _validate_priority_or_exit(priority: str, *, blocks_everything: bool = False) -> None:
    """The shared priority gate: PRIORITY_ORDER membership, then the write rule.

    P0 refuses without the blocks-everything acknowledgement; exit 1 is the
    membership miss, exit 2 the write rule."""
    from fno.graph._constants import PRIORITY_ORDER, validate_priority_write

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)
    try:
        validate_priority_write(priority, blocks_everything=blocks_everything)
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)


def _require_node_id(task_id: str) -> None:
    """The shared node-id gate: one refusal text for every verb that takes one."""
    from fno.graph._constants import has_node_id_prefix

    if has_node_id_prefix(task_id):
        return
    typer.echo(
        f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True
    )
    raise typer.Exit(code=1)


def _require_nodes(entries: "list[dict]", ids: "list[str]") -> None:
    """The shared missing-node gate for the multi-id verbs."""
    from fno.graph._intake import _find_node

    missing = [tid for tid in ids if _find_node(entries, tid) is None]
    if missing:
        typer.echo(f"Error: feature(s) not found: {', '.join(missing)}", err=True)
        raise typer.Exit(code=1)


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
            "[,adopt]} specs; '@file' or '-' reads stdin. Per-group project/cwd "
            "route a child into another repo; `adopt` re-parents existing nodes "
            "instead of minting. Full schema: "
            "skills/blueprint/references/epic-decomposition.md."
        ),
    ),
    max_prs: Optional[int] = typer.Option(
        None,
        "--max-prs",
        help=(
            "Ceiling on group/PR count; rejects when exceeded. Default: "
            "config.blueprint.max_prs_per_epic, tightened by the epic doc's "
            "`max_children:` frontmatter."
        ),
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help="Allow a re-decomposition that orphans an already-shipped group child node.",
    ),
    plans: str = typer.Option(
        "separate",
        "--plans",
        help=(
            "Only 'separate': a self-contained quick-plan per child (one plan "
            "== one PR == one node). The removed 'fragment' form is still "
            "recognized on existing children for idempotent re-decompose."
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
    from fno.graph._constants import mint_node_id, validate_priority_write
    from fno.graph.store import commit_rows_via_store, GraphUnreadableError
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
        epic_strategy_from_doc,
        scaffold_separate_plan,
        separate_plan_path,
        validate_groups,
    )
    from fno.graph._contain import contain_into, refuse_dead_owner
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
        epic_node = _find_node(wire_rows(path=_graph_path()), epic_id)
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
                epic_doc_rel = os.path.relpath(epic_doc, epic_node.get("cwd") or os.getcwd())
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
        try:
            validate_priority_write(
                live_epic.get("priority", "p2"),
                blocks_everything=bool(live_epic.get("blocks_everything")),
            )
        except ValueError as exc:
            raise DecomposeError(str(exc), exit_code=2) from exc
        epic_resolved_id = live_epic["id"]
        epic_id_box[0] = epic_resolved_id
        base = plan_base(live_epic.get("plan_path"))
        verbatim_base_box[0] = base  # the relative base, for the source_doc seed
        # `base` (verbatim, possibly relative) is the node-identity key used by
        # child_plan_path below - DO NOT mutate it. For the set-expected
        # shell-out only, resolve a relative base against the epic's project
        # root (its stored cwd) so a decompose run from a subdirectory still
        # locates the doc on disk; the writer resolves relative paths against
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
        # entry as the full id - so `ab-abcd` in one group and `` in
        # another read as two claims there and are one node here.
        adopt_claim: dict[str, str] = {}
        plan_to_group: list[tuple[dict, dict]] = []  # (node, normalized group)
        for grp in norm:
            frag_path = child_plan_path(base, grp["slug"])
            sep_path = separate_plan_path(base, grp["slug"])
            # Tolerant lookup: identity is the durable group_slug (US2), so
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
                # `ready` (US2, Locked Decision 4). group_slug is the durable
                # identity that survives the unlinked window.
                node = _build_backlog_node(
                    title=grp["title"],
                    parent=epic_resolved_id,
                    project=route_proj if route_proj is not None else live_epic.get("project"),
                    cwd=route_cwd if route_cwd is not None else live_epic.get("cwd"),
                    priority=live_epic.get("priority", "p2"),
                    blocks_everything=bool(live_epic.get("blocks_everything")),
                    difficulty=live_epic.get("difficulty"),
                    domain=live_epic.get("domain", "code"),
                    plan_path=None,
                    origin_channel="decompose",
                    origin_evidence=f"parent:{epic_resolved_id}",
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

            # Adoption (US2): re-parent each named node under this group
            # child, inside the same locked mutation. Nothing is minted for it
            # and nothing is deleted; membership rides the `parent` pointer.
            adopted: list[str] = []
            # A dead delivery unit cannot own containment; the guard and its
            # remedy text live in _contain (shared with `fno backlog contain`).
            # Gated on a non-empty adopt list so a dead group child with
            # nothing to adopt still updates its title, waves, and blocked_by.
            if grp["adopt"]:
                refuse_dead_owner(node, context=f"group {grp['slug']!r}")
            for adopt_id in grp["adopt"]:
                target = _find_node(graph_entries, adopt_id)
                if target is None:
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopts {adopt_id}, which resolves to no node",
                        exit_code=3,
                    )
                if target["id"] == epic_resolved_id:
                    # validate_groups checks the RAW epic argument, so an
                    # aliasable `ab-` prefix slips past it and would land on the
                    # generic cycle refusal instead of this specific one.
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopt names the epic {epic_resolved_id} itself",
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
                        whose = (
                            "already this epic's group child on a legacy plan "
                            f"path ({target.get('plan_path')}) that no longer "
                            "matches the epic doc"
                        )
                    else:
                        whose = f"already a group child of another epic ({target.get('plan_path')})"
                    raise DecomposeError(
                        f"group {grp['slug']!r} adopts {target['id']}, which is "
                        f"{whose}; demoting a group into a task "
                        "reshapes the epic",
                        exit_code=2,
                    )
                outcome = contain_into(
                    graph_entries,
                    node,
                    target,
                    live_worker=_live_worker,
                    context=f"group {grp['slug']!r}",
                )
                if outcome.warning:
                    uncontained_box[0].append(
                        f"warning: adopted {target['id']} into group "
                        f"{grp['slug']!r} but did NOT mark it contained: it "
                        f"{outcome.warning}, so it is its own delivery unit. It stays "
                        "separately dispatchable, separately costed, and is not "
                        "closed by the group's merge."
                    )
                if outcome.adopted:
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
            dep, stub_against, cversion, downgrade = classify_group_dep(grp, pinned_versions, base)
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

    # Rationale (11 lines): docs/architecture/graph-cli-rationale.md#cmd-decompose-2220
        _unadopted = [
            e
            for e in graph_entries
            if e.get("id") and e.get("parent") == epic_resolved_id and not is_group_child(e)
        ]
        unadopted_box[0] = [e["id"] for e in _unadopted]
        contained_unadopted_box[0] = [
            e["id"] for e in _unadopted if e.get("contained_in") == epic_resolved_id
        ]
        return graph_entries

    orphan_box: list[list[str]] = [[]]
    unadopted_box: list[list[str]] = [[]]
    contained_unadopted_box: list[list[str]] = [[]]
    # Adoptees that were re-parented but deliberately NOT marked contained
    # (they carry their own PR or cost). Same box-then-emit shape as the
    # unadopted warning: collected inside the locked mutator, printed after.
    uncontained_box: list[list[str]] = [[]]
    base_box: list = [None]
    verbatim_base_box: list = [None]
    epic_cwd_box: list = [None]
    downgrade_box: list[list[str]] = [[]]
    try:
        commit_rows_via_store(_graph_path(), mutator)
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
    from fno.graph import api as graph_api

    try:
        by_id = {
            e.get("id"): e
            for e in (
                n.model_dump(by_alias=True)
                for n in graph_api.nodes(include_archived=True, path=_graph_path()).nodes
            )
        }
    except Exception:  # noqa: BLE001 - never wedge the report on a re-read failure
        by_id = {}

    # Rationale (11 lines): docs/architecture/graph-cli-rationale.md#cmd-decompose-2279
    why_digest = ""
    # The epic's own Execution Strategy, read from the SAME doc text as the why
    # digest. `group N` already used these waves to partition the children, so
    # handing each child its slice hands back a partition the epic computed
    # rather than asking a builder to recompute one.
    epic_strategy: dict | None = None
    if separate and base_box[0]:
        try:
            epic_text = Path(base_box[0]).read_text(encoding="utf-8")
            why_digest, why_warn = extract_why_digest(epic_text)
            if why_warn:
                typer.echo(f"warning: {why_warn}", err=True)
            epic_strategy = epic_strategy_from_doc(epic_text)
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
    # Rationale (8 lines): docs/architecture/graph-cli-rationale.md#cmd-decompose-2326
            child_root = (child.get("cwd") if child else None) or epic_cwd_box[0] or repo_root()
            canonical = Path(
                canonical_child_plan_path(
                    slug,
                    child_id,
                    str(child_root),
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
                # every commitment the plan must address (task 1.7).
                adopted_nodes = [
                    (aid, (by_id.get(aid) or {}).get("title") or aid)
                    for aid in (grp.get("adopt") or [])
                ]
                canonical.write_text(
                    scaffold_separate_plan(
                        grp,
                        epic_resolved_id,
                        source_doc,
                        why_digest=why_digest,
                        adopted=adopted_nodes,
                        epic_strategy=epic_strategy,
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

    # Rationale (18 lines): docs/architecture/graph-cli-rationale.md#cmd-decompose-2375
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
            #  wave-2 lane: opt-in, default OFF. Wave 0 means "no
            # intra-epic blocker", which `compute_waves` already derives (and
            # already projects as `wave:`), so there is nothing new to compute -
            # only a second reason to spawn beside the `needs_think` flag.
            # Restricted to wave 0 deliberately: those children are genuinely
            # independent, which is the only case where handing design to a cold
            # worker beats one warm context writing several coherent siblings.
            wave0_ids: set[str] = set()
            if think_spawn_on_decompose_wave0(
                project_root=Path(epic_cwd_box[0]) if epic_cwd_box[0] else None
            ):
                from fno.plan._project import plan_docs

                wave_by_id = (plan_docs("waves", epic_id=epic_resolved_id) or {}).get("wave_by_id", {})
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
                        child,
                        run_state=born_rs,
                        env=forced_env,
                        quiet=json_mode(ctx),
                        chain_blueprint=True,
                        why_digest=why_digest,
                        project_root=Path(child_root) if child_root else None,
                    )
    # Rationale (8 lines): docs/architecture/graph-cli-rationale.md#cmd-decompose-2451
                    lane = "wave0" if cid in wave0_ids else "needs_think"
                    fanout.append(
                        {
                            "id": cid,
                            "decision": res.decision,
                            "reason": res.reason,
                            "lane": lane,
                            "owned": res.decision == "spawned",
                        }
                    )
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
            _unowned = [
                c for c in spec_ids if not any(f["id"] == c and f.get("owned") for f in fanout)
            ]
            if not json_mode(ctx):
                typer.echo(
                    f"warning: design fan-out failed ({exc}); "
                    f"{len(_unowned)} child(ren) stay yours to inline-fill"
                    + (f": {', '.join(_unowned)}" if _unowned else ""),
                    err=True,
                )

    # 4b. Report what happened (AC1-UI).
    if json_mode(ctx):
        typer.echo(
            json.dumps(
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
            )
        )
    else:
        typer.echo(f"epic: {epic_resolved_id}")
        typer.echo(f"decomposed into {len(results)} group child node(s) (packaging: {plans}):")
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
            typer.echo(f"  {r['action']}: {r['id']} ({marker}){waves}{blk}{tier}{adopted}")
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
            typer.echo(f"  fan-out OWNS (do NOT inline-fill): {', '.join(_owned)}")
        _unowned_attempts = [fo["id"] for fo in fanout if not fo.get("owned")]
        if _unowned_attempts:
            typer.echo(
                f"  fan-out did NOT claim (inline-fill these): {', '.join(_unowned_attempts)}"
            )
        if orphan_ids:
            typer.echo(
                f"warning: {len(orphan_ids)} group child node(s) no longer in the spec, "
                f"left in place: {', '.join(orphan_ids)}",
                err=True,
            )
        for msg in downgrades:
            typer.echo(f"warning: {msg}", err=True)

    # 4c. Name the epic children no group adopted (US3). Emitted on BOTH
    #     report paths, not just the human one: a --json caller decomposing a
    #     populated epic needs this as much as an operator does, and stderr
    #     never pollutes the JSON on stdout.
    if unadopted_ids:
        _contained_n = len(contained_unadopted_box[0])
        _contained_clause = (
            f"; {_contained_n} of them are contained in the epic itself, which "
            "has no PR, so add them to a group's adopt list or they never close"
            if _contained_n
            else ""
        )
        typer.echo(
            f"warning: {len(unadopted_ids)} epic child(ren) adopted by no group, "
            f"left parented to the epic: {', '.join(unadopted_ids)}. "
            "Add them to a group's `adopt` list to package them into that PR"
            f"{_contained_clause}.",
            err=True,
        )

    # Rationale (9 lines): docs/architecture/graph-cli-rationale.md#cmd-decompose-2571
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
    allow_no_surface: bool = False,
) -> None:
    """Implementation for the intake verb.

    Pulls an existing plan file into the backlog as a new node. Typer-parameter
    defaults are intentionally plain Python values here so the thin command
    wrapper can pass through already-parsed arguments. Kept as a separate
    `_intake_impl` (rather than inlined into `cmd_intake`) so the underlying
    `_intake.py` helpers can be exercised by tests without going through Typer.
    """
    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph.store import commit_rows_via_store
    from fno.graph._intake import (
        _prepare_intake,
        _build_intake_node,
        _refuse_surfaceless_intake,
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
            args,
            all_paths,
            roadmap_id=roadmap_id,
            dry_run=dry_run,
            allow_no_surface=allow_no_surface,
        )
        return

    # Single-path flow
    plan_path = all_paths[0]

    cli_deps: list[str] = [d.strip() for d in deps.split(",") if d.strip()] if deps else []

    # Creation path: the same external-backend refusal every birth path
    # carries (an intake mints new nodes).
    _refuse_create_on_external_backend()

    entries = wire_rows(path=_graph_path())

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
            plan_path,
            entries,
            roadmap_id=roadmap_id,
            cli_title=title,
            cli_priority=priority,
            cli_deps=cli_deps,
            cli_points=points,
            cli_project=project,
            cli_claim=claims,
        )
    except ValueError as e:
        typer.echo(f"Error: {e}", err=True)
        raise typer.Exit(code=1)
    if prep["status"] == "already":
        typer.echo(f"already intaked: {prep['id']}")
        return

    # After _prepare_intake validates and before any write, so the refusal
    # also covers the dry-run preview (surface is mandatory at intake).
    _refuse_surfaceless_intake([plan_path], allow_no_surface=allow_no_surface)

    spec = prep["node_spec"]

    if dry_run:
        verb = "claim" if prep["status"] == "claim" else "intake"
        typer.echo(f"{verb.capitalize()} preview (dry-run, no changes):")
        target = f" (claims {prep['id']})" if prep["status"] == "claim" else ""
        typer.echo(f'  would {verb}: "{spec["title"]}"  (plan: {plan_path}){target}')
        if spec["deps"]:
            typer.echo(f"  blocked_by: {', '.join(spec['deps'])}")
        return

    # Emit "not in ledger" warning before mutating
    from fno.graph._intake import _lookup_ledger_entry

    if _lookup_ledger_entry(plan_path) is None:
        typer.echo("plan_path not in ledger.json - intake will continue anyway", err=True)

    if prep["status"] == "claim":
        claim_id = prep["id"]
        claim_source = prep["claim_source"]

        def claim_mutator(es):
            return _apply_claim_in_place(
                es, claim_id, plan_path=plan_path, spec=spec, project=project
            )

        commit_rows_via_store(_graph_path(), claim_mutator)
        typer.echo(
            f'linked plan to {claim_id} via {claim_source}: "{spec["title"]}" - '
            f"take the work lock: fno do target start {claim_id}"
        )
        # Mirror nav fields onto the just-linked plan of the CLAIMED node too -
        # this branch returns early, so the append-path projection never runs.
        # Routed through the converger so parent_slug is injected consistently.
        try:
            from fno.plan._project import project_graph_nodes

            project_graph_nodes(wire_rows(path=_graph_path()), [claim_id])
        except Exception as e:  # noqa: BLE001 - additive; never wedge the claim
            sys.stderr.write(f"warning: post-claim plan projection failed: {e}\n")
        return

    new_id_holder: list[Optional[str]] = [None]

    def mutator(es):
        node = _build_intake_node(spec, es)
        new_id_holder[0] = node["id"]
        es.append(node)
        return es

    try:
        commit_rows_via_store(_graph_path(), mutator)
    except ValueError as exc:
        # Build-time refusals (the difficulty gate, an invalid band) land as
        # a clean one-line error on the single-file lane; the multi lane
        # already caught these per-file to skip, not abort, the batch.
        typer.echo(f"error: intake refused: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    destination = roadmap_id if roadmap_id else "backlog"
    typer.echo(f'intake {new_id_holder[0]} -> {destination}: "{spec["title"]}"')

    try:
        from fno.graph._intake import _warn_unknown_project, _find_node

        post_entries = wire_rows(path=_graph_path())
        node = _find_node(post_entries, new_id_holder[0] or "")
        landed_project = node.get("project") if node else None
        _warn_unknown_project(landed_project)
    except Exception as e:
        # The mutation already committed; a stray failure in the warning
        # path must not surface as if the intake itself failed.
        sys.stderr.write(f"warning: post-intake project check failed: {e}\n")

    # Filing-time dedup net (plan ): warn if the just-born node resembles
    # an existing one across all live states. Own try/except so a dedup failure
    # is reported as itself, not conflated with the project check above.
    try:
        from fno.graph._intake import _find_node, _warn_similar_nodes

        post_entries = wire_rows(path=_graph_path())
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

            project_graph_nodes(wire_rows(path=_graph_path()), [new_id_holder[0]])
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

            born_node = _find_node(wire_rows(path=_graph_path()), new_id_holder[0])
            if born_node is not None:
                # The native hook re-reads the durable row from the graph.
                on_node_born(born_node, graph_path=_graph_path())
        except Exception:  # noqa: BLE001 - additive; never wedge the intake
            pass


@cli.command(
    "intake",
    hidden=True,
    epilog="Paired verb: `fno backlog remove <id>` deletes the node this creates.",
)
def cmd_intake(
    plan_paths: Optional[List[str]] = typer.Argument(default=None, help="Plan paths"),
    from_list: Optional[str] = typer.Option(
        None, "--from", help="Read paths from FILE or '-' for stdin"
    ),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id", help="Target roadmap ID"),
    title: Optional[str] = typer.Option(None, "--title", "-t", help="Override derived title"),
    priority: Optional[str] = typer.Option(None, "--priority", "-p", help="p0|p1|p2|p3"),
    deps: Optional[str] = typer.Option(None, help="Comma-separated ab-IDs"),
    points: Optional[int] = typer.Option(None, help="Story point estimate"),
    project: Optional[str] = typer.Option(
        None, "--project", help="Override the project field (beats frontmatter and cwd inference)"
    ),
    force_new_roadmap: bool = typer.Option(False, "--force-new-roadmap"),
    batch: bool = typer.Option(False, "--batch", hidden=True),
    dry_run: bool = typer.Option(False, "--dry-run", "-N"),
    claims: Optional[str] = typer.Option(
        None,
        "--claims",
        help=(
            "ab-XXXXXXXX of an existing node, in any state, that this plan "
            "implements. "
            "Updates the node in place rather than creating a new one. "
            "Beats any frontmatter 'claims:' value."
        ),
    ),
    allow_no_surface: bool = typer.Option(
        False,
        "--allow-no-surface",
        help=(
            "Admit a plan whose '## Files to Modify' parses empty. Such a node "
            "cannot be collision-checked, so lane fill dispatches it fail-open."
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
        allow_no_surface=allow_no_surface,
    )


# -- update --


@cli.command("encounter", hidden=True)
def cmd_encounter(
    task_id: str = typer.Argument(..., help="Node id this session hit while doing something else."),
    evidence: str = typer.Option(
        ...,
        "--evidence",
        "-e",
        help="What it cost, in a sentence or two. Required, and capped by config.style.word_cap.encounter.",
    ),
    as_operator: bool = typer.Option(
        False,
        "--operator",
        help="Record this as the operator's vote under the stable 'operator' voter key. A declaration, not proof; one per node, and the casting session's id is kept on the record when provable.",
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the appended record as JSON."),
) -> None:
    """Record ONE encounter with this node, from this session, with evidence.

    An encounter is a thing that happened and cannot be edited or withdrawn:
    there is no correction verb, and a later correction is a `fno backlog note`.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno import rust_binary
    from fno.claims.self_identity import resolve_self_identity
    from fno.config import load_settings
    from fno.graph.store import append_encounter
    from fno.harness_identity import canonical_handle

    evidence = evidence.strip()
    if not evidence:
        typer.echo(
            "Error: an encounter with no evidence is a poll. Name what it cost.",
            err=True,
        )
        raise typer.Exit(code=1)

    try:
        identity = resolve_self_identity()
    except Exception:  # noqa: BLE001 - an unresolvable identity is a refusal, not a crash
        identity = None
    session_id = getattr(identity, "session_id", None)
    session_id = session_id if isinstance(session_id, str) else None
    harness = getattr(identity, "harness", None)
    harness = harness if isinstance(harness, str) else None
    # `--operator` is a declaration, not proof; see the contract doc.
    if not as_operator and (not session_id or not harness):
        typer.echo(
            "Error: no provable session identity, so this encounter would not be "
            "readable back to a transcript. Run `fno whoami` to see what this "
            "session can prove.",
            err=True,
        )
        raise typer.Exit(code=5)

    # Rule 7's escapes, inherited per docs/style-rules.md. Evidence and identity
    # stay required above: the cap is length policy, not the falsifiability one.
    if os.environ.get("FNO_STYLE_ENFORCE") != "0":
        cap = load_settings().style.word_cap.encounter
        err, receipt = rust_binary.style_receipt(evidence, "encounter", cap)
        # A door error, a violation, or a silent binary (no dict) refuses: the gate never vanishes.
        clean = isinstance(receipt, dict) and (receipt.get("exception") or not receipt.get("violations"))
        if not clean:
            typer.echo(err or (receipt or {}).get("report") or "style gate unreadable", err=True)
            raise typer.Exit(code=4)

    record: dict[str, object] = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "evidence": evidence,
    }
    if as_operator:
        record.update({"voter_key": "operator", "voter_kind": "operator"})
        # The canonical provenance keys every encounter carries, so a reader
        # keyed on `session_id` (the falsifiability contract, and every
        # pre-operator record) sees who cast the vote. `voter_key` stays
        # `operator` for the dedupe and the demand split, and takes precedence
        # over `session_id` in voter_key resolution, so provenance cannot
        # fork the operator lane into per-session voters.
        if session_id:
            record["session_id"] = session_id
            record["fno_id"] = canonical_handle(session_id)
        if harness:
            record["harness"] = harness
        record.update(_encounter_provenance(harness))
    else:
        assert session_id is not None and harness is not None
        record.update(
            {
                "session_id": session_id,
                "voter_key": session_id,
                "voter_kind": "agent",
                "harness": harness,
                "fno_id": canonical_handle(session_id),
            }
        )
        record.update(_encounter_provenance(harness))
    appended, error, reason = append_encounter(_graph_path(), task_id, record)
    if not appended:
        typer.echo(f"Error: {error}", err=True)
        if reason == "duplicate":
            typer.echo(
                "Add a `fno backlog note` instead if there is more to say.",
                err=True,
            )
            raise typer.Exit(code=3)
        raise typer.Exit(code=1)

    # Counted inside this verb rather than in a helper. The helper was its own
    # enclosing function, so its graph read had no verb boundary for the
    # external-backend guard to sit on; here the read rides the guard this
    # tracker-owned verb already carries.
    from fno.graph._intake import _find_node

    try:
        stored = _find_node(wire_rows(path=_graph_path()), task_id)
        total = len(stored.get("encounters") or []) if stored else 0
    except Exception:  # noqa: BLE001 - a receipt count must not fail a landed write
        total = 0
    if json_output:
        typer.echo(
            json.dumps(
                {"id": task_id, "encounter": record, "total": total}, separators=(",", ":")
            )
        )
    else:
        if as_operator:
            typer.echo(f"encounter recorded on {task_id} (operator, {total} total)")
        else:
            assert session_id is not None
            typer.echo(
                f"encounter recorded on {task_id} "
                f"(session {canonical_handle(session_id)}, {total} total)"
            )


@cli.command("demand", hidden=True)
def cmd_demand(
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the rows as JSON."),
) -> None:
    """Show where agent encounters and operator priority DISAGREE.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.graph.demand import demand_rows, format_rows
    from fno.tracker import active_backend_name

    # An encounter is footnote-minted metadata that lives only in the graph, and
    # `encounter` is already refused under an external backend. So refuse here
    # too, naming the backend, rather than reading a store that is not the
    # source of truth and reporting an empty signal as if it were measured.
    backend = active_backend_name()
    if backend != "graph":
        typer.echo(
            f"fno backlog demand: encounters live in the graph; under the "
            f"{backend} tracker backend there are none to read.",
            err=True,
        )
        raise typer.Exit(code=1)

    rows = demand_rows(wire_rows(path=_graph_path()))
    if json_output:
        typer.echo(json.dumps(rows, separators=(",", ":")))
    else:
        typer.echo(format_rows(rows))



# -- unclaim / release / requeue: the queue-return subject lives in
# fno.backlog.requeue; registration stays here on the backlog app. --


@cli.command("unclaim", hidden=True)
def cmd_unclaim(
    task_id: str = typer.Argument(
        ..., help="Node id to free (reverts claimed -> ready, releases the lockfile)"
    ),
) -> None:
    from fno.backlog.requeue import _unclaim_node

    _unclaim_node(task_id)


@cli.command("requeue", hidden=True)
def cmd_requeue(
    node: str = typer.Argument(..., help="Node id / slug / bare-hex to return to the queue."),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit a structured receipt."),
) -> None:
    """Return a node wedged in_progress by a dead worker to the queue.

    The re-lock door retired with the graph claim mirror; re-acquisition is a claim store acquire, not a backlog verb.
    """
    from fno.backlog.requeue import cmd_requeue as _impl

    _impl(node, json_out=json_out)


# -- next --


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
    """The transient joined selection model: the Rust snapshot exactly once.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.tracker import get_tracker

    tracker = get_tracker()
    try:
        entries = tracker._call("snapshot")["entries"]  # type: ignore[attr-defined]
    except Exception as exc:  # noqa: BLE001 - name the backend, fail closed
        raise _ExternalSelectionError(f"tracker {tracker.name!r} snapshot failed: {exc}") from exc
    return [e for e in entries if e.get("state") == "open"]


    # Rationale (8 lines): docs/architecture/graph-cli-rationale.md#joined-open-candidates-4307
EXTERNAL_SELECTION_TTL = "15m"


def _dispatch_node_summary(e) -> dict:
    """The ONE projection a dispatcher sees when it picks work.

    `next` and `ready` both feed autonomous dispatch, so the two summaries
    are one thing spelled twice: the union of every field either surface
    carried, no filtering, no ordering. A key dropped here is dropped from
    every dispatch decision - the silent `dispatch_verb` loss this exists
    to prevent. Consumers read by key, so additions are safe.
    """
    return {
        # slug leads () so a list / clipboard is readable; `id` stays the
        # canonical key right after.
        "slug": e.get("slug"),
        "id": e["id"],
        "title": e.get("title"),
        "priority": e.get("priority"),
        "domain": e.get("domain"),
        "project": e.get("project"),
        "cwd": e.get("cwd"),
        "parent": e.get("parent"),
        "size": e.get("size"),
        "difficulty": e.get("difficulty"),
        # the native lane-fill door's dispatch-time collision gate compares plan file
        # surfaces; without this it has nothing to read.
        "plan_path": e.get("plan_path"),
        # The per-node model pin rides so the active-backlog drain can
        # prefer it over cfg.model.
        "model": e.get("model"),
        # The per-node dispatch overrides must ride so the resolver's
        # verb/brief routing fires for real graph nodes, not only for
        # tests that inject them.
        "dispatch_verb": e.get("dispatch_verb"),
        "dispatch_brief": e.get("dispatch_brief"),
        "mission_id": e.get("mission_id"),
        "mission_wave": e.get("mission_wave"),
        "mission_slug": e.get("mission_slug"),
        "mission_from_msg_id": e.get("mission_from_msg_id"),
        # age and rank ride too, so a dispatcher can order and
        # staleness-check without a second `backlog get` per node.
        "created_at": e.get("created_at"),
        "touched_at": e.get("touched_at"),
        "rank": e.get("rank"),
    }


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
    from fno.graph._intake import (
        detect_project,
        descendants_of,
        _find_node,
    )
    from fno.tracker import active_backend_name

    result: list = [None]
    project_filter = project
    _external = active_backend_name() != "graph"

    # The graph-backend answer is native: the door (fno backlog next) owns
    # the prelude, the occupancy, the selection, the reservation, and the
    # receipts. This wheel spelling keeps only the external-backend branch,
    # whose joined tracker candidates the door forwards here from inside its
    # own arm.
    if not _external:
        typer.echo(
            "Error: the selection is served by the native door; "
            "run `fno backlog next`.",
            err=True,
        )
        raise typer.Exit(code=2)

    # One read for the prelude AND selection: the transient joined model
    # (list_open + sidecar join, fail-closed).
    try:
        pre_entries = _joined_open_candidates()
    except _ExternalSelectionError as exc:
        typer.echo(f"Error: {exc}; selection refused", err=True)
        raise typer.Exit(code=1)
    if not project_filter and not all_:
        assert pre_entries is not None  # set under the same condition above
        project_filter = detect_project(pre_entries)
    # Rationale (8 lines): docs/architecture/graph-cli-rationale.md#cmd-next-4424
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

    def _select(entries, occupancy):
        from fno.graph._intake import repo_root
        from fno.graph.store import (
            ClaimsUnavailableError,
            ReadyParentMissingError,
            StoreUnavailable,
            ready as store_ready,
        )

        try:
            return store_ready(
                project=project_filter,
                all=all_,
                roadmap_id=roadmap_id,
                mission=mission,
                parent=parent_target_id,
                include_ideas=include_ideas,
                include_deferred=include_deferred,
                repo_root=repo_root(),
                entries=entries,
                occupancy=occupancy,
            )["rows"]
        except StoreUnavailable as exc:
            typer.echo(f"Error: store keeper unavailable; selection refused: {exc}", err=True)
            raise typer.Exit(code=1) from exc
        except ReadyParentMissingError as exc:
            typer.echo(f"Error: {exc}", err=True)
            raise typer.Exit(code=1) from exc
        except ClaimsUnavailableError as exc:
            typer.echo(f"Error: {exc}", err=True)
            raise typer.Exit(code=1) from exc

    from fno.backlog.undispatched import (
        ObserverReadError,
        build_selection_divergence_event,
        prepend_missed_rows,
        read_planned_unclaimed_from_entries,
    )

    # The claim set of the selection that actually ran, for the receipts below.
    selection_claimed: list = [set()]

    def _prepare(entries: list[dict]) -> tuple[set, dict]:
        """Dispatch occupancy plus the observer receipt, read ONCE per selection.

        The claim verdict and the roster read are what this command costs, and
        three layers each used to pay for them. A `--claim` transaction that
        loses to an interleaved writer pays again on its retry, deliberately:
        the entries it selects from are new, so its occupancy must be too.
        """
        try:
            claimed, worked = read_occupancy(entries, _live_claimed_node_ids)
        except OccupancyUnavailable as exc:
            typer.echo(f"Error: {exc}; selection refused", err=True)
            raise typer.Exit(code=1) from exc
        try:
            observer = read_planned_unclaimed_from_entries(
                entries,
                project=None if all_ else project_filter,
                mission=mission,
                roadmap_id=roadmap_id,
                parent=parent_target_id,
                worked=worked,
            )
        except ObserverReadError as exc:
            typer.echo(f"Error: {exc}; selection refused", err=True)
            raise typer.Exit(code=1) from exc
        except Exception as exc:  # noqa: BLE001 - unknown state refuses recovery
            typer.echo(f"Error: observer revalidation failed: {exc}", err=True)
            raise typer.Exit(code=1) from exc
        selection_claimed[0] = claimed
        return claimed | set(worked), observer

    def _with_observer(
        candidates: list[dict],
        source_entries: list[dict],
        occupied: set,
        current_observer: dict,
    ) -> list[dict]:
        by_id = {entry.get("id"): entry for entry in source_entries}
        container_ids = _container_ids(source_entries)
        from fno.backlog.advance import _guard_staleness_days, selection_guards

        guard_now = datetime.now(timezone.utc)
        guard_stale = _guard_staleness_days()
        safe_rows = []
        for row in current_observer["rows"]:
            entry = by_id.get(row.get("id"))
            if entry is None or row.get("id") in occupied:
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
        if pre_entries is not None:
            from fno.claims.cli import _parse_ttl
            from fno.claims.core import ClaimHeldByOther, acquire_claim

            occupied, observer = _prepare(pre_entries)
            candidates = _with_observer(
                _select(pre_entries, occupied), pre_entries, occupied, observer
            )
            for winner in candidates:
                key = f"node:{winner['id']}"
                try:
                    acquire_claim(
                        key,
                        claim,
                        ttl_ms=_parse_ttl(EXTERNAL_SELECTION_TTL),
                    )
                except ClaimHeldByOther:
                    continue
                result[0] = _dispatch_node_summary(winner)
                break
    else:
        assert pre_entries is not None
        entries = pre_entries
        occupied, observer = _prepare(entries)
        candidates = _with_observer(_select(entries, occupied), entries, occupied, observer)
        if candidates:
            result[0] = _dispatch_node_summary(candidates[0])

    # Zero-silent-starvation receipts (G1): explain to stderr why nothing
    # was picked - and, even when a winner WAS picked, which in-scope nodes
    # are stranded under terminal parents. Advisory - stdout stays
    # exactly the node-or-"null" contract `_next_node` parses, so a receipt
    # failure never breaks dispatch. Under an external backend the receipts
    # explain the ACTUAL joined denominator, never the local graph.
    try:
        from fno.backlog.advance import _guard_staleness_days

        recv_entries = pre_entries or []
        scope_ids = (
            descendants_of(recv_entries, parent_target_id)
            if parent_target_id is not None
            else None
        )
        receipts = _starvation_receipts(
            recv_entries,
            project_filter,
            all_,
            scope_ids,
            selection_claimed[0],
            datetime.now(timezone.utc),
            _guard_staleness_days(),
            mission=mission,
            roadmap_id=roadmap_id,
        )
    except Exception as exc:  # noqa: BLE001 - receipts are advisory
        typer.echo(f"warning: starvation receipts failed: {exc}", err=True)
    else:
        if result[0] is None:
            for nid, reason in receipts:
                typer.echo(f"excluded {nid}: {reason}", err=True)
        else:
            # A winner exists: only the strand receipts fire, capped, so a
            # healthy dispatch never drowns in exclusion noise again.
            for line in _stranded_next_receipts(receipts):
                typer.echo(line, err=True)

    typer.echo(json.dumps(result[0], indent=2) if result[0] else "null")


@cli.command("undispatched", hidden=True)
def cmd_undispatched(
    project: Optional[str] = typer.Option(None, "--project", "-p"),
    all_: bool = typer.Option(False, "--all", "-A"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    parent: Optional[str] = typer.Option(None, "--parent"),
    mission: Optional[str] = typer.Option(None, "--mission"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Name finalized, ready leaf plans with no node claim.

    The graph-backend answer is native: the door (fno backlog undispatched)
    owns the classify, the claim keys, and the worked fold. This wheel
    spelling keeps only the external-backend branch, whose joined tracker
    candidates the door forwards here from inside its own arm.
    """
    del all_, json_output  # the observer is JSON by contract and all-scoped by default
    from fno.backlog.undispatched import ObserverReadError, read_planned_unclaimed_from_entries
    from fno.tracker import active_backend_name

    if active_backend_name() == "graph":
        typer.echo(
            "Error: the observer is served by the native door; "
            "run `fno backlog undispatched`.",
            err=True,
        )
        raise typer.Exit(code=2)

    try:
        receipt = read_planned_unclaimed_from_entries(
            _joined_open_candidates(),
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


@cli.command(
    "ready", hidden=True,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    epilog=("Native date filters: --created-before/--created-after/--touched-before/"
            "--touched-after <Nd|YYYY-MM-DD>, --sort created|touched. Touched falls back to created."),
)
def cmd_ready(
    ctx: typer.Context,
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
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON (default; flag accepted for parity)."
    ),
) -> None:
    from fno.graph._intake import repo_root
    from fno.graph.store import ClaimsUnavailableError, StoreUnavailable, ready as store_ready
    from fno.tracker import active_backend_name

    # External backends share the Rust filters and ranking with `next`.
    entries = None
    if active_backend_name() != "graph":
        try:
            entries = _joined_open_candidates()
        except _ExternalSelectionError as exc:
            typer.echo(f"Error: {exc}; selection refused", err=True)
            raise typer.Exit(code=1)

    try:
        result = store_ready(
            project=project,
            all=all_,
            roadmap_id=roadmap_id,
            parent=parent,
            mission=mission,
            include_ideas=include_ideas,
            include_deferred=include_deferred,
            repo_root=repo_root(),
            entries=entries,
            filter_args=list(ctx.args or []),
        )
    except StoreUnavailable as exc:
        typer.echo(f"Error: store keeper unavailable; ready selection refused: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    except ClaimsUnavailableError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    typer.echo(json.dumps(result["rows"], indent=2))


# -- lane-fill --


# -- lane-fill (native door) --


@cli.command("lane-fill", hidden=True)
def cmd_lane_fill(
    max_lanes: Optional[int] = typer.Option(None, "--max"),
    project: Optional[str] = typer.Option(None, "--project", "-p"),
    mission: Optional[str] = typer.Option(None, "--mission"),
    claim: bool = typer.Option(False, "--claim"),
) -> None:
    """The parallel fill answers natively; there is no external body here.

    Unlike next/undispatched this wheel spelling keeps nothing: the tombstone
    is the whole arm, on every backend.
    """
    del max_lanes, project, mission, claim
    typer.echo(
        "Error: the parallel fill is served by the native door; "
        "run `fno backlog lane-fill`.",
        err=True,
    )
    raise typer.Exit(code=2)


@cli.command("dispatch-lanes", hidden=True)
def cmd_dispatch_lanes(
    max_lanes: Optional[int] = typer.Option(
        None, "--max", help="Max lanes (default: config.parallel.max_lanes)."
    ),
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Filter by project name"),
    mission: Optional[str] = typer.Option(
        None, "--mission", help="Restrict dispatch to this mission's nodes."
    ),
    model: Optional[str] = typer.Option(
        None,
        "--model",
        "-m",
        help="Pin a model for every lane spawned this run, overriding node annotations.",
    ),
    harness: Optional[str] = typer.Option(
        None,
        "--harness",
        "-H",
        help="Pin the CLI harness for every lane.",
    ),
    provider: Optional[str] = typer.Option(
        None,
        "--provider",
        "-P",
        help="Pin the model vendor for every lane.",
    ),
    source: Optional[str] = typer.Option(
        None,
        "--source",
        help="Dispatch origin for every lane worker's name : the daemon passes ab; an attended run passes nothing.",
    ),
) -> None:
    """Spawn up to max_lanes isolated background lanes (parallel mode, group 3).

    Selects collision-clean ready nodes (like ``lane-fill``), then for each one
    isolates a worktree off origin/main, seeds its per-lane
    ``.fno/config.local.toml``, and spawns a detached ``/target`` worker rooted
    there. Prints one JSON object: ``lanes`` plus a ``fill`` summary.
    ``max_lanes < 1`` spawns nothing (a single lane is the daemon's path).
    """
    from fno.dispatch_flags import (
        DispatchFlagError,
        reject_empty_model,
    )
    from fno.backlog.advance import dispatch_lanes

    _refuse_unknown_source("dispatch-lanes", source)

    try:
        model = reject_empty_model(model)
        if harness is not None:
            harness = harness.strip()
            if not harness:
                raise DispatchFlagError("--harness must not be empty")
            from fno.harness_names import SPAWN_HARNESSES
            from fno.harness_names import unknown_thread_harness_message

            if harness not in SPAWN_HARNESSES:
                # The builder the spawn seam raises, rendered from the tuples
                # beside it - so this seam needs no capability-table read.
                raise DispatchFlagError(unknown_thread_harness_message(harness))
        if provider is not None:
            provider = provider.strip()
            if not provider:
                raise DispatchFlagError("--provider must not be empty")
            from fno.harness_names import KNOWN_HARNESSES

            if provider in KNOWN_HARNESSES:
                raise DispatchFlagError(
                    f"--provider names the model VENDOR axis; {provider!r} is a "
                    f"HARNESS. Use -H/--harness {provider}."
                )
    except DispatchFlagError as exc:
        typer.echo(f"dispatch-lanes: {exc}", err=True)
        raise typer.Exit(code=2)

    if max_lanes is None:
        from fno.config import load_settings

        max_lanes = load_settings().parallel.max_lanes

    fill: dict = {}
    receipts = dispatch_lanes(
        max_lanes,
        project,
        mission=mission,
        model=model,
        harness=harness,
        vendor=provider,
        report=fill,
        source=source,
    )
    # dispatch_lanes fills every key before returning (both the no-selection and
    # completion paths write the report), so the shape is read directly.
    fill_output = {
        "requested": fill["requested"],
        "selected": fill["filled"],
        "dispatched": fill["dispatched"],
        "skipped": fill["skipped"],
        "stop": fill["stop"],
        "excluded": fill["excluded"],
    }
    typer.echo(json.dumps({"lanes": receipts, "fill": fill_output}, indent=2))
    if receipts and not any(
        receipt.get("status") == "dispatched" for receipt in receipts
    ):
        raise typer.Exit(code=1)


# -- join --


@cli.command("join", hidden=True)
def cmd_join(
    node: str = typer.Argument(..., help="Node id (slug / bare hex resolve)."),
    workers: Optional[int] = typer.Option(
        None, "--workers",
        help="Requested joiner count. Empty derives it from the node priority "
        "and the plan's highest wave band; the plan's ready-graph width "
        "bounds it either way.",
    ),
    model: Optional[str] = typer.Option(
        None, "--model", "-m",
        help="Explicit model for the joiner spawns ( shape); empty "
        "leaves the spawn CLI's own default resolution.",
    ),
) -> None:
    """Spawn execute-waves joiners into a held node's worktree .
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.backlog.advance import JoinRefuse, join_node
    from fno.graph.fuzzy import resolve_node

    entries = _display_entries("backlog.join")
    match = resolve_node(node, entries)
    if match.kind != "exact" or not match.id:
        typer.echo(f"backlog join: no node matches '{node}'", err=True)
        raise typer.Exit(code=2)

    try:
        receipt = join_node(match.id, workers, model=model)
    except JoinRefuse as exc:
        typer.echo(f"backlog join: {exc.message}", err=True)
        raise typer.Exit(code=exc.code)
    typer.echo(json.dumps(receipt, indent=2))


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
    LANE_SLOT_PREFIX = "lane-slot:"  # the binary-owned slot namespace

    try:
        from fno.config import load_settings

        max_lanes = load_settings().parallel.max_lanes
    except Exception:  # noqa: BLE001 - a config miss must not hide live lanes
        max_lanes = 1

    nodes: dict = {}
    try:

        nodes = {
            e["id"]: e for e in wire_rows(path=_graph_path()) if isinstance(e, dict) and e.get("id")
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
    from fno.graph.board import (
        board_unreadable_payload,
        compute_board,
        print_board,
    )
    from fno.graph.store import GraphUnreadableError, StoreUnavailable, read_graph_strict
    from fno.tracker import active_backend_name

    # The board spans done + in-progress + ready sections, which needs
    # lookback past list_open()'s open-only contract (the same done-at-
    # PR-green grace window pr_watch's discovery needs and cannot get from
    # the tracker seam without storage-engine work, out of scope here). An
    # external backend degrades the same way an unreadable graph already
    # does, rather than reading the wrong store.
    if active_backend_name() != "graph":
        payload = board_unreadable_payload(
            f"board is unavailable under the {active_backend_name()} tracker backend"
        )
        if json_output:
            typer.echo(json.dumps(payload))
        else:
            print_board(payload, typer.echo)
        raise typer.Exit(code=1)

    try:
        entries = read_graph_strict(_graph_path())
    except GraphUnreadableError as exc:
        payload = board_unreadable_payload(f"graph unreadable ({exc})")
        if json_output:
            typer.echo(json.dumps(payload))
        else:
            print_board(payload, typer.echo)
        raise typer.Exit(code=1)
    except StoreUnavailable as exc:
        # Same degradation as an unreadable graph: the store's keeper being
        # unreachable (a pip-only box with no fno-agents-worker, a packet
        # that has not built the binary yet) is an unreadable SOURCE, and
        # this command's contract is an explicit unknown, never a traceback.
        payload = board_unreadable_payload(f"graph store unavailable ({exc})")
        if json_output:
            typer.echo(json.dumps(payload))
        else:
            print_board(payload, typer.echo)
        raise typer.Exit(code=1)

    proj = project
    if proj is None:
        from fno.graph._intake import detect_project

        proj = detect_project(entries)

    board = compute_board(entries, project=proj)
    if json_output:
        typer.echo(json.dumps(board))
        return
    print_board(board, typer.echo)


@cli.command("render-views", hidden=True)
def cmd_render_views() -> None:
    """Replay the canonical post-publish views after a native store write:
    a fresh read renders the same graph.md and configured board targets a
    CLI write would. A failed view exits nonzero, so the keeper withholds
    the rendered_version stamp and its backoff retries the pass.
    """
    from fno.graph.store import render_canonical_views

    failed = render_canonical_views()
    if failed:
        typer.echo(f"render-views: {failed} view(s) failed", err=True)
        raise typer.Exit(code=1)


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
        _echo_node_entry(joined, field, False)
        return
    typer.echo(json.dumps(joined, indent=2))


def _echo_node_entry(e: dict, field: Optional[str], grouped: bool) -> None:
    """Render one resolved node: the field / grouped / JSON output ladder."""
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


@cli.command("get")
def cmd_get(
    ids: List[str] = typer.Argument(
        ..., help="One or more node ab-id, slug, or bare 8-hex (e.g.  | dashless-spawn | ff6f96e0)"
    ),
    field: Optional[str] = typer.Option(None, help="Print only this field"),
    grouped: bool = typer.Option(
        False, "--grouped", help="Render populated fields in human-readable concept groups."
    ),
    strict: bool = typer.Option(
        False,
        "--strict",
        help="Exact-only resolution (id/slug/bare-hex); never fuzzy. The stable surface the /think router seeds a design from - a miss exits 1 so a typo'd token can never silently seed.",
    ),
) -> None:
    from fno.tracker import active_backend_name

    from fno.graph.get_batch import resolve_or_dispatch
    id = resolve_or_dispatch(ids, field=field, grouped=grouped, strict=strict)

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


# -- project-root (work-map resolution; null-for-unmapped) --


@cli.command("project-root", hidden=True)
def cmd_project_root(
    project: str = typer.Argument(
        ..., help="Project name to resolve against config.work.workspaces."
    ),
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
        resolve_transcript(
            sc.source_harness,
            sc.source_session_id,
            sc.source_cwd or sc.cwd,
            projects_root=_DEFAULT_PROJECTS_ROOT,
        )
        if sc.source_session_id
        else None
    )
    spawn_result = (
        resolve_transcript(
            sc.spawned_by_harness,
            sc.spawned_by_session,
            sc.spawned_by_cwd,
            projects_root=_DEFAULT_PROJECTS_ROOT,
        )
        if sc.spawned_by_session
        else None
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
            "pr": pr_block(sc),
            "sessions": sc.sessions,
            "lifecycle": None,  # roster derives from sc.sessions; kept for shape parity
            "source_node_id": sc.source_node_id,
            "source_node_title": _title_of(sc.source_node_id),
            "source_plan_path": sc.source_plan_path,
            "related": [],  # footnote-minted; unavailable under an external backend
        }
        if spawned:
            output["spawned"] = {
                "nodes": [{"depth": d, "id": nid, "title": _title_of(nid)} for d, nid in walk_rows],
                "cycle_detected": False,
                "truncated_at_depth": None,
            }
        typer.echo(json.dumps(output, indent=2))
        return
    typer.echo(f"provenance for {node.id}: {node.title or ''}")
    typer.echo(f"  node_birth: {sc.source_session_id or '(none)'}")
    typer.echo(f"  spawn: {sc.spawned_by_session or '(none)'}")
    typer.echo(render_pr_line(pr_block(sc)))
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
        from fno.graph.store import served_store_path
        typer.echo(f"No node matching '{id}' in {served_store_path(_graph_path())}", err=True)
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
            birth_harness,
            birth_session,
            birth_cwd,
            projects_root=_DEFAULT_PROJECTS_ROOT,
        )

    # spawn edge: uses spawned_by_cwd
    spawn_session = e.get("spawned_by_session")
    spawn_harness = e.get("spawned_by_harness")
    spawn_cwd = e.get("spawned_by_cwd")
    spawn_result = None
    if spawn_session:
        spawn_result = resolve_transcript(
            spawn_harness,
            spawn_session,
            spawn_cwd,
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

    # Runtime-attempt projection (wave 3): live/suspect/stale/interrupted
    # attempts read off manifests + the claim, alongside the confirmed lifecycle
    # rows below. Never a confirmed `do` row; never a graph mutation.
    from fno.provenance.runtime_attempts import runtime_attempts

    runtime = runtime_attempts(node_id, e)

    # Lifecycle roster (): per-phase start/end/duration + an honest total,
    # computed once for both the JSON and human paths.
    roster_lines, roster_summary = _lifecycle_roster(
        e["sessions"], registry_status_index()
    )

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
            # Append-only lifecycle provenance in raw append order ().
            # read_graph's defaults guarantee the key, so no fallback guard.
            "pr": pr_block(e),
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
                    {"depth": d, "id": n.get("id"), "title": n.get("title")} for d, n in walk_rows
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
    lines.append(render_pr_line(pr_block(e)))

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

    # Runtime attempts (wave 3): live/suspect/stale/interrupted attempts
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

    # Lifecycle roster (, ): per-phase start/end/duration + an
    # honest total. Distinct from the birth/spawn edges above -- those are
    # single parent pointers; this is the per-phase who-did-what across sessions
    # and harnesses. Every lifecycle phase always renders (not recorded / end
    # only for gaps) so a roster never reads "nobody touched this" by omission.
    lines.append("  lifecycle:")
    lines.extend(roster_lines)

    typer.echo("\n".join(lines))


# Session lifecycle verbs (add/open/close/backfill/reap-open) went native:
# crates/fno-agents/src/backlog/session_cli.rs answers `fno backlog session`
# at the door, and the Python surface no longer mounts a twin.


# -- task rows + task claims (epic  group 3) --

task_app = typer.Typer(
    name="task",
    help="Task-grain rows and the task claim on status transition ().",
    no_args_is_help=True,
    add_completion=False,
)


@task_app.callback()
def _task_callback() -> None:
    """Keep ``list``/``update`` real subcommands (single-command Typer collapse)."""


#: An unreadable graph on a task verb. Distinct from 3, which the wave loop
#: reads as "a peer holds it, skip this round" (see waves.md 3e).
TASK_GRAPH_UNREADABLE_EXIT = 5

#: This node has no task grain to guard, so the caller dispatches unguarded
#: exactly as a run with no bound node does. A node with no bound plan and a
#: non-graph tracker backend both land here. Neither is a stop: the first is
#: one live node in five, and the second is every node in a tracker-backend
#: project, so reading either as a refusal halts a wave that used to run.
TASK_NO_GRAIN_EXIT = 6


def _task_plan_or_exit(node_token: str, graph_path: Path) -> tuple[str, str]:
    """Resolve NODE to ``(node_id, plan_path)``; exit 1/2 on the named refusals.

    Honors the caller's graph redirect. Exact rows use the native read;
    missing rows fall back for bare-hex resolution and strict read failures.
    An unreadable graph exits TASK_GRAPH_UNREADABLE_EXIT, never peer-held;
    an unreadable bound plan refuses instead of reporting an empty task list.
    """
    from fno.graph.collision import resolve_plan_path
    from fno.graph.fuzzy import resolve_node
    from fno.graph.store import GraphUnreadableError, read_graph_strict, read_nodes_by_ids

    try:
        fast = read_nodes_by_ids(graph_path, [node_token])
        match = resolve_node(node_token, [row for row in (fast or {}).get("entries") or [] if not row.get("archived_at")])
        if match.kind != "exact":
            match = resolve_node(node_token, read_graph_strict(graph_path))
    except GraphUnreadableError as e:
        typer.echo(
            f"Could not read the graph cleanly, so '{node_token}' cannot be "
            f"resolved: {e}",
            err=True,
        )
        raise typer.Exit(code=TASK_GRAPH_UNREADABLE_EXIT)
    if match.kind != "exact":
        typer.echo(f"Error: node '{node_token}' does not resolve to a node", err=True)
        raise typer.Exit(code=1)
    entry = match.candidates[0]
    plan_path = entry.get("plan_path") or ""
    if not plan_path:
        typer.echo(
            f"no plan bound to {entry.get('id')}; no task grain to guard",
            err=True,
        )
        raise typer.Exit(code=TASK_NO_GRAIN_EXIT)
    # The graph stores plan_path absolute, `~`-prefixed, or repo-relative, so a
    # raw Path() read refuses two of the three forms and the caller dispatches
    # unclaimed - the double-dispatch these verbs exist to close.
    resolved = resolve_plan_path(plan_path)
    if not resolved.is_file():
        typer.echo(f"plan file for {entry.get('id')} is not readable: {plan_path}", err=True)
        raise typer.Exit(code=1)
    return entry["id"], str(resolved)


def _task_ids_or_exit(plan_path: str) -> list[str]:
    """Plan task ids, or a named exit-1 refusal on a malformed plan.

    A plan with a broken Execution Strategy fence must refuse like every
    other task-verb failure mode, never exit through a parser traceback.
    """
    from fno.graph.tasks import derive_task_ids

    try:
        ids = derive_task_ids(Path(plan_path))
    except Exception as exc:  # noqa: BLE001 - stop-not-traceback contract
        typer.echo(f"plan parse failed for {plan_path}: {exc}", err=True)
        raise typer.Exit(code=1)
    if not ids:
        # A plan declaring `waves:` and no top-level `tasks:` parses fine and
        # derives nothing. That is the same "no grain here" as an unbound
        # plan, not the exit 2 that halts a wave which ran before task rows
        # existed.
        typer.echo(f"no tasks declared by {plan_path}", err=True)
        raise typer.Exit(code=TASK_NO_GRAIN_EXIT)
    return ids


@task_app.command("list")
def cmd_task_list(
    node: str = typer.Argument(..., help="Node id / slug / bare-hex with a bound plan."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit rows as JSON."),
) -> None:
    """List the node's task rows, materializing any the plan adds.

    The first read of a node with a bound plan writes the missing ``pending``
    rows, so a peer never sees a node whose tasks are invisible until someone
    starts one. Once every plan task id has a row, the read is read-only: a
    polling fleet does not take the graph lock and re-render the board on a
    steady-state no-op.
    """
    from pathlib import Path

    from fno.graph.store import commit_rows_via_store
    from fno.graph.tasks import ensure_task_rows

    node_id, plan_path = _task_plan_or_exit(node, _graph_path())
    ids = _task_ids_or_exit(plan_path)

    def _print(rows: list[dict]) -> None:
        if json_output:
            typer.echo(json.dumps({"node": node_id, "tasks": rows}, indent=2))
            return
        for r in rows:
            typer.echo(f"{r.get('id')}\t{r.get('status')}\t{r.get('owner') or '-'}")

    # Fast path: every plan id already has a row -> print without the lock.
    # An id-less plan with no rows has nothing to materialize on ANY poll, so
    # it refuses here too rather than paying a full locked write each time.
    for e in wire_rows(path=_graph_path()):
        if isinstance(e, dict) and e.get("id") == node_id:
            # The keeper round-trip drops a null owner key; restore it.
            rows = [
                {**r, "owner": r.get("owner")}
                for r in e.get("tasks") or []
                if isinstance(r, dict)
            ]
            known = {r.get("id") for r in rows}
            if all(i in known for i in ids):
                if rows:
                    _print(rows)
                    return
                if not ids:
                    typer.echo(f"no tasks declared by {plan_path}", err=True)
                    raise typer.Exit(code=2)
            break

    materialized: list[dict] = []

    def mutator(entries):
        for e in entries:
            if isinstance(e, dict) and e.get("id") == node_id:
                materialized.extend(ensure_task_rows(e, Path(plan_path), ids))
                break
        return entries

    commit_rows_via_store(_graph_path(), mutator)
    if not materialized:
        typer.echo(f"no tasks declared by {plan_path}", err=True)
        raise typer.Exit(code=2)
    _print(materialized)


@task_app.command("update")
def cmd_task_update(
    node: str = typer.Argument(..., help="Node id / slug / bare-hex with a bound plan."),
    task_id: str = typer.Argument(..., help="Task id as declared by the plan."),
    status: str = typer.Option(
        ..., "--status", help="pending | in_progress | done."
    ),
    owner: Optional[str] = typer.Option(
        None,
        "--owner",
        help=(
            "Names THIS caller as the holder (tests / operators / a "
            "hand-started session that can prove its own id). The escape for "
            "a GONE holder is --takeover, not this flag. Never a head-8 "
            "handle - a codex UUIDv7 head-8 is a ~65.5s clock bucket two "
            "workers can share."
        ),
    ),
    takeover: bool = typer.Option(
        False,
        "--takeover",
        help=(
            "Take a task whose holder is GONE: the stale claim and row owner "
            "clear under MY resolved identity (worker name or session id). A "
            "live holder is never takeable (exit 3). Mutually exclusive with "
            "--owner."
        ),
    ),
) -> None:
    """Transition one task row; the claim IS the transition.

    ``--status in_progress`` takes the ``task:<node>:<task>`` claim FIRST
    (exit 3 naming the holder when a peer owns it, exit 4 when no per-worker
    identity is provable and none was passed), then writes the row.
    ``--status done`` writes the row and releases the claim. ``--status
    pending`` is the holder-only give-back for a blocked or failed task.
    ``--takeover`` is the gone-holder escape for all three transitions.
    """
    from pathlib import Path

    from fno.claims.core import (
        ClaimContended,
        ClaimHeldByOther,
        ClaimValidationError,
    )
    from fno.claims.self_identity import resolve_task_holder
    from fno.claims.session_pid import resolve_session_harness, resolve_session_pid
    from fno.claims.tasks import acquire_task, release_task, task_key
    from fno.graph.store import commit_rows_via_store
    from fno.graph.tasks import TASK_STATUSES

    if status not in TASK_STATUSES:
        typer.echo(
            f"invalid --status {status!r}; one of {', '.join(TASK_STATUSES)}", err=True
        )
        raise typer.Exit(code=2)
    if owner and takeover:
        typer.echo(
            "--owner names this caller as the holder; --takeover replaces a "
            "gone holder with the ambient identity. One flag, one meaning - "
            "pass one, never both",
            err=True,
        )
        raise typer.Exit(code=2)
    node_id, plan_path = _task_plan_or_exit(node, _graph_path())
    ids = _task_ids_or_exit(plan_path)
    if task_id not in ids:
        typer.echo(
            f"task '{task_id}' not in plan {plan_path}; plan tasks: "
            f"{', '.join(ids) or '(none)'}",
            err=True,
        )
        raise typer.Exit(code=2)

    # The claims layer's per-worker holder resolver: a spawned worker's roster
    # name, else a session id this process can prove (or that at least is not
    # the worktree manifest's shared value). The graph layer may not import
    # fno.agents, so the resolver lives in fno.claims.
    if owner:
        holder: Optional[str] = owner
    else:
        holder, identity_reason = resolve_task_holder()
        if not holder:
            typer.echo(
                f"cannot prove a per-worker identity ({identity_reason}); "
                "set --owner <full-session-id> or spawn through "
                "fno agents spawn",
                err=True,
            )
            raise typer.Exit(code=4)
    # The owner arm assigns a truthy owner; the resolver arm raised on empty.
    assert holder is not None
    key = task_key(node_id, task_id)

    # Rationale (10 lines): docs/architecture/graph-cli-rationale.md#cmd-task-update-6154
    from fno.claims.core import claim_status as _live_check

    try:
        _st = _live_check(key)
    except Exception:  # noqa: BLE001 - an unreadable claim blocks nothing
        _st = {}
    if _st.get("state") == "live":
        if takeover:
            typer.echo(
                f"{key} is held LIVE by {_st.get('holder')} "
                f"(pid={_st.get('pid')}); "
                + (
                    "--takeover is the escape for a holder that is gone, not "
                    "a way to take a claim from a running worker"
                    if _st.get("holder") == holder
                    else "a running worker is never takeable"
                ),
                err=True,
            )
            raise typer.Exit(code=3)
        if _st.get("holder") == holder and _st.get("pid") != resolve_session_pid():
            typer.echo(
                f"{key} is held LIVE by this holder identity under another "
                f"live process (pid={_st.get('pid')}); the identity is "
                "shared, not owned. Re-run from the owning session, or spawn "
                "through fno agents spawn",
                err=True,
            )
            raise typer.Exit(code=4)

    def _set_row(mutate) -> Optional[dict]:
        found: list[dict] = []

        def mutator(entries):
            for e in entries:
                if isinstance(e, dict) and e.get("id") == node_id:
                    from fno.graph.tasks import ensure_task_rows

                    ensure_task_rows(e, Path(plan_path), ids)
                    for row in e.get("tasks") or []:
                        if isinstance(row, dict) and row.get("id") == task_id:
                            mutate(row)
                            found.append(dict(row))
                            break
                    break
            return entries

        commit_rows_via_store(_graph_path(), mutator)
        return found[0] if found else None

    def _release_claim_or_note() -> None:
        # Our own claim releases by name. A takeover'd GONE holder's claim file
        # names the gone holder, so this unlinks nothing there - it reads stale
        # (its pid is dead) and frees by liveness.
        release_task(node_id, task_id, holder)

    if status == "in_progress":
        pid = resolve_session_pid()
        harness = resolve_session_harness()
        # acquire_task succeeds idempotently for a caller that ALREADY holds
        # the key, so releasing on a later refusal would drop a claim this
        # call never took and leave a live worker unclaimed.
        from fno.claims.core import claim_status as _claim_status

        try:
            _before = _claim_status(key)
            held_before = _before.get("holder") == holder and _before.get("state") in ("live", "suspect")
        except Exception:  # noqa: BLE001 - an unreadable claim is not a held one
            held_before = False

        def _release_if_taken() -> None:
            if not held_before:
                release_task(node_id, task_id, holder)

        try:
            acquire_task(node_id, task_id, holder, pid=pid, harness=harness)
        except ClaimHeldByOther as exc:
            typer.echo(
                f"another holder owns {key} ({exc.holder}, pid={exc.pid})", err=True
            )
            raise typer.Exit(code=3)
        except ClaimContended as exc:
            # Recovery-mutex contention, not a live holder: skip this round
            # like a held task; a later pass re-runs the same command.
            typer.echo(f"task claim contention on {key}: {exc}", err=True)
            raise typer.Exit(code=3)
        except ClaimValidationError as exc:
            typer.echo(f"invalid task claim key {key}: {exc}", err=True)
            raise typer.Exit(code=2)
        claimed_at = datetime.now(timezone.utc).isoformat()
        reopen_refused: list[str] = []

        def _claim_row(row) -> None:
            # A done row is shipped work; re-opening it from stale per-checkout
            # state re-dispatches a finished task, so the transition refuses.
            if row.get("status") == "done":
                reopen_refused.append(str(row.get("id")))
                return
            row.update(
                {"status": "in_progress", "owner": holder, "claimed_at": claimed_at}
            )

        try:
            row = _set_row(_claim_row)
        except (Exception, SystemExit):
            # SystemExit too: locked_mutate_graph exits (does not raise) on a
            # corrupt graph, and an exited write must release like a raised one
            # or the claim outlives the failed transition held by the session
            # pid.
            _release_if_taken()
            raise
        if reopen_refused:
            _release_if_taken()
            typer.echo(
                f"task {task_id} is done; re-offering shipped work is refused",
                err=True,
            )
            raise typer.Exit(code=3)
        if row is None:
            _release_if_taken()
            typer.echo(f"node {node_id} or task {task_id} vanished at write time", err=True)
            raise typer.Exit(code=1)
        typer.echo(f"{key} in_progress holder={holder}")
        return

    if status == "done":
        # The row write is holder-guarded like the give-back: a non-holder
        # marking a live task done leaves the peer working a task the board
        # reports finished. An UNowned row (never claimed) accepts done from
        # anyone - there is no in-flight worker to contradict. --takeover
        # relaxes the owner match for a GONE holder; the live-claim guard
        # above already proved nothing live is holding it.
        done_refused: list[str] = []

        def _done_row(row) -> None:
            owner_now = row.get("owner")
            if owner_now and owner_now != holder and not takeover:
                done_refused.append(str(owner_now))
                return
            row.update({"status": "done"})

        row = _set_row(_done_row)
        if done_refused:
            # The owner check has no liveness test, so a reaped or handed-off
            # holder would wedge the row at in_progress forever. Name the
            # escape here, where the caller reads the refusal.
            typer.echo(
                f"task {task_id} held by {done_refused[0]}; only the holder "
                "can mark it done. If that holder is gone, re-run with "
                "--takeover",
                err=True,
            )
            raise typer.Exit(code=3)
        if row is None:
            typer.echo(f"node {node_id} or task {task_id} vanished at write time", err=True)
            raise typer.Exit(code=1)
        _release_claim_or_note()
        typer.echo(f"{key} done")
        return

    # pending: the holder-only give-back. The owner check runs INSIDE the
    # locked mutation: a check on a pre-read row could clobber a row a peer
    # claimed between the read and the write, advertising a live task as free.
    # --takeover relaxes the owner match for a GONE holder; the live-claim
    # guard above already proved nothing live is holding it.
    refused_owner: list[str] = []
    giveback_done_refused: list[str] = []

    def _give_back(row) -> None:
        # `done` leaves `owner` set, so the holder's own later `blocked` emit
        # passes the owner check and reopens shipped work; an unowned done row
        # would accept a give-back from anyone. Refuse both, as _claim_row does.
        if row.get("status") == "done":
            giveback_done_refused.append(str(row.get("id")))
            return
        owner_now = row.get("owner")
        if owner_now and owner_now != holder and not takeover:
            refused_owner.append(str(owner_now))
            return
        row.update({"status": "pending", "owner": None})
        row.pop("claimed_at", None)

    row = _set_row(_give_back)
    if giveback_done_refused:
        typer.echo(
            f"task {task_id} is done; giving shipped work back is refused",
            err=True,
        )
        raise typer.Exit(code=3)
    if refused_owner:
        typer.echo(
            f"task {task_id} held by {refused_owner[0]}; "
            "only the holder can give it back. If that holder is gone, "
            "re-run with --takeover",
            err=True,
        )
        raise typer.Exit(code=3)
    if row is None:
        typer.echo(f"node {node_id} or task {task_id} vanished at write time", err=True)
        raise typer.Exit(code=1)
    release_task(node_id, task_id, holder)
    typer.echo(f"{key} pending (given back by {holder})")


cli.add_typer(task_app, name="task", hidden=True)


# -- backfill-slugs --

# -- view --


@cli.command("view")
def cmd_view() -> None:
    """Render the backlog as HTML and open it with the system's default handler.

    Always rerenders before opening so the file reflects current graph.json
    state even if the auto-render hook hasn't fired since the last edit. The
    file lives at ``~/.fno/pages/graph.html`` and is opened via ``open`` on
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

    # The board is the native front binary's snapshot render: it re-gathers
    # the store itself and writes every configured local target, the
    # canonical board among them.
    from fno.graph.roadmap_public import render_local_targets

    failures = render_local_targets()
    if failures:
        typer.echo(
            f"Error: local board render failed ({failures} target(s)); "
            "see the warnings above",
            err=True,
        )
        raise typer.Exit(code=1)
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
        help="Directory to emit the .base files into (default: the capture-inbox dir).",
    ),
) -> None:
    """Emit the canonical epic/mission progress Base files ().

    Regenerable: refreshes a file carrying the generated marker, refuses to
    clobber a hand-authored base (one prints `refused:`). Prints one line per
    file: written | unchanged | refused.
    """
    from fno.graph._bases import BASES, write_base
    from fno.paths import inbox_path

    out_dir = Path(out) if out else inbox_path().parent
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
    out: Optional[str] = typer.Option(
        None, "--out", help="Write markdown to this path instead of stdout."
    ),
    backlog_html: Optional[str] = typer.Option(
        None,
        "--backlog-html",
        help=(
            "Also write the public open-work HTML board (leak-gated, native "
            "renderer) to this path."
        ),
    ),
) -> None:
    """Render public roadmap and backlog projections through one leak gate.

    Qualifying nodes are public unless explicitly marked ``public: false``.
    Titles must clear the shared leak gate before any public output is written.
    """
    from pathlib import Path

    from fno.graph._intake import detect_project_from_settings, repo_root
    from fno.graph.roadmap_public import (
        atomic_write_documents,
        load_render_entries,
        omit_leaky_rows,
        render_public_backlog_html,
        render_public_roadmap_md,
    )

    resolved_project = project or detect_project_from_settings(repo_root())
    if not resolved_project:
        typer.echo(
            "Error: no project given and none mapped to the cwd; pass --project.",
            err=True,
        )
        raise typer.Exit(code=1)

    try:
        entries = load_render_entries(_display_entries("roadmap", strict=True))
    except typer.Exit:
        raise
    except Exception as exc:
        typer.echo(f"Error: canonical graph read failed: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    entries, _ = omit_leaky_rows(entries, resolved_project)

    if backlog_html:
        if not render_public_backlog_html(resolved_project, backlog_html):
            typer.echo("Error: public backlog HTML render failed; see the warning above", err=True)
            raise typer.Exit(code=1)

    md = render_public_roadmap_md(entries, resolved_project)

    documents: dict[Path, str] = {}
    out_path = Path(os.path.expanduser(out)) if out else None
    if out_path:
        documents[out_path] = md
    atomic_write_documents(documents)

    if out_path:
        typer.echo(str(out_path))
    else:
        typer.echo(md, nl=False)


# -- tree --

# -- status --


@cli.command("status", hidden=True)
def cmd_status(
    project: Optional[str] = typer.Option(None, help="Filter by project"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
) -> None:
    from fno.graph._intake import detect_project

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
                typer.echo(
                    f"Roadmap: {roadmaps[0].get('roadmap_id', '?')} ({roadmaps[0].get('title', '?')})"
                )
            ideas_suffix = (
                f" | ideas: {ideas} (use 'fno backlog ready --ideas' to list)" if ideas else ""
            )
            # Active-most → inactive-most ordering: done | claimed | ready
            # | ideas | blocked | deferred. Deferred is the only state that
            # requires an explicit `--include-deferred` to re-surface, so it
            # belongs at the tail.
            deferred_suffix = (
                f" | deferred: {deferred} (use 'fno backlog ready --include-deferred' to list)"
                if deferred
                else ""
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
    from fno.graph.store import commit_rows_via_store

    session = merge_deprecated_alias(
        session, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    # --session-id is required; the merge returns None only when NEITHER
    # spelling was passed (the hidden alias forces a None default here).
    if session is None:
        raise click.UsageError("Missing option '--session-id'.")

    _require_node_id(task_id)

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

    commit_rows_via_store(_graph_path(), mutator)
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
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.graph.store import commit_rows_via_store
    from fno.graph._intake import _find_node, _find_dependents

    _require_node_id(task_id)

    entries = wire_rows(path=_graph_path())
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
        # Same invariant for containment (), and here a dangling pointer
        # is a permanent trap rather than mere untidiness: the reconcile heal
        # deliberately skips a MISSING owner, so nothing would ever free them.
        _freed_box[0] = _release_contained_children(entries, task_id)
        return [e for e in entries if e.get("id") != task_id]

    commit_rows_via_store(_graph_path(), mutator)
    _echo_freed(_freed_box[0], task_id)
    typer.echo(f"Removed {task_id}" + (f" (orphaned deps in {dependents})" if dependents else ""))


    # Rationale (10 lines): docs/architecture/graph-cli-rationale.md#cmd-remove-6902


def _expand_valid_ids(task_ids: list[str]) -> list[str]:
    """Expand one-or-many id args; refuse an empty set and non-node ids
    (the shared prologue of every batch-mutating verb)."""

    ids = _expand_id_args(task_ids)
    if not ids:
        typer.echo("Error: at least one task_id is required", err=True)
        raise typer.Exit(code=1)
    for tid in ids:
        _require_node_id(tid)
    return ids


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
    # The queue read is native: the door (fno backlog queued) serves the
    # read, the project filter, and the sort. A direct wheel spelling has no
    # leg left to run, so it names the door instead of carrying a second
    # implementation.
    typer.echo(
        "Error: the queue read is served by the native door; run `fno backlog queued`.",
        err=True,
    )
    raise typer.Exit(code=2)


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
        "--reason",
        "-R",
        help="Why these nodes are being queued (applies to all). Free text, surfaced on the card.",
    ),
) -> None:
    """Queue one or more backlog nodes for action. Sets ``queued_at`` + optional ``queued_reason``.

    Atomic across the batch: if any ID is unknown, none of the nodes
    are queued. Same reason applies to every ID in the batch.
    """
    # The queue write is native: the door (fno backlog queue) owns the
    # batch stamp and its atomicity. A direct wheel spelling has no leg left
    # to run, so it names the door instead of carrying a second
    # implementation.
    typer.echo(
        "Error: the queue write is served by the native door; "
        "run `fno backlog queue <ids>`.",
        err=True,
    )
    raise typer.Exit(code=2)


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
    # The unqueue write is native: the door (fno backlog unqueue) owns the
    # batch clear and its atomicity. A direct wheel spelling has no leg left
    # to run, so it names the door instead of carrying a second
    # implementation.
    typer.echo(
        "Error: the unqueue write is served by the native door; "
        "run `fno backlog unqueue <ids>`.",
        err=True,
    )
    raise typer.Exit(code=2)


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
    all_: bool = typer.Option(
        False, "--all", "-A", help="Show all projects (default: current cwd)"
    ),
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
        "--reason",
        "-R",
        help="Reason applied to every newly-queued node (optional).",
    ),
) -> None:
    """Interactively manage the backlog queue via fzf with live marker updates.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    import os as _os
    import platform
    import shlex
    import shutil
    import subprocess
    import tempfile

    from fno.graph.store import commit_rows_via_store
    from fno.graph._intake import filter_by_project, _find_node, _graph_sort_key_fn
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.roadmap_public import _load_obsidian_vault, obsidian_url as _obsidian_url

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

    entries = wire_rows(path=_graph_path())
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
    candidates.sort(key=lambda e: (0 if e["id"] in currently_queued else 1, _graph_sort_key_fn(e)))

    vault = _load_obsidian_vault()
    open_cmd = (
        "open"
        if platform.system() == "Darwin"
        else (shutil.which("xdg-open") or shutil.which("wslview") or "xdg-open")
    )

    # Tempfiles:
    #   cands.tsv: the immutable snapshot of candidates the picker reads
    #   pending.txt: empty file the keybinds append intents to
    #   awk.script: the renderer logic invoked by fzf reload
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
                blockers = ",".join(b for b in (e.get("blocked_by") or []) if isinstance(b, str))
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
            header_lines.append("(set config.obsidian.vault in settings.yaml to enable 'o' opener)")
        header = "\n".join(header_lines)

        # Initial row set: run the renderer once with empty pending.
        initial = subprocess.run(
            [awk_bin, "-f", awk_path, pend_path, cand_path],
            capture_output=True,
            text=True,
            check=False,
        ).stdout

        proc = subprocess.run(
            [
                fzf,
                "--no-multi",
                "--delimiter",
                "\t",
                "--with-nth",
                "3..",
                "--nth",
                "3..",
                # Q/U/T keybinds: append intent line to pending.txt, then
                # reload the row list from awk. Cursor preserves via fzf's
                # default reload behavior; +down advances to next row.
                "--bind",
                f"q:execute-silent(printf 'Q %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind",
                f"u:execute-silent(printf 'U %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind",
                f"space:execute-silent(printf 'T %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind",
                "enter:accept",
                "--bind",
                f'o:execute-silent(u={{1}}; [[ -n "$u" ]] && {open_cmd} "$u")',
                "--header",
                header,
                "--prompt",
                "pick> ",
                "--height",
                "85%",
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

    commit_rows_via_store(_graph_path(), mutator)
    queued_applied = results["queued_applied"]
    unqueued_applied = results["unqueued_applied"]

    suffix = f': "{cleaned_reason}"' if cleaned_reason else ""
    for tid in queued_applied:
        typer.echo(f"Queued {tid}{suffix}")
    for tid in unqueued_applied:
        typer.echo(f"Unqueued {tid}")
    if queued_applied or unqueued_applied:
        typer.echo(f"({len(queued_applied)} queued, {len(unqueued_applied)} unqueued)")
    else:
        typer.echo("(no changes)")


@cli.command(
    "contain",
    hidden=True,
    epilog="Inverse: `fno backlog update <id> --parent null` un-contains a node - a verb the native binary serves after the python update leg retired.",
)
def cmd_contain(
    ctx: typer.Context,
    owner: str = typer.Argument(..., help="The owning node: contained nodes ship inside its PR."),
    task_ids: List[str] = typer.Argument(
        ...,
        help="Node IDs to fold (ab-XXXXXXXX). Multiple via space and/or comma.",
    ),
) -> None:
    """Fold existing nodes into an owner: they ship inside its PR. No plan needed.

    Stamps contained_in + parent in one locked mutation. Atomic across the
    batch: any refusal stamps nothing. A deferred target is accepted (contain
    first, undefer second, so the node is never armed in between). Containment
    is released by the owner's merge cascade or by moving the node away.
    """
    # The containment write is native: the door (fno backlog contain) owns the
    # guard ladder, the stamps, and the receipts. A direct wheel spelling has
    # no leg left to run, so it names the door instead of carrying a second
    # implementation.
    typer.echo(
        "Error: the containment write is served by the native door; "
        "run `fno backlog contain <owner> <ids>`.",
        err=True,
    )
    raise typer.Exit(code=2)


# -- backfill-deferred-kind --


def _load_deferred_kind_map(map_file) -> dict[str, str]:
    """Parse + validate the operator TSV: ``kind<TAB>exact reason`` per row.

    ``#`` comments and blank lines are ignored. An unknown kind or a
    malformed row is a hard error BEFORE any graph read: a typo'd map must
    never half-apply. Duplicate reasons with conflicting kinds are refused;
    a duplicate with the same kind is idempotent-tolerated.
    """
    from fno.graph._constants import DEFERRED_KINDS

    mapping: dict[str, str] = {}
    for lineno, raw in enumerate(map_file.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.rstrip("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if "\t" not in line:
            raise ValueError(f"{map_file}:{lineno}: expected 'kind<TAB>reason', got no tab")
        kind, reason = line.split("\t", 1)
        kind, reason = kind.strip(), reason.strip()
        if kind not in DEFERRED_KINDS:
            raise ValueError(
                f"{map_file}:{lineno}: unknown kind '{kind}' "
                f"(valid: {', '.join(DEFERRED_KINDS)})"
            )
        if not reason:
            raise ValueError(f"{map_file}:{lineno}: empty reason")
        if mapping.get(reason, kind) != kind:
            raise ValueError(f"{map_file}:{lineno}: reason maps to two kinds ({reason[:60]!r})")
        mapping[reason] = kind
    return mapping


@cli.command("backfill-deferred-kind", hidden=True)
def cmd_backfill_deferred_kind(
    map_file: Optional[Path] = typer.Option(
        None,
        "--map",
        help="TSV file: kind<TAB>exact deferred_reason per row. Merged over the code table.",
    ),
    apply: bool = typer.Option(
        False,
        "--apply",
        help="Write the classification. Default is a dry-run report.",
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON report"),
) -> None:
    """Classify deferred rows by EXACT reason match. Never closes or undefers.

    Mechanical: a node is stamped only when its ``deferred_reason`` equals a
    known exact string (code table + --map). Anything else stays
    unclassified - an honest unknown beats a wrong label, because a wrong
    kind silently changes whether an epic can close. Existing kinds are never
    overwritten; non-deferred nodes are never touched.
    """
    from fno.graph._constants import classify_deferred_reason
    from fno.graph.store import commit_rows_via_store

    extra_map: dict[str, str] = {}
    if map_file is not None:
        try:
            extra_map = _load_deferred_kind_map(map_file)
        except ValueError as exc:
            typer.echo(f"Error: {exc}", err=True)
            raise typer.Exit(code=1)

    def classify(node) -> str | None:
        return classify_deferred_reason(node.get("deferred_reason"), extra_map)

    if not apply:
        entries = wire_rows(path=_graph_path())
        would: dict[str, int] = {}
        unclassified = 0
        for e in entries:
            if e.get("status") != "deferred" or e.get("deferred_kind"):
                continue
            kind = classify(e)
            if kind:
                would[kind] = would.get(kind, 0) + 1
            else:
                unclassified += 1
        if json_output:
            typer.echo(json.dumps({"dry_run": True, "would_stamp": would, "unclassified": unclassified}))
        else:
            for kind in sorted(would):
                typer.echo(f"  {kind}: {would[kind]}")
            typer.echo(f"  unclassified (stays unknown): {unclassified}")
            typer.echo("  dry run; pass --apply to write")
        return

    counts: dict[str, int] = {}

    def mutator(entries):
        for e in entries:
            if e.get("status") != "deferred" or e.get("deferred_kind"):
                continue
            kind = classify(e)
            if kind:
                e["deferred_kind"] = kind
                counts[kind] = counts.get(kind, 0) + 1
        return entries

    commit_rows_via_store(_graph_path(), mutator)
    if json_output:
        typer.echo(json.dumps({"applied": True, "stamped": counts}))
    else:
        for kind in sorted(counts):
            typer.echo(f"  stamped {kind}: {counts[kind]}")
        typer.echo(
            f"  total stamped: {sum(counts.values())} "
            f"(existing kinds preserved; unclassified left unclassified)"
        )


# -- stuck-epics --


@cli.command("stuck-epics", hidden=True)
def cmd_stuck_epics(
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON report"),
) -> None:
    """Epics whose only incomplete children are deferred/superseded.

    Read-only surface for the operator: it NEVER closes, undefers, or
    re-parents anything. A stuck epic is closable when no child holds it
    open (graph/epics.holds_epic_open): done and superseded children never
    do, and a wont_do deferral is a decision, not a delay. Everything else
    (an unclassified or contingent deferral) holds the epic open and needs a
    human ruling.
    """
    from fno.graph.epics import stuck_epics

    entries = wire_rows(path=_graph_path())
    rows = stuck_epics(entries)
    if json_output:
        typer.echo(json.dumps({"stuck_epics": rows}, default=lambda o: o.__dict__))
        return
    if not rows:
        typer.echo("no stuck epics")
        return
    for row in rows:
        verdict = "closable" if row.closable else f"held open by {row.held_open_by}"
        typer.echo(f"  {row.id} [{row.status}] {row.title} - {verdict}")
        for h in row.holders:
            typer.echo(f"      {h['id']} [{h['status']}"
                       f"{', ' + h['deferred_kind'] if h.get('deferred_kind') else ''}]")
    typer.echo(
        "  read-only: closing or undefering any of these is an operator ruling, never automatic"
    )


# -- done --


def _project_plans_from_graph(
    node_ids: list[str],
    *,
    mirror_keys_for: tuple[str, frozenset[str]] | None = None,
    force_status_off_terminal_for: str | None = None,
    clear_keys_for: tuple[str, frozenset[str]] | None = None,
) -> None:
    """Project each named node's mirror fields + forward status onto its plan.

    Re-reads the graph so every node carries its recomputed ``status``, then
    delegates to the shared converger. Covers cascade-closed epic parents that
    ``_stamp_and_graduate_plan`` never stamps. Best-effort per node: a missing
    or unreadable plan never fails the mutation.

    ``mirror_keys_for`` pairs the ONE node whose extra keys (``type``,
    ``difficulty``) may be written with those keys, set only where the
    operator supplied the value. It is an id, not a flag: this projection
    repaints ancestors and siblings too, and their values are defaults.
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
        entries,
        ids,
        mirror_keys_for=mirror_keys_for,
        force_status_off_terminal_for=force_status_off_terminal_for,
        clear_keys_for=clear_keys_for,
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
    # Done dominates deferred per the cascade. Clear any deferred/queued state
    # so the row presents as cleanly done with no ghost fields.
    node["deferred_at"] = None
    node["deferred_reason"] = None
    node.pop("deferred_kind", None)
    node["queued_at"] = None
    node["queued_reason"] = None
    node["completed_at"] = datetime.now(timezone.utc).isoformat()
    if merge_status is not None:
        node["merge_status"] = merge_status


def _auto_closed_note(entry: dict) -> str:
    """completion_note for a container closed by all-children-complete ().

    Both close paths (_cascade_close_parents, _sweep_close_done_epics) reach
    here holding the parent dict. A container with its own plan_path but no
    PR may carry deliverables the children never built; the cascade closes it
    anyway, so an untracked false-done needs a flag. When plan_path is set and
    pr_number is not, the note marks the deliverables UNVERIFIED - not that
    they are missing (a filesystem stat measured 75% false positives from
    renames and path conventions), only that no PR proves them. A flag, not a
    gate: the close still happens, it just becomes findable.
    """
    if entry.get("plan_path") and not entry.get("pr_number"):
        return "auto-closed: all children complete; own plan deliverables UNVERIFIED (plan_path set, no PR)"
    return "auto-closed: all children complete"


def _cascade_close_parents(entries: list[dict], node_id: str) -> list[str]:
    """Close ancestor epics whose children are now all complete .
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.graph._reconcile import cascade_close_should_stop
    id_to_entry = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
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
        if parent is None:
            break  # missing ancestor -> stop this branch
        kids = children_by_parent.get(pid) or []
        if cascade_close_should_stop(parent, kids, cur):
            break  # already closed, or a child still open -> stop this branch
        _apply_completion_fields(parent)
        if not parent.get("completion_note"):
            parent["completion_note"] = _auto_closed_note(parent)
        # Deactivate the mission (K1): a kicked-off epic carries
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


# In graph/strand.py: the terminal-parent strand family (moved with the
# close guards, release twins, and self-heal that share its liveness predicate).
from fno.graph.strand import (  # noqa: E402
    _release_contained_children,
    _stranded_next_receipts,
)

# In graph/selection_evidence.py: the occupancy one `backlog next` selection
# reads, and the receipts that explain what it passed over.
from fno.graph.selection_evidence import (  # noqa: E402
    OccupancyUnavailable,
    _starvation_receipts,
    read_occupancy,
)

# In graph/_closures.py: this file is over the source budget; these four ride
# this module's namespace for the tests that import them from here.
from fno.graph._closures import (  # noqa: E402, F401
    _cascade_close_contained as _cascade_close_contained,
    _strandable_contained_ids as _strandable_contained_ids,
    _strandable_epic_ids as _strandable_epic_ids,
    _sweep_close_done_epics as _sweep_close_done_epics,
)


def _status_drift(path: Path) -> dict[str, tuple[str, str]]:
    import copy

    from fno.graph.statuses import recompute_statuses
    from fno.graph.store import _read_json, read_graph_strict

    persisted: dict[str, str] = {}
    for entry in _read_json(path):
        node_id = entry.get("id") if isinstance(entry, dict) else None
        status = entry.get("status") if isinstance(entry, dict) else None
        if isinstance(node_id, str) and isinstance(status, str) and not (status == "blocked" and entry.get("blocked_by")):
            persisted[node_id] = status

    derived: dict[str, str] = {}
    for entry in recompute_statuses(copy.deepcopy(read_graph_strict(path))):
        node_id = entry.get("id") if isinstance(entry, dict) else None
        status = entry.get("status") if isinstance(entry, dict) else None
        if isinstance(node_id, str) and isinstance(status, str):
            derived[node_id] = status
    return {
        node_id: (persisted[node_id], derived[node_id])
        for node_id in sorted(persisted.keys() & derived.keys())
        if persisted[node_id] != derived[node_id]
    }


def _stamp_and_graduate_plan(
    plan_path: str,
    *,
    url: Optional[str] = None,
    session_id: Optional[str] = None,
) -> bool:
    """Best-effort: stamp a plan ``shipped`` (when a ship URL is known) then graduate.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.plan._project import plan_docs

    stamped_shipped = False
    if url:
        sid = session_id or "backlog-close"
        res = plan_docs("stamp", plan_path=plan_path, session_id=sid, urls=[url])
        if res is None or res["exit"]:
            return False
        stamped_shipped = True

    res = plan_docs("graduate", plan_path=plan_path)
    if res is None or res["exit"]:
        # A successful stamp already recorded the ship; report that win even if
        # the graduate spawn failed.
        return stamped_shipped
    return True


# Closed set of outcomes from _set_expected_count, so the call site's
# `status == "failed"` compare is type-checked rather than a free-form string.
SetExpectedStatus = Literal["ok", "skipped", "failed"]


def _set_expected_count(plan_path: str, count: int) -> tuple[SetExpectedStatus, str]:
    """Authoritatively write expected_url_count=count onto a plan's frontmatter.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.plan._project import plan_docs

    res = plan_docs("set_expected", plan_path=plan_path, count=count)
    return (res["status"], res["message"].strip()) if res else ("failed", "graph store unavailable")


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
        evidence = resolve_merge_evidence(refs, cwd=node.get("cwd"), query=_done_gh_query)
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
        # is recorded even when the cross-check was bypassed ().
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

    # -- Step 3b: promise gate () --
    # The merge gate asked "is a PR merged"; this asks "did the plan's declared
    # work all ship". Skipped under --force so a deliberate half-ship stays a
    # journaled line (the backlog_done_forced event above) rather than silence.
    if not force:
        promise = resolve_promise_evidence(node, cwd=node.get("cwd"), query=_done_gh_query)
        if not promise.satisfied:
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


def _done_via_seam(task_id: str, *, skip_stamp: bool, force: bool, reason: Optional[str]) -> None:
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
        "id": task_id,
        "title": tnode.title,
        "cwd": sc.cwd,
        "plan_path": sc.plan_path,
        "pr_number": sc.pr_number,
        "pr_url": sc.pr_url,
        "additional_prs": sc.additional_prs,
        "sessions": sc.sessions,
        # The rollup reads containment off the node (a contained child claims
        # no cost); dropping it here would double-count the delivery unit.
        "contained_in": sc.contained_in,
    }
    refs = node_pr_refs(row)
    evidence_pr_url = _done_gate_pipeline(task_id, row, refs, force=force, reason=reason)

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

    # Retro-at-done lifecycle trigger (): same non-fatal posture as the
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
    "whose PR merged outside the gate (hidden). Correction is reopen, a verb "
    "the native binary serves after the python leg retired.",
)
def cmd_done(
    task_id: Optional[str] = typer.Argument(
        None,
        help="Feature ID (ab-XXXXXXXX), title substring, or omit to auto-detect from the git branch.",
    ),
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
    pr: Optional[int] = typer.Option(
        None, "--pr-number", "--pr", "-p", help="PR number (for code-domain completions)."
    ),
    pr_url: Optional[str] = typer.Option(
        None,
        "--pr-url",
        help="PR URL. Derived from the repo when omitted; supply it when the repo slug cannot be resolved.",
    ),
    repo: Optional[str] = typer.Option(
        None,
        "--repo",
        help=(
            "owner/name of the repo the PR lives in, when that differs from the "
            "cwd checkout. Naming the repo makes the stamp an assertion rather "
            "than a cwd derivation, which is also what lets it override a "
            "recorded pr_url naming a different repo ()."
        ),
    ),
    link: Optional[str] = typer.Option(
        None,
        "--link",
        "--url",
        "-l",
        help="Artifact URL (Figma/Canva/Obsidian/any) - sets artifact_url.",
    ),
    note: Optional[str] = typer.Option(
        None, "--note", "-m", help="Free-text completion note - sets completion_note."
    ),
    backfill: bool = typer.Option(
        False,
        "--backfill",
        help=(
            "Run ONLY the ledger-rollup (session_id, cost_usd, cost_sessions, "
            "points). Does not flip status or completed_at. With no TASK_ID, "
            "sweeps every node with status=done."
        ),
    ),
    force_overwrite: bool = typer.Option(
        False,
        "--force-overwrite",
        help="Overwrite existing rollup fields instead of fill-if-null. Use with --backfill for explicit re-reconciliation of stale rollups.",
    ),
) -> None:
    """Mark a node complete.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.graph._constants import has_node_id_prefix

    # The close flags and the completion flags never mixed on either legacy
    # spelling, and the delegation below would silently drop --force. Refuse
    # the combination by name rather than letting a deliberate half-ship
    # request run ungated.
    _close_flags = force or reason is not None or skip_stamp
    _rich_flags = (
        backfill
        or force_overwrite
        or pr is not None
        or pr_url is not None
        or repo is not None
        or link is not None
        or note is not None
    )
    if _close_flags and _rich_flags:
        typer.echo(
            "Error: the close flags (--force/--reason/--skip-stamp) and the "
            "completion flags (--pr/--pr-url/--link/--note/--backfill/"
            "--force-overwrite) are separate paths. A deliberate half-ship "
            "closes on the id alone: `fno backlog done <id> --force --reason ...`",
            err=True,
        )
        raise typer.Exit(code=2)

    # Rationale (8 lines): docs/architecture/graph-cli-rationale.md#cmd-done-8491
    if (
        not _close_flags
        and not _rich_flags
        and task_id is not None
        and not has_node_id_prefix(task_id)
    ):
        from fno.tracker import active_backend_name

        if active_backend_name() == "graph":
            from fno.done.cli import _current_branch
            from fno.graph.fuzzy import resolve_id

            _match = resolve_id(
                task_id,
                wire_rows(path=_graph_path()),
                git_branch=_current_branch(),
            )
            if _match.kind in ("exact", "fuzzy", "branch_derived"):
                task_id = _match.id
            # none/ambiguous fall through to the delegate for its messages.

    # The rich surface delegates (): any ported flag or a missing id
    # (branch auto-detect) routes to the implementation that already owns
    # those paths - and only when NO close flag is set, because the delegate
    # has no --force/--reason/--skip-stamp and would drop them. A node id
    # (given or resolved above) keeps THIS command's gates, stamp, and
    # dependent cascade - the canonical close.
    _delegate = not _close_flags and (
        _rich_flags or task_id is None or not has_node_id_prefix(task_id)
    )
    if _delegate:
        from fno.done.cli import done_command

        done_command(
            query=task_id,
            pr=pr,
            pr_url=pr_url,
            repo=repo,
            link=link,
            note=note,
            backfill=backfill,
            force_overwrite=force_overwrite,
        )
        return
    if task_id is None:
        typer.echo(
            "Error: an explicit node id is required with the close flags "
            "(--force/--reason/--skip-stamp); branch auto-detect applies to "
            "the completion surface only",
            err=True,
        )
        raise typer.Exit(code=2)
    # External backend (task 4.1): the shared gates then exactly one
    # tracker.close. The local <prefix>-<hex> grammar guard does not apply to
    # an opaque external id - resolution is the tracker's exact read.
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        _done_via_seam(task_id, skip_stamp=skip_stamp, force=force, reason=reason)
        return

    # The graph-backend close is native: the door (fno backlog done) serves
    # the close-flag surface, the receipts, and the dependent cascade. A
    # direct wheel-level spelling has no leg left to run, so it names the
    # door instead of carrying a second implementation.
    typer.echo(
        "Error: the graph-backend close is served by the native door; "
        "run `fno backlog done " + (task_id or "<id>") + "`.",
        err=True,
    )
    raise typer.Exit(code=2)


def _canonical_post_close(
    node: dict,
    *,
    task_id: str,
    cascade_closed: list,
    skip_stamp: bool,
    evidence_pr_url: Optional[str],
) -> None:
    """The canonical post-close steps, shared by every close path (PR 1200).

    Stamp+graduate the plan, project the closed node and any cascade-closed
    epic parents, and fire the retro trigger. The rich completion surface
    (fno.done.cli) runs the SAME steps so closing depth never forks on which
    flags or spelling named the node.
    """
    plan_path = node.get("plan_path")
    if plan_path and not skip_stamp:
        # Stamp the plan shipped (against the evidencing PR) THEN graduate, so a
        # plan that never went through target's ship gate still records the ship
        # rather than getting a graduate no-op ().
        _stamp_and_graduate_plan(
            plan_path,
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
        _project_plans_from_graph([task_id, *cascade_closed])

    # A2 (): retro-at-done lifecycle trigger. Dispatch a `retro` context
    # /think while the closed node's session context is still resolvable. Gated
    # by config.think_spawn.on_retro (default OFF) and strictly non-fatal: a
    # dispatch failure never unwinds the close it rode in on.
    try:
        from fno.provenance.spawn_think import on_node_retro

        on_node_retro(node)
    except Exception:  # noqa: BLE001 - additive; never wedge `done`
        pass


# -- reconcile (close merged-PR drift) --
# `done` and `reopen` are a deliberate gate inversion (`done` refuses when no
# referenced PR is merged, `reopen` refuses when one IS); both verbs are
# native now, owned by the door's lifecycle module.


@cli.command("advance", hidden=True)
def cmd_advance(
    closed: Optional[str] = typer.Option(
        None,
        "--closed",
        help="The just-merged node id whose close triggered this advance (AC1-RACE keying).",
    ),
    epic: Optional[str] = typer.Option(
        None, "--epic", help="Advance (converge) an epic mission: fan out its ready leaf children across all projects ( K1). Mutually exclusive with --closed.",
    ),
    stop: bool = typer.Option(
        False, "--stop", help="With --epic: deactivate the mission (clear mission_active) and dispatch nothing.",
    ),
    loose: bool = typer.Option(False, "--loose", help="With --project: drain the territory's loose ready nodes."),
    continuation: bool = typer.Option(
        False, "--continuation", hidden=True,
        help="With --epic: K2 daemon-drain mode - never (re)activate the mission; retire an already-inactive one (dispatches nothing, reports deactivated).",
    ),
    max_dispatch: Optional[int] = typer.Option(
        None, "--max", help="With --epic: cap the total workers this epic advance dispatches (width derives from spawn-gate headroom).",
    ),
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Restrict next-node selection to this project."),
    explain: bool = typer.Option(
        False,
        "--explain",
        help=(
            "Dry run: report why this node and not another - the refusing gate "
            "or cap with its measured value, and the grid's resolve. Dispatches "
            "nothing, claims nothing, ignores config.auto_continue."
        ),
    ),
    explain_node: Optional[str] = typer.Option(
        None,
        "--explain-node",
        help="With --explain: answer for THIS node - the filter that dropped it, or its rank.",
    ),
    explain_top: int = typer.Option(
        5, "--explain-top", help="With --explain: how many ranked candidates to show."
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the decision as JSON."),
    verbose: bool = typer.Option(False, "--verbose", help="Print the dispatch decision to stderr."),
    model: Optional[str] = typer.Option(
        None, "--model", "-m", help="Pin a model for the dispatched worker(s), overriding node annotations.",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider", help="Pin a provider for the dispatched worker(s). (No -p short: it is --project here.)",
    ),
    source: Optional[str] = typer.Option(
        None,
        "--source",
        help="Dispatch origin for the worker name: ab daemon, ac merge continuation. Omit when attended.",
    ),
) -> None:
    """Dispatch a fresh /target --no-merge worker for the next now-unblocked node.

    Merge-triggered auto-continue (). Opt-in and non-fatal; driven by the
    merge event (reconcile / post-merge). Always exits 0 (a dispatch decision
    is never an error to the host op). ``--epic <id>`` switches to the epic
    advance / converge path ( K1): mark the mission active and fan out every
    ready LEAF child across all projects; ``--stop`` deactivates instead.
    """
    from fno.dispatch_flags import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_harness,
    )
    from fno.backlog.advance import advance as _advance
    from fno.backlog.advance import advance_dependents as _advance_deps

    _refuse_unknown_source("advance", source)  # refuse, never default.

    # --explain returns BEFORE every dispatch path, including the pin validation
    # below: it is a read, so an unparseable --model must not stop it from
    # reporting why a node did not launch. A graph read that fails exits
    # non-zero naming the read rather than printing a partial verdict.
    if explain:
        from fno.backlog.explain import build_report, render_report

        # --explain --epic models the DAEMON's drain (the --epic fan-out through the
        # converge gates), never the next cascade whose answer is the second-selector lie.
        if epic is not None:
            from fno.backlog.explain import build_lane_fill_report, render_lane_fill_report

            try:
                report = build_lane_fill_report(
                    epic=epic, project=project, node_id=explain_node, top=explain_top,
                    max_dispatch=max_dispatch, provider=provider, model=model,
                )
            except Exception as exc:  # noqa: BLE001 - never a partial verdict
                typer.echo(f"advance --explain --epic: {exc}", err=True)
                raise typer.Exit(code=1)
            typer.echo(
                json.dumps(report, indent=2, default=str)
                if json_out
                else render_lane_fill_report(report)
            )
            return
        try:
            report = build_report(
                project=project, node_id=explain_node, top=explain_top
            )
        except Exception as exc:  # noqa: BLE001 - never a partial verdict
            typer.echo(f"advance --explain: {exc}", err=True)
            raise typer.Exit(code=1)
        typer.echo(json.dumps(report, indent=2, default=str) if json_out else render_report(report))
        return
    if explain_node is not None or explain_top != 5:
        typer.echo("advance: --explain-node / --explain-top require --explain", err=True)
        raise typer.Exit(code=2)

    refuse_if_paused(json_out=json_out)
    # Validate dispatch pins before spawn; absent pins keep per-node defaults.
    # `--provider` names its flag in refusals rather than the resolved axis.
    try:
        model = reject_empty_model(model)
        provider = (
            resolve_dispatch_harness(provider, flag="--provider")[0]
            if provider is not None
            else None
        )
    except DispatchFlagError as exc:
        typer.echo(f"advance: {exc}", err=True)
        raise typer.Exit(code=2)

    from contextlib import nullcontext

    from fno.backlog.advance import run_advance_epic, run_advance_loose
    from fno.backlog.single_flight import advance_flight_scope

    # --epic routes to the epic-advance path; it is a distinct trigger from the
    # merge-advance --closed path (they never combine on one call).
    if epic is not None:
        if closed is not None or loose:
            typer.echo("advance: --epic is mutually exclusive with --closed/--loose", err=True)
            raise typer.Exit(code=2)
        # One in flight per mission; the key uses the CANONICAL id so
        # both spellings of an epic are one scope. --stop is a control action
        # and never queues behind its own drain.
        canonical_epic = epic
        try:
            from fno.graph._intake import _find_node

            _epic_node = _find_node(wire_rows(path=_graph_path()), epic)
            if _epic_node and _epic_node.get("id"):
                canonical_epic = _epic_node["id"]
        except Exception:  # noqa: BLE001 - an unreadable graph keys on the raw arg
            pass
        scope_cm = nullcontext(True) if stop else advance_flight_scope(canonical_epic, json_out=json_out)
        with scope_cm as ok:
            if not ok:
                return
            run_advance_epic(
                epic,
                stop=stop,
                max_dispatch=max_dispatch,
                json_out=json_out,
                verbose=verbose,
                model=model,
                provider=provider,
                continuation=continuation,
                source=source,
            )
        return
    if loose:
        run_advance_loose(project, closed=closed, max_dispatch=max_dispatch,
                          json_out=json_out, verbose=verbose, model=model,
                          provider=provider)
        return
    if stop or max_dispatch is not None or continuation:
        typer.echo("advance: --stop / --max / --continuation require --epic", err=True)
        raise typer.Exit(code=2)

    # RC2: closed_project is the CLOSED NODE's own project from the graph -
    # NEVER the --project next-selection flag (which is normally OMITTED on a
    # manual `advance --closed A`; see docs/architecture/backlog-board-ordering).
    closed_project: Optional[str] = None
    if closed:
        try:
            from fno.graph._intake import _find_node

            _cn = _find_node(wire_rows(path=_graph_path()), closed)
            closed_project = _cn.get("project") if _cn else None
        except Exception:  # noqa: BLE001 - non-fatal; advance_deps fails closed on None
            closed_project = None

    # One in flight for the board advance: the merge event, a groom
    # leg and a manual run all fire this verb, and nothing used to stop two
    # of them from running at once.
    with advance_flight_scope(None, json_out=json_out) as ok:
        if not ok:
            return
        try:
            result = _advance(
                closed_node_id=closed,
                project=project,
                verbose=verbose,
                model=model,
                provider=provider,
                source=source,
            )
            # G1 (AC5-FR): follow this node's blocked_by edges into OTHER projects.
            # Only meaningful with --closed (an edge source); the project-scoped
            # next selection above never reaches a foreign dependent. Shares the
            # dispatch:<id> dedup with reconcile's call so a node seen by both the
            # reconcile sweep and this explicit verb dispatches at most once.
            if closed:
                _advance_deps(
                    closed_node_id=closed,
                    closed_project=closed_project,
                    verbose=verbose,
                    model=model,
                    provider=provider,
                    source=source,
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
            # and exit 0.: stdout also gets one verdict line - the detail
            # alone left stdout empty, byte-identical to a swallowed crash.
            typer.echo(f"advance: unexpected error (non-fatal): {exc}", err=True)
            typer.echo("advance: failed reason=unexpected-error")
            return
    if json_out:
        typer.echo(json.dumps(result.json_receipt(), indent=2))
    else:
        for line in result.render():
            typer.echo(line)


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
    (). This re-runs the harvest addressed-detection against each open
    retro node's source PR and closes the ones now addressed - the
    reconciliation counterpart to the harvest-side suppression. Dry-run by
    default; ``--apply`` closes via ``fno backlog done --note``. A PR whose
    review state can't be read is skipped, never closed on uncertainty.
    """
    import subprocess

    from fno.retro.reconcile_findings import scan_addressed_findings

    entries = wire_rows(path=_graph_path())
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
        typer.echo(f"\n{len(findings)} node(s) would close. Re-run with --apply to close them.")
        return

    closed = 0
    for f in findings:
        reason = (
            f"addressed on PR #{f.pr_number} ({f.signal}); retro reconcile-findings "
            f"re-check - fix landed without the thread being resolved/replied"
        )
        proc = subprocess.run(["fno", "backlog", "done", f.node_id, "--note", reason])
        if proc.returncode == 0:
            closed += 1
        else:
            typer.echo(
                f"reconcile-findings: close of {f.node_id} failed (rc={proc.returncode})",
                err=True,
            )
    typer.echo(f"reconcile-findings: closed {closed}/{len(findings)} node(s)")



# -- maintain (recurring backlog + kanban hygiene sweep) --


@cli.command("maintain", hidden=True)
def cmd_maintain(
    apply: bool = typer.Option(
        False,
        "--apply",
        help=(
            "Apply the deterministic legs; the judgment legs stay "
            "proposal-only regardless of this flag."
        ),
    ),
    json_out: bool = typer.Option(
        False,
        "--json",
        "-J",
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
            "Read-only retro sweep: print drained nodes carrying evidence of "
            "a human curation decision, then exit; mutates nothing."
        ),
    ),
) -> None:
    """Keep graph.json + the kanban board clean by composing existing verbs.

    Deterministic legs apply under ``--apply``; the judgment legs (dedup,
    drain-stale, cap-Now) only ever propose. Full leg list + loop form:
    docs/backlog-usage.md "Health and hygiene". Best-effort: a single failed
    apply does not abort the rest; an empty graph is a clean no-op.

    The pass runs under a wall-clock budget, ``backlog.maintain.budget_seconds``
    (default 300s), checked between legs; the validity analyzer additionally
    inherits whatever time is left. A pass that runs out exits 4 and prints a
    partial receipt naming the leg it stopped in, plus a health-history row
    with ``complete: false`` - exit 4 means "partial results", never a clean
    board.
    """
    from fno.graph import maintain as _maintain

    _maintain.run_pass(
        apply=apply,
        json_out=json_out,
        recheck=recheck,
        no_validity=no_validity,
        suspect_reverts=suspect_reverts,
        graph_path=_graph_path,
        live_claimed=_live_claimed_node_ids,
        require_live_claimed=_require_live_claimed_node_ids,
    )


# -- reprioritize --


@cli.command("reprioritize", hidden=True)
def cmd_reprioritize(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    priority: str = typer.Argument(..., help="New priority: p0|p1|p2|p3"),
    blocks_everything: bool = typer.Option(
        False, "--blocks-everything", help="Acknowledge that p0 blocks all downstream work."
    ),
) -> None:
    from fno.graph.store import commit_rows_via_store
    from fno.graph._intake import _find_node

    _require_node_id(task_id)

    _validate_priority_or_exit(priority, blocks_everything=blocks_everything)

    old_holder: list = [None]

    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        old_holder[0] = node.get("priority", "p2")
        node["priority"] = priority
        if priority == "p0":
            node["blocks_everything"] = True
        return entries

    commit_rows_via_store(_graph_path(), mutator)
    typer.echo(f"Reprioritized {task_id}: {old_holder[0]} -> {priority}")


@cli.command("migrate-priorities", hidden=True)
def cmd_migrate_priorities(
    apply: bool = typer.Option(False, "--apply", help="Apply the idempotent p0 to p1 migration."),
    rollback: bool = typer.Option(
        False, "--rollback", help="Restore rows changed by the migration."
    ),
) -> None:
    """Dry-run or apply the explicit legacy p0 re-band migration."""
    from fno.graph.migrations import migrate_legacy_p0, rollback_legacy_p0
    from fno.graph.store import commit_rows_via_store

    if apply and rollback:
        typer.echo("Error: --apply and --rollback are mutually exclusive", err=True)
        raise typer.Exit(code=2)
    holder: list[dict] = []
    if rollback:

        def rollback_mutator(entries: list[dict]) -> list[dict]:
            holder.append(rollback_legacy_p0(entries))
            return entries

        commit_rows_via_store(_graph_path(), rollback_mutator)
        receipt = holder[0]
    elif not apply:
        receipt = migrate_legacy_p0(wire_rows(path=_graph_path()), apply=False)
    else:

        def mutator(entries: list[dict]) -> list[dict]:
            holder.append(migrate_legacy_p0(entries, apply=True))
            return entries

        commit_rows_via_store(_graph_path(), mutator)
        receipt = holder[0]
    typer.echo(json.dumps(receipt, sort_keys=True))


@cli.command("migrate-difficulty", hidden=True)
def cmd_migrate_difficulty(
    apply: bool = typer.Option(
        False, "--apply", help="Move each model_tier band onto difficulty and drop the retired key."
    ),
    backfill: bool = typer.Option(
        False, "--backfill", help="Backfill missing bands from size, priority, or type."
    ),
) -> None:
    """Dry-run or apply the difficulty migrations."""
    from fno.graph.migrations import backfill_difficulty, migrate_model_tier
    from fno.graph.store import commit_rows_via_store

    def _run(entries: list[dict]) -> dict:
        try:
            if backfill:
                return backfill_difficulty(entries, apply=apply)
            return migrate_model_tier(entries, apply=apply)
        except ValueError as exc:
            typer.echo(f"fno backlog migrate-difficulty: {exc}", err=True)
            raise typer.Exit(code=2)

    if apply:
        holder: list[dict] = []

        def mutator(entries: list[dict]) -> list[dict]:
            holder.append(_run(entries))
            return entries

        commit_rows_via_store(_graph_path(), mutator)
        receipt = holder[0]
    else:
        receipt = _run(wire_rows(path=_graph_path()))
    typer.echo(json.dumps(receipt, sort_keys=True))


@cli.command("migrate-updated-at", hidden=True)
def cmd_migrate_updated_at(
    apply: bool = typer.Option(
        False,
        "--apply",
        help="Remove the proven-unread __updated_at field from candidate rows.",
    ),
) -> None:
    """Dry-run or apply the one-shot __updated_at residue migration."""
    from fno.graph.migrations import migrate_updated_at
    from fno.graph.store import commit_rows_via_store

    if apply:
        holder: list[dict] = []

        def mutator(entries: list[dict]) -> list[dict]:
            holder.append(migrate_updated_at(entries, apply=True))
            return entries

        commit_rows_via_store(_graph_path(), mutator)
        receipt = holder[0]
    else:
        receipt = migrate_updated_at(wire_rows(path=_graph_path()), apply=False)
    typer.echo(json.dumps(receipt, sort_keys=True))




# -- archive --

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
    """Sweep old terminal (done/superseded) nodes into archive residency:
    same store, but they stop answering default reads.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from datetime import datetime, timezone

    from fno.graph.store import commit_rows_via_store
    from fno.graph.archive import (
        _archive_bucket_counts,
        _last_sweep_line,
        _receipt_reason_order,
        partition_for_archive,
        release_soft_edges,
        retire_stale_postmortems,
    )

    now = datetime.now(timezone.utc)

    def _split(entries):
        # Guard against the FULL graph so an open node in another roadmap that
        # references one of these terminal nodes (blocker/parent/supersede) is
        # still protected; only the archive SET is roadmap-restricted.
        to_archive, _remaining_pool, skipped = partition_for_archive(entries, older_than_days, now)
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
        typer.echo(f"  last sweep: {_last_sweep_line(now)}")

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
        entries, retired = retire_stale_postmortems(wire_rows(path=_graph_path()), now)
        to_archive, _rem, skipped = _split(entries)
        typer.echo(
            f"[dry-run] would archive {len(to_archive)} terminal node(s) "
            f"older than {older_than_days}d into archive residency"
        )
        typer.echo(f"  would retire {len(retired)} stale postmortem receipt(s)")
        _echo_receipt(len(to_archive), _archive_bucket_counts(skipped))
        typer.echo("Re-run with --apply to move them.")
        # Every run emits, dry-run included: a leg that went silent must stay
        # distinguishable from one that never ran, and the dry-run leg (the
        # daily groom rehearsal) is the one most likely to break quietly.
        _emit_swept_event(len(to_archive), _archive_bucket_counts(skipped), mode="dry-run")
        return

    receipt: dict = {"moved": 0, "held": _archive_bucket_counts([]), "stripped": 0, "retired": 0}

    def mutator(entries):
        entries, retired = retire_stale_postmortems(entries, now)
        receipt["retired"] = len(retired)
        to_archive, _remaining, skipped = _split(entries)
        receipt["held"] = _archive_bucket_counts(skipped)
        if not to_archive:
            return entries
        receipt["moved"] = len(to_archive)

        # Soft-edge release BEFORE the stamp: no live row keeps a pointer
        # at an archived id.
        arch_ids = {e["id"] for e in to_archive if isinstance(e, dict) and e.get("id")}
        patched, stripped = release_soft_edges(
            [e for e in entries if e.get("id") not in arch_ids], arch_ids
        )
        receipt["stripped"] = stripped

        # One atomic write: ALL rows come back or the sweep would drop them.
        stamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")
        patched_by_id = {e.get("id"): e for e in patched if isinstance(e, dict)}
        return [
            {**e, "archived_at": stamp} if isinstance(e, dict) and e.get("id") in arch_ids
            else patched_by_id.get(e.get("id"), e)
            for e in entries
        ]

    commit_rows_via_store(_graph_path(), mutator)
    if receipt["moved"]:
        typer.echo(f"Archived {receipt['moved']} terminal node(s)")
    else:
        typer.echo("No terminal nodes eligible to archive.")
    if receipt["retired"]:
        typer.echo(
            f"Retired {receipt['retired']} stale postmortem receipt(s) (closed by age rule)"
        )
    _echo_receipt(receipt["moved"], receipt["held"], receipt["stripped"])
    _emit_swept_event(receipt["moved"], receipt["held"], receipt["stripped"])


@cli.command(
    "album",
    hidden=True,
    epilog="Read-only browse over the archive: the memento book of "
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
    from fno.graph.store import read_archive_entries
    from fno.tracker import active_backend_name

    # The album renders the local archive's shipped work: guarded local-store
    # display, refused (named) under an external selection.
    if active_backend_name() != "graph":
        typer.echo(
            "fno backlog album: the album renders the local archive; unavailable under an external tracker backend",
            err=True,
        )
        raise typer.Exit(code=2)

    # The album shows shipped (done, not superseded) archive residents.
    entries = [
        e for e in read_archive_entries()
        if isinstance(e, dict) and derived_status(e) == "done"
        and not e.get("superseded_by")
    ]
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
    """Clear one node's archive stamp: it answers default reads again.
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    from fno.graph import api
    from fno.graph._intake import _find_node
    from fno.graph.store import read_archive_entries

    _require_node_id(task_id)

    # The default read excludes archived rows, so presence here is live-only.
    if _find_node(wire_rows(path=_graph_path()), task_id) is not None:
        typer.echo(f"warning: {task_id} is already in the working graph", err=True)
        return

    archived = read_archive_entries()
    # Fuzzy-resolve, matching the working-graph lookup and `reopen`'s probe.
    row = _find_node(archived, task_id)
    if row is None:
        # A reminted archive entry keeps its old id as previous_id.
        row = next((e for e in archived if e.get("previous_id") == task_id), None)
    if row is None:
        typer.echo(
            f"Error: {task_id} is in neither the working graph nor the archive",
            err=True,
        )
        raise typer.Exit(code=1)

    resolved = row.get("id") or task_id
    # The store op clears archived_at under the lock; one row, one write.
    payload = api.unarchive_node(resolved, path=_graph_path())
    if not payload.success:
        typer.echo(f"Error: {resolved} could not be unarchived", err=True)
        raise typer.Exit(code=1)
    typer.echo(f"Unarchived {resolved}")


# -- Internal helpers for intake / update (avoid circular imports) --


def _apply_claim_in_place(es, claim_id: str, *, plan_path: str, spec: dict, project: Optional[str]):
    """Bind a claimed node to the plan in place, in any state: attach the
    plan, merge the doc-declared fields, promote idea -> ready.

    The single-plan claim lane's mutator body, shared with the multi lane so
    a claim lands identically from either surface. A second copy here would
    drift, and drift on this path mints duplicate nodes.
    """
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
    if raw_difficulty is None and frontmatter.get("model_tier") is not None:
        # Same loss-signal as the intake lane (): the claim reads the
        # canonical key only, so a plan still spelling model_tier must hear
        # the band was dropped rather than wonder why the node has none.
        typer.echo(
            f"warning: {plan_path}: frontmatter model_tier is retired and no "
            "longer read; set difficulty: low|medium|high to carry the band",
            err=True,
        )
    for entry in es:
        if entry.get("id") != claim_id:
            continue
        entry["plan_path"] = plan_path
        entry["title"] = spec["title"]
        if raw_difficulty is not None:
            try:
                revised_difficulty = normalize_difficulty(raw_difficulty)
            except (ValueError, AttributeError, TypeError):
                # AttributeError joins ValueError: a non-string difficulty
                # dies inside strip()/lower() before the band check; warn and
                # leave the band unchanged rather than traceback mid-claim.
                typer.echo(
                    f"warning: invalid difficulty {raw_difficulty!r} "
                    "(expected one of: low, medium, high); "
                    "difficulty left unchanged",
                    err=True,
                )
            else:
                # The one canonical-write shape, shared with the native
                # update verb; the claim records every canonical write (the filed-versus-
                # revised delta counts confirmations too).
                from fno.graph._constants import write_canonical_difficulty

                write_canonical_difficulty(
                    entry,
                    revised_difficulty,
                    "blueprint",
                    datetime.now(timezone.utc).isoformat(),
                    history_on="always",
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
            parent_node = _find_node(es, entry["parent"]) if entry.get("parent") else None
            if (
                claimed_type == "epic"
                and parent_node is not None
                and _would_exceed_epic_depth(es, {**entry, "type": "epic"}, parent_node)
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
            merged = list(dict.fromkeys([*entry.get("blocked_by", []), *spec["deps"]]))
            entry["blocked_by"] = merged
        # Only override priority if the plan supplied a non-default one.
        if spec.get("priority") and spec["priority"] != "p2":
            entry["priority"] = spec["priority"]
        if spec.get("priority") == "p0" and spec.get("blocks_everything"):
            entry["blocks_everything"] = True
        if spec.get("points") is not None:
            entry["points"] = spec["points"]
        # Backfill project/cwd when the node was created via
        # `fno backlog new` (no plan path -> no auto-scope) and is
        # now being claimed by a plan that lives in a project repo.
        # Only fills nulls; never overwrites existing values.
        if entry.get("project") is None or entry.get("cwd") is None:
            resolved_project, resolved_cwd, _ = resolve_node_project_and_cwd(
                plan_path,
                project,
                es,
            )
            if entry.get("project") is None and resolved_project:
                entry["project"] = resolved_project
            if entry.get("cwd") is None and resolved_cwd:
                entry["cwd"] = resolved_cwd
        break
    return es


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


def _do_intake_multi(
    args,
    all_paths: list[str],
    *,
    roadmap_id,
    dry_run,
    allow_no_surface: bool = False,
) -> None:
    """Multi-path intake flow delegating to intake helpers."""
    from fno.graph.store import commit_rows_via_store
    from fno.graph._intake import (
        _prepare_intake,
        _build_intake_node,
        _refuse_surfaceless_intake,
        _validate_cli_deps,
    )

    cli_deps: list[str] = (
        [d.strip() for d in args.deps.split(",") if d.strip()] if args.deps else []
    )
    cli_project = getattr(args, "project", None)
    _refuse_create_on_external_backend()
    _validate_cli_deps(cli_deps, wire_rows(path=_graph_path()))

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

    preview_entries = wire_rows(path=_graph_path())
    # Rationale (8 lines): docs/architecture/graph-cli-rationale.md#do-intake-multi-11781
    from fno.graph.store import plan_path_owner_conflict

    def _unbound(f: str) -> bool:
        # plan_path_owner_conflict compares normpath-only, so a CLI-relative
        # spelling never matches a graph-stored absolute plan_path. Probe the
        # absolute spelling too - the mutator's per-file verdicts have the
        # same view, and a false refusal blames a file this run was a no-op
        # for (the reviewer's divergent-spelling batch case).
        spellings = [f]
        target = Path(f).expanduser()
        if not target.is_absolute():
            spellings.append(str(target.resolve()))
        return all(plan_path_owner_conflict(preview_entries, None, s) is None for s in spellings)

    _refuse_surfaceless_intake(
        [f for f in concrete_files if _unbound(f)],
        allow_no_surface=allow_no_surface,
    )

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
        would = 0
        for r in resolved:
            if r["status"] == "missing":
                typer.echo(f"  warning: not found, skipped: {r['path']}")
                continue
            for f in r["files"]:
                # Validate-then-preview, mirroring the single-plan dry run: a
                # plan the real run would refuse (bad priority, a cross-roadmap
                # owner conflict) or skip as already-intaked must preview as
                # exactly that, never as would-intake.
                try:
                    prep = _prepare_intake(
                        f,
                        preview_entries,
                        roadmap_id=roadmap_id,
                        cli_title=args.title,
                        cli_priority=args.priority,
                        cli_deps=cli_deps,
                        cli_points=args.points,
                        cli_project=cli_project,
                    )
                except ValueError as exc:
                    typer.echo(f"  error: would skip {f}: {exc}", err=True)
                    continue
                if prep["status"] == "already":
                    typer.echo(f'  already intaked {prep["id"]}: "{prep["title"]}"  ({f})')
                    continue
                spec = prep["node_spec"]
                if prep["status"] == "claim":
                    typer.echo(
                        f'  would claim: "{spec["title"]}"  (plan: {f})  (claims {prep["id"]})'
                    )
                    _apply_claim_in_place(
                        preview_entries, prep["id"], plan_path=f,
                        spec=spec, project=cli_project,
                    )
                    continue
                # Grow the preview graph the way the real mutator grows its
                # entries - the BUILT node, so a later duplicate of this plan
                # previews the outcome the real run would produce (already
                # intaked by this batch), not a second would-intake. Building
                # here also previews build-time refusals (invalid difficulty)
                # as would-skip, the same outcome the real run now lands.
                try:
                    preview_entries.append(_build_intake_node(spec, preview_entries))
                except ValueError as exc:
                    typer.echo(f"  error: would skip {f}: {exc}", err=True)
                    continue
                typer.echo(f'  would intake: "{spec["title"]}"  (plan: {f})')
                would += 1
        typer.echo(f"{would} plans would be intaked. Run without --dry-run to apply.")
        return

    typer.echo(f"Multi-intake {len(concrete_files)} plans:")
    tallies = {"intaked": 0, "claimed": 0, "already": 0, "invalid": 0}
    landed_projects: set[str] = set()
    new_ids: list[str] = []
    claimed_ids: list[str] = []

    def mutator(es):
        for r in resolved:
            if r["status"] != "ready":
                typer.echo(f"  warning: not found, skipped: {r['path']}")
                continue
            for f in r["files"]:
                # A refusal names ONE file: skip it with a clean per-file error
                # so the batch lands, exactly like the single-plan surface that
                # catches the same ValueError. The try covers BOTH raise sites -
                # _prepare_intake (bad priority, a cross-roadmap owner conflict)
                # and _build_intake_node (invalid difficulty frontmatter).
                # Uncaught, either aborted the whole mutator with a traceback
                # from inside the lock and persisted nothing.
                try:
                    prep = _prepare_intake(
                        f,
                        es,
                        roadmap_id=roadmap_id,
                        cli_title=args.title,
                        cli_priority=args.priority,
                        cli_deps=cli_deps,
                        cli_points=args.points,
                        cli_project=cli_project,
                    )
                except ValueError as exc:
                    tallies["invalid"] += 1
                    typer.echo(f"  error: skipped {f}: {exc}", err=True)
                    continue
                if prep["status"] == "already":
                    tallies["already"] += 1
                    typer.echo(f'  already intaked {prep["id"]}: "{prep["title"]}"  ({f})')
                    continue
                if prep["status"] == "claim":
                    # The claim lands on the existing idea node in place, the
                    # same helper the single-plan lane uses - never a fresh
                    # node beside an unclaimed idea (the duplicate the claim
                    # mechanism exists to prevent).
                    _apply_claim_in_place(
                        es,
                        prep["id"],
                        plan_path=f,
                        spec=prep["node_spec"],
                        project=cli_project,
                    )
                    tallies["claimed"] += 1
                    claimed_ids.append(prep["id"])
                    typer.echo(
                        f'  claim {prep["id"]} via {prep["claim_source"]}: "{prep["title"]}"  ({f})'
                    )
                    continue
                try:
                    node = _build_intake_node(prep["node_spec"], es)
                except ValueError as exc:
                    tallies["invalid"] += 1
                    typer.echo(f"  error: skipped {f}: {exc}", err=True)
                    continue
                es.append(node)
                tallies["intaked"] += 1
                new_ids.append(node["id"])
                typer.echo(f'  intake {node["id"]}: "{node["title"]}"  ({f})')
                if isinstance(node.get("project"), str):
                    landed_projects.add(node["project"])
        return es

    commit_rows_via_store(_graph_path(), mutator)

    # A claimed node gets the same nav-field projection the single-plan claim
    # path runs; additive and guarded so it never wedges the batch.
    if claimed_ids:
        try:
            from fno.plan._project import project_graph_nodes

            project_graph_nodes(wire_rows(path=_graph_path()), claimed_ids)
        except Exception as e:  # noqa: BLE001
            _safe_stderr_warn(f"warning: post-claim projection skipped: {e}\n")

    # Filing-time dedup net (plan ): per just-born node, warn if it
    # resembles an existing one. Fresh post-write read; non-fatal.
    try:
        from fno.graph._intake import _find_node, _warn_similar_nodes

        post_entries = wire_rows(path=_graph_path())
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
        f"\n{tallies['intaked']} newly intaked, "
        f"{tallies['claimed']} claimed, "
        f"{tallies['already']} already intaked, "
        f"{missing} skipped." + (f" {tallies['invalid']} refused." if tallies["invalid"] else "")
    )
    if tallies["intaked"] + tallies["claimed"] + tallies["already"] == 0:
        raise typer.Exit(code=4)


@cli.command("discover", hidden=True)
def cmd_discover(
    limit: int = typer.Option(
        20,
        "--limit",
        "-L",
        min=1,
        help="Max candidate matches retained for each expired node.",
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit a JSON worklist."),
) -> None:
    """Review expired deferred nodes without changing backlog state.

    The output is a ranked worklist for a human.  It never closes, undefers,
    or otherwise writes a node.
    """
    from fno.graph import discovery

    report, refusal = discovery.expired_worklist(limit, graph_path=_graph_path())
    if refusal:
        typer.echo(refusal, err=True)
        raise typer.Exit(code=2)

    if json_output:
        typer.echo(json.dumps(report, indent=2))
        return

    worklist = report["worklist"]
    positive = report["positive_control"]
    typer.echo(
        f"discover: assessed={report['assessed']} "
        f"excluded-by-kind={report['excluded_by_kind']}"
    )
    for row in worklist:
        evidence = ", ".join(row["evidence"]) or row["reason"]
        typer.echo(f"{row['id']}\t{row['verdict']}\t{evidence}")
    if positive is not None:
        typer.echo(
            f"positive control: {positive['query']!r} -> "
            f"{', '.join(positive['matches']) or 'no matches'} ({positive['lane']})"
        )
    for warning in report["warnings"]:
        typer.echo(f"degraded: {warning}", err=True)


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
        None,
        "--self-id",
        help="Skip this node ID when comparing (excludes self-collision)",
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit structured JSON instead of human text"
    ),
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
        typer.echo(
            json.dumps(
                {
                    "status": "ok" if evaluated else "unevaluated",
                    "collisions": payload,
                },
                indent=2,
            )
        )
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
    "fno backlog supersede <new> --replaces <old> "
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
        False,
        "--force",
        "-F",
        help="Supersede even if the target still has live children (orphaning them)",
    ),
) -> None:
    """Record that ``new_id`` proposes to replace ``replaces``.

    Sets the compatibility edge plus a structured evidence record on the old
    node. The edge terminals the old row's status immediately ; the
    record stays unverified until a merged PR covers every declared surface,
    which reconcile reports as receipts. Refuses if ``replaces`` still has
    live children unless ``--force`` is given; under ``--force`` the live
    children's ``parent`` is cleared so they stay dispatchable instead of
    stranding under a dead unit. Reverse with ``unsupersede``.
    """
    # The supersede write is native: the door (fno backlog supersede) owns the
    # guard ladder, the edge + record, both child releases, and the plan
    # projection. A direct wheel spelling has no leg left to run, so it names
    # the door instead of carrying a second implementation.
    typer.echo(
        "Error: the supersede write is served by the native door; "
        "run `fno backlog supersede <new> --replaces <old> ...`.",
        err=True,
    )
    raise typer.Exit(code=2)


# ---------------------------------------------------------------------------
# unsupersede: reverse a supersede (the death transition was reversible only
# by hand-editing graph.json before this; cmd_undefer clears deferred_at but
# leaves superseded_by set, so a superseded node stayed superseded)
# ---------------------------------------------------------------------------





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
            for dep in by_id[stack.pop()].get("blocked_by") or []:
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
                for ev in e.get("requires_evidence") or []:
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
    return {
        "live": "live",
        "suspect": "unknown",
        "stale": "unknown",
        "corrupted": "unknown",
        "free": "",
    }.get(state, "")


# -- the external-backend verb classification (the sets are imported at the
# top of this module from _verb_classification.py, beside the data)


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
        if label in _NO_GRAIN_ON_EXTERNAL_BACKEND:
            raise typer.Exit(code=TASK_NO_GRAIN_EXIT)
        raise typer.Exit(code=1)


def iter_backlog_registry():
    """The (group-label, typer-app) pairs carrying every backlog verb.
    The ONE structural list: the verb classifier, the census, and the
    classification tests all walk it; register a new sub-app beside its
    add_typer call.
    """
    return [
        (None, cli),
        ("capture", _capture_cli),
        ("batch", _batch_cli),
        ("relatedness", _relatedness_cli),
        ("epic", _epic_cli),
        ("task", task_app),
        ("collisions", collisions_app),
    ]


from fno.graph import note_cli  # noqa: E402,F401

# Node-lifecycle reversal verbs live in graph/lifecycle.py (file-budget
# ratchet): the module never imports graph.cli, so the shared helpers are
# injected here at registration, the same shape as the decide leaves above.
# Each helper rides a lambda so the call resolves the module global at CALL
# time: tests monkeypatch these names on this module, and a reference
# captured at import would read the unpatched original.
from fno.graph.lifecycle import register_lifecycle_commands  # noqa: E402

register_lifecycle_commands(
    cli,
    lambda *a, **k: _expand_valid_ids(*a, **k),
    lambda *a, **k: _require_nodes(*a, **k),
    lambda: _graph_path(),
    lambda *a, **k: _project_plans_from_graph(*a, **k),
)

# The root loader runs this before Click builds the tree.
_fno_pre_dispatch = classify_backlog_verbs
