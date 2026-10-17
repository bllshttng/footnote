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
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, List, Literal, Optional

import typer

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

from fno.graph.worked import cmd_worked as _cmd_worked  # noqa: E402

from fno.graph.triage_forward import register_triage_forward  # noqa: E402
register_triage_forward(cli)
from fno.graph.native_forward import register_native_forward  # noqa: E402

# The create/epic verbs whose bodies ported to the dispatcher; the wheel
# keeps the route (hidden, whole-argv forward) so this wheel still serves them.
register_native_forward(cli, "intake")
register_native_forward(cli, "decompose")
# Born native: the in-place id rename has no wheel body at all.
register_native_forward(cli, "re-id")
cli.command("worked", hidden=True)(_cmd_worked)


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
from fno.graph.statuses import live_claimed_node_ids as _live_claimed_node_ids  # noqa: E402


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
    point (cmd_tree, and the native decompose/intake doors; add, idea and the new word
    answer natively and carry their own guard) so the guard is not decorative. A guard on only some reachable
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


# -- update --


# -- release / requeue: the queue-return subject lives in fno.backlog.requeue;
# registration stays here on the backlog app. --




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
        from fno.rust_binary import verb_call

        guards = verb_call(["backlog", "advance", "--stdin"], {"op": "selection-guards", "entries": source_entries})["guards"]
        safe_rows = []
        for row in current_observer["rows"]:
            entry = by_id.get(row.get("id"))
            if entry is None or row.get("id") in occupied:
                continue
            if entry.get("completed_at") or _has_unmerged_open_pr(entry):
                continue
            if entry.get("id") in container_ids or _is_batched_member(entry):
                continue
            if guards.get(entry.get("id"), {}).get("reason"):
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
        from fno.rust_binary import verb_call

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
            verb_call(["backlog", "advance", "--stdin"], {"op": "staleness"})["days"],
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


# -- join --


# -- lanes --




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


# -- cost --


# -- remove --


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


# -- stuck-epics --




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


def _auto_closed_note(_entry: dict) -> str:
    """completion_note for a container closed by all-children-complete ().

    Both close paths (_cascade_close_parents, _sweep_close_done_epics) reach
    here holding the parent dict. A plan-having, PR-less container is no
    longer closed at all: the own-plan-work hold stops both close paths, so
    the old UNVERIFIED flag branch is retired with the false close it
    flagged.
    """
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


# -- reconcile (close merged-PR drift) --
# `done` and `reopen` are a deliberate gate inversion (`done` refuses when no
# referenced PR is merged, `reopen` refuses when one IS); both verbs are
# native now, owned by the door's lifecycle module.


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


# -- reprioritize --




# collisions sub-app: file-overlap detection between plans
# ---------------------------------------------------------------------------

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
            # Exit 6: the shared no-task-grain shape (the native task verb
            # answers with the same code).
            raise typer.Exit(code=6)
        raise typer.Exit(code=1)


def iter_backlog_registry():
    """The (group-label, typer-app) pairs carrying every backlog verb.
    The ONE structural list: the verb classifier, the census, and the
    classification tests all walk it; register a new sub-app beside its
    add_typer call.
    """
    return [
        (None, cli),
    ]


from fno.graph import note_cli  # noqa: E402,F401

# The node-lifecycle reversal verbs (defer, undefer, retract, unsupersede)
# answer natively from the grouped dispatcher; the Python registration
# retired with their bodies.

# The root loader runs this before Click builds the tree.
_fno_pre_dispatch = classify_backlog_verbs
