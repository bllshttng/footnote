"""Why this node and not that one: the selection cascade, made answerable.

`fno backlog next` narrows the open graph through a fixed sequence of filters
and then sorts what survives. Every drop was silent. An operator asking why a
ready node never launched had no instrument, and the 2026-09-01 orchestration
audit had to reconstruct the answer by reading source.

The cascade lives HERE and `cmd_next._pick_ready` consumes it, rather than the
explanation reimplementing the same ten filters beside the selector. A parallel
implementation is a second selector that lies the moment either one moves, and
an explanation that disagrees with the selection is worse than none.

A filter narrows a LIST, not a row. That is not indirection for its own sake:
`filter_by_project` resolves the project by DETECTING it from the candidates it
is handed, so it cannot be expressed as a per-row predicate without changing
what it does.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Optional


@dataclass(frozen=True)
class SelectionFilter:
    """One narrowing step, with the sentence an operator needs when it bites."""

    #: Stable identifier, printed by `advance --explain` and safe to grep for.
    name: str
    #: Why a node dropped here, in terms of what the operator can do about it.
    why: str
    narrow: Callable[[list[dict]], list[dict]]


@dataclass
class CascadeResult:
    """What survived, what each filter took, and where each node fell out."""

    survivors: list[dict] = field(default_factory=list)
    #: filter name -> how many candidates it removed, in cascade order.
    drops: list[tuple[str, int]] = field(default_factory=list)
    #: node id -> the name of the first filter that removed it. A node absent
    #: from this map and from `survivors` was never a candidate at all.
    dropped_by: dict[str, str] = field(default_factory=dict)

    def reason_for(self, node_id: str) -> Optional[str]:
        """The filter that dropped ``node_id``, or None if it survived."""
        return self.dropped_by.get(node_id)


def run_cascade(candidates: list[dict], filters: list[SelectionFilter]) -> CascadeResult:
    """Apply ``filters`` in order, recording what each one took.

    Attribution is to the FIRST filter that removes a node. A node dropped by
    the project filter may also be a container and also be batched; naming all
    three would bury the one an operator has to act on.
    """
    result = CascadeResult()
    current = list(candidates)
    for f in filters:
        before = {e.get("id") for e in current if e.get("id")}
        current = f.narrow(current)
        after = {e.get("id") for e in current if e.get("id")}
        gone = before - after
        result.drops.append((f.name, len(gone)))
        for node_id in gone:
            if node_id:
                result.dropped_by.setdefault(node_id, f.name)
    result.survivors = current
    return result


def build_selection_filters(
    entries: list[dict],
    *,
    roadmap_id: Optional[str],
    mission: Optional[str],
    parent_target_id: Optional[str],
    project_filter: Optional[str],
    all_: bool,
    claimed: "set[str] | frozenset[str]",
    container_ids: "set[str] | frozenset[str]",
) -> list[SelectionFilter]:
    """The cascade `fno backlog next` runs, in its exact shipped order.

    ``claimed`` and ``container_ids`` are passed in rather than computed here
    because the caller already holds them under its graph lock; recomputing
    would read a different instant than the selection it is explaining.
    """
    from datetime import datetime, timezone

    from fno.graph._intake import descendants_of, filter_by_project

    fs: list[SelectionFilter] = []

    if roadmap_id:
        fs.append(
            SelectionFilter(
                "roadmap",
                f"not on roadmap {roadmap_id}",
                lambda c: [e for e in c if e.get("roadmap_id") == roadmap_id],
            )
        )
    if mission:
        fs.append(
            SelectionFilter(
                "mission",
                f"not in mission {mission}",
                lambda c: [e for e in c if e.get("mission_id") == mission],
            )
        )
    if parent_target_id is not None:
        scope = descendants_of(entries, parent_target_id)
        fs.append(
            SelectionFilter(
                "parent-scope",
                f"not a descendant of {parent_target_id}",
                lambda c: [e for e in c if e.get("id") in scope],
            )
        )

    fs.append(
        SelectionFilter(
            "project",
            "belongs to another project (pass --all to widen, or --project)",
            lambda c: filter_by_project(c, project_filter, all_),
        )
    )

    if claimed:
        fs.append(
            SelectionFilter(
                "live-claim",
                "a live session already holds node:<id>; check `fno agents claim status`",
                lambda c: [e for e in c if e.get("id") not in claimed],
            )
        )

    def _drop_open_pr(c: list[dict]) -> list[dict]:
        from fno.graph.cli import _has_unmerged_open_pr

        return [e for e in c if e.get("status") != "ready" or not _has_unmerged_open_pr(e)]

    fs.append(
        SelectionFilter(
            "unmerged-open-pr",
            "already carries a PR that has not merged; the work is in review, not waiting",
            _drop_open_pr,
        )
    )

    fs.append(
        SelectionFilter(
            "container",
            "an epic is never built directly; its work lives in its children",
            lambda c: [e for e in c if e.get("id") not in container_ids],
        )
    )

    def _drop_batched(c: list[dict]) -> list[dict]:
        from fno.graph.cli import _is_batched_member

        return [e for e in c if not _is_batched_member(e)]

    fs.append(
        SelectionFilter(
            "batched",
            "committed to an open batch; it ships via the batch PR",
            _drop_batched,
        )
    )

    def _drop_guarded(c: list[dict]) -> list[dict]:
        from fno.backlog.advance import _guard_staleness_days, selection_guards

        guard_now = datetime.now(timezone.utc)
        guard_stale = _guard_staleness_days()
        guard_by_id = {e.get("id"): e for e in entries if e.get("id")}
        return [
            e
            for e in c
            if not selection_guards(e, guard_by_id, guard_now, staleness_days=guard_stale)
        ]

    fs.append(
        SelectionFilter(
            "selection-guard",
            "under a dead ancestor, or ready and untouched past the staleness window",
            _drop_guarded,
        )
    )
    return fs


# ---------------------------------------------------------------------------
# The report: selection, gates, routing, decision.
#
# `advance --explain` is a DRY RUN, not a receipt formatter, and that is the
# single most consequential decision here. Measured 2026-09-01: all 83
# `advance_skipped` rows in the project journal carry `reason: "disabled"`.
# `config.auto_continue.enabled` is false, so advance() returns at its first
# branch and the whole selection / lane-cap / quota / routing pipeline below has
# ZERO production instances. A verb that formatted what advance decided would
# print "disabled" a hundred percent of the time and teach nothing.
#
# So this runs the pipeline itself, ignores the armed state, dispatches nothing,
# claims nothing, and writes no event.
# ---------------------------------------------------------------------------


@dataclass
class Gate:
    """One admission gate as an operator needs to read it.

    ``measured`` and ``threshold`` are carried separately from ``verdict`` on
    purpose. A gate that says only "pass" teaches nothing about how close it is,
    and a gate that says only "refuse" teaches nothing about what to change.
    """

    name: str
    measured: Optional[str]
    threshold: Optional[str]
    verdict: str
    #: The config key an operator would edit, when there is one.
    key: Optional[str] = None
    note: Optional[str] = None

    def as_dict(self) -> dict:
        return {
            "name": self.name,
            "measured": self.measured,
            "threshold": self.threshold,
            "verdict": self.verdict,
            "key": self.key,
            "note": self.note,
        }

    def line(self) -> str:
        measured = self.measured if self.measured is not None else "?"
        threshold = self.threshold if self.threshold is not None else "-"
        out = f"  {self.name:<22} {measured:>12} / {threshold:<12} {self.verdict}"
        if self.key:
            out += f"  [{self.key}]"
        if self.note:
            out += f"\n{' ' * 25}{self.note}"
        return out


def _unreadable(name: str, exc: BaseException, *, key: Optional[str] = None) -> Gate:
    """A gate whose measurement failed.

    Never rendered as a pass and never as 0. An unreadable count and an empty
    fleet are opposite facts, and the spawn gate itself treats unreadable as a
    refusal (fail-closed), so showing it as headroom would invert the meaning.
    """
    return Gate(name, None, None, f"unreadable: {exc}", key=key)


def gates_for(node: Optional[dict], grid_harness: Optional[str] = None) -> list[Gate]:
    """Every gate advance would consult for ``node``, each measured.

    Calls the measurement functions only - never ``preflight_gate``, which
    acquires the spawn mutex and a worker slot. An explain that queued behind
    the real gate would change the fleet it is describing.

    ``grid_harness`` is the harness the capacity grid picked, so the provider
    lane reported is the one the spawn would actually be counted against rather
    than the config default the grid was about to override.
    """
    from fno.agents.spawn_gate import (
        ProviderCountUnavailable,
        census,
        provider_lanes_cap,
        provider_live_count,
    )
    from fno.backlog import advance as adv

    out: list[Gate] = []

    try:
        walker_live = adv._claim_is_live(adv._walker_key())
        out.append(
            Gate(
                "walker-claim",
                "live" if walker_live else "free",
                "free",
                "refuse" if walker_live else "pass",
                note="a live walk owns this repo; it will pick the node up itself"
                if walker_live
                else None,
            )
        )
    except Exception as exc:  # noqa: BLE001
        out.append(_unreadable("walker-claim", exc))

    if node is not None:
        node_cwd = node.get("_resolved_cwd") or node.get("cwd") or None
        try:
            blocked = adv._node_dispatch_block_reason(node["id"], node_cwd)
            out.append(
                Gate(
                    "node-claim",
                    blocked or "free",
                    "free",
                    "refuse" if blocked else "pass",
                )
            )
        except Exception as exc:  # noqa: BLE001
            out.append(_unreadable("node-claim", exc))

    # Per-project occupancy row DELETED with the dead lane counter (x-7f1f):
    # the epic advance's width derives from spawn-gate headroom (the fleet and
    # provider rows above), and a configured parallel.max_lanes is deprecated
    # and ignored - a counter for a cap nobody reads is a row that reports a
    # selector the drain does not run.

    # Provider lanes: the cap that was actually binding on 2026-09-01.
    provider = _resolved_vendor(node, grid_harness)
    if provider is None and node is None:
        # No subject to resolve a vendor from (the epic explain with an empty
        # fill): the binding configured provider is the one whose cap explains
        # a width of 0. Without this row the report shows width 0 and no gate
        # naming why. Fails open to no row, never to a fake pass.
        from fno.backlog import advance as adv

        try:
            provider = adv._binding_provider()
        except Exception:  # noqa: BLE001 - an unreadable read names no provider
            provider = None
    if provider:
        try:
            from fno.config import load_settings, provider_limits_table

            limits = dict(provider_limits_table(load_settings().agents))
            cap = provider_lanes_cap(limits.get(provider))
        except Exception as exc:  # noqa: BLE001
            out.append(_unreadable("provider-lane", exc))
        else:
            try:
                count = provider_live_count(provider)
            except ProviderCountUnavailable as exc:
                out.append(
                    _unreadable(
                        "provider-lane", exc, key=f"agents.provider_limits.{provider}.lanes"
                    )
                )
            else:
                full = cap is not None and count >= cap
                out.append(
                    Gate(
                        "provider-lane",
                        f"{count} ({provider})",
                        str(cap) if cap is not None else "uncapped",
                        "refuse" if full else "pass",
                        key=f"agents.provider_limits.{provider}.lanes",
                    )
                )

    try:
        c = census()
        cap = int(_max_live())
        out.append(
            Gate(
                "fleet-rows",
                str(c.slot_count),
                str(cap),
                "refuse" if c.slot_count >= cap else "pass",
                key="agents.max_live",
                note="x-3f84: rows are not what the machine spends; see the machine gates",
            )
        )
    except Exception as exc:  # noqa: BLE001
        out.append(_unreadable("fleet-rows", exc, key="agents.max_live"))

    out.extend(_machine_gates())
    return out


def _max_live() -> int:
    from fno.config import load_settings

    return int(load_settings().agents.max_live)


def _machine_gates() -> list[Gate]:
    """RAM and load, read the way the gate reads them (never probing to refuse)."""
    from fno.agents.spawn_gate import available_ram_gb

    out: list[Gate] = []
    try:
        from fno.config import load_settings

        agents_cfg = load_settings().agents
        floor = float(agents_cfg.min_free_gb)
        per_cpu = float(agents_cfg.max_load_per_cpu)
    except Exception as exc:  # noqa: BLE001
        return [_unreadable("machine", exc)]

    try:
        avail = available_ram_gb()
    except Exception as exc:  # noqa: BLE001
        out.append(_unreadable("ram-floor", exc, key="agents.min_free_gb"))
    else:
        if avail is None:
            out.append(
                Gate("ram-floor", None, f"{floor:.1f}GB", "skipped: RAM unreadable",
                     key="agents.min_free_gb")
            )
        else:
            out.append(
                Gate(
                    "ram-floor",
                    f"{avail:.1f}GB",
                    f"{floor:.1f}GB",
                    "refuse" if avail < floor else "pass",
                    key="agents.min_free_gb",
                )
            )

    try:
        import os

        load1 = os.getloadavg()[0]
        cpus = os.cpu_count() or 1
        trigger = per_cpu * cpus
        out.append(
            Gate(
                "load-trigger",
                f"{load1:.1f}",
                f"{trigger:.1f} ({per_cpu:g} x {cpus} cpu)",
                # Over the trigger the gate does NOT refuse; it asks footprint
                # whose CPU this is. Calling that "refuse" here would be a
                # second instrument disagreeing with the first (x-7c0f).
                "over trigger; attribution decides" if load1 > trigger else "pass",
                key="agents.max_load_per_cpu",
            )
        )
    except Exception as exc:  # noqa: BLE001
        out.append(_unreadable("load-trigger", exc, key="agents.max_load_per_cpu"))
    return out


def _resolved_vendor(node: Optional[dict], grid_harness: Optional[str] = None) -> Optional[str]:
    """The VENDOR whose lane cap a spawn for ``node`` would be counted against.

    Five axes, never confused: harness, provider (vendor), model, effort,
    account. `agents.provider_limits` is keyed by VENDOR (`zai`), while a node's
    own `provider` field and `config.dispatch.harness` carry the HARNESS
    (`codex`), and `effective_active()` returns an ACCOUNT record (`makers`).
    An early draft of this function reported `provider-lane 0 (makers)` - an
    account name checked against a vendor-keyed table, so it could only ever
    read 0. That is the axis-inference trap by name.

    Resolved through `resolve_lane_vendor`, the shipped harness-to-vendor
    mapping, rather than a second table here: two tables disagree.
    """
    if node is None:
        return None
    from fno.agents.spawn_defaults import resolve_lane_vendor

    harness = grid_harness or (node.get("provider") or "").strip() or None
    if harness is None:
        try:
            from fno.dispatch_flags import resolve_dispatch_harness

            harness = resolve_dispatch_harness(None)[0]
        except Exception:  # noqa: BLE001 - an unresolved harness reports absent
            return None
    try:
        return resolve_lane_vendor([], harness=harness)
    except Exception:  # noqa: BLE001
        return None


def routing_for(node: Optional[dict]) -> dict:
    """What the capacity grid resolves for ``node``, and from which inputs.

    The chain is RECOVERED, not constructed: `route_resolve.resolve_grid`
    already returns ``(candidate, chain)`` whose last element is its terminal
    reason, and `advance._grid_lane_for` throws it away as ``_chain``. The
    strings are the existing receipt vocabulary and are surfaced verbatim -
    reformatting them would fork it.
    """
    if node is None:
        return {"chain": [], "candidate": None, "inputs": {}}
    from fno import route_resolve

    # An unplanned node bills the planning tier at the spawn seam, so the floor
    # is applied here too or the two dispatch doors price one node differently.
    role = None if (node.get("plan_path") or "").strip() else "planning"
    inputs = {
        "difficulty": node.get("difficulty"),
        "priority": node.get("priority"),
        "role": role,
        "plan_path": node.get("plan_path") or None,
    }
    try:
        inventory = route_resolve.resolve_inventory()
        capacity = dict(route_resolve.runtime_capacity(inventory=inventory))
        candidate, chain = route_resolve.resolve_grid(
            node.get("difficulty"),
            node.get("priority"),
            capacity,
            role=role,
            inventory=inventory,
        )
    except Exception as exc:  # noqa: BLE001 - an unreadable grid is reported
        return {"chain": [f"grid unreadable: {exc}"], "candidate": None, "inputs": inputs}
    inputs["capacity"] = {
        harness: (state.get("state") if isinstance(state, dict) else state)
        for harness, state in capacity.items()
    }
    return {"chain": list(chain), "candidate": candidate, "inputs": inputs}


def build_report(
    *,
    project: Optional[str],
    node_id: Optional[str] = None,
    top: int = 5,
) -> dict:
    """Run the selection and routing pipeline as a READ, and report all of it.

    Never dispatches, never claims, never emits. Ignores
    ``config.auto_continue.enabled`` - see this section's header for why the
    armed state is context here and not the answer.
    """
    # Classification + backstop: `advance` is a tracker-owned verb, so its
    # callback already refuses on an external tracker backend before this
    # runs. The consumer census attributes the graph read to THIS function,
    # so the one refusal call lives here too - never a second ruling.
    from fno.graph.cli import _refuse_tracker_owned_on_external_backend

    _refuse_tracker_owned_on_external_backend("advance")

    from fno.backlog import advance as adv
    from fno.graph._intake import make_selection_sort_key
    from fno.graph.cli import _container_ids, _require_live_claimed_node_ids
    from fno.graph.ladder import is_cold_dispatchable
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    entries = read_graph(graph_json())
    claimed = _require_live_claimed_node_ids("backlog explain")
    container_ids = _container_ids(entries)

    # The same admission predicate `_pick_ready` opens with, and the same
    # default `allowed` set the autonomous paths use (bare `next`).
    candidates = [
        e
        for e in entries
        if (e.get("status") == "ready" or is_cold_dispatchable(e))
        and not e.get("completed_at")
    ]
    pool = len(candidates)

    filters = build_selection_filters(
        entries,
        roadmap_id=None,
        mission=None,
        parent_target_id=None,
        project_filter=project,
        all_=project is None,
        claimed=claimed,
        container_ids=container_ids,
    )
    cascade = run_cascade(candidates, filters)
    survivors = sorted(
        cascade.survivors, key=make_selection_sort_key(entries, live_claimed=claimed)
    )

    winner = survivors[0] if survivors else None
    subject_id = node_id or (winner or {}).get("id")
    by_id = {e.get("id"): e for e in entries if e.get("id")}
    subject = by_id.get(subject_id) if subject_id else None

    asked: dict = {}
    if node_id:
        rank = next(
            (i for i, e in enumerate(survivors) if e.get("id") == node_id), None
        )
        asked = {
            "id": node_id,
            "known": node_id in by_id,
            "dropped_by": cascade.reason_for(node_id),
            "rank": rank,
            # A node in neither place was never a candidate: not `ready`, or
            # already carrying completed_at. Reported as its own answer rather
            # than as a silent absence.
            "never_a_candidate": (
                node_id in by_id
                and rank is None
                and cascade.reason_for(node_id) is None
            ),
        }
        if asked["never_a_candidate"]:
            asked["status"] = by_id[node_id].get("status")

    routing = routing_for(subject)
    armed, rank_source = adv._auto_continue_resolve()

    return {
        "selection": {
            "pool": pool,
            "drops": [{"filter": n, "dropped": d} for n, d in cascade.drops],
            "survivors": len(survivors),
            "head": [
                {
                    "id": e.get("id"),
                    "priority": e.get("priority"),
                    "difficulty": e.get("difficulty"),
                    "project": e.get("project"),
                    "parent": e.get("parent"),
                    "title": e.get("title"),
                }
                for e in survivors[:top]
            ],
            "why": {f.name: f.why for f in filters},
        },
        "asked": asked,
        # Routing first: the grid picks the harness, and the harness decides
        # WHICH provider lane the spawn would be counted against. Reporting a
        # lane resolved from the config default the grid was about to override
        # would name the wrong cap.
        "gates": [
            g.as_dict()
            for g in gates_for(subject, (routing.get("candidate") or {}).get("harness"))
        ],
        "routing": routing,
        "decision": {
            "would_dispatch": subject_id if subject is not None else None,
            "armed": armed,
            "armed_rank": rank_source,
            "note": (
                "advance is DISARMED, so nothing above would run automatically. "
                "This report is a dry run of the pipeline, not a record of a "
                "decision advance made."
            )
            if not armed
            else None,
        },
    }


def render_report(report: dict) -> str:
    """The four sections as text. Section order is the operator's question order:
    which node, which gate, which lane, and only then what advance would do."""
    out: list[str] = []
    sel = report["selection"]
    out.append(f"SELECTION  {sel['pool']} candidates -> {sel['survivors']} eligible")
    for row in sel["drops"]:
        if row["dropped"]:
            why = sel["why"].get(row["filter"], "")
            out.append(f"  -{row['dropped']:<5} {row['filter']:<18} {why}")
        else:
            out.append(f"  -{0:<5} {row['filter']:<18}")
    if sel["head"]:
        out.append("  ranked head:")
        for i, e in enumerate(sel["head"]):
            marker = "->" if i == 0 else "  "
            out.append(
                f"   {marker} {i + 1}. {e['id']}  {e['priority'] or '-':<3} "
                f"{e['difficulty'] or '-':<7} {(e['title'] or '')[:60]}"
            )

    asked = report.get("asked") or {}
    if asked:
        out.append("")
        if not asked["known"]:
            out.append(f"ASKED  {asked['id']}: no such node")
        elif asked["dropped_by"]:
            why = sel["why"].get(asked["dropped_by"], "")
            out.append(f"ASKED  {asked['id']}: dropped by {asked['dropped_by']} - {why}")
        elif asked.get("never_a_candidate"):
            out.append(
                f"ASKED  {asked['id']}: never a candidate "
                f"(status {asked.get('status')}, not ready and not cold-dispatchable)"
            )
        else:
            out.append(f"ASKED  {asked['id']}: eligible, ranked {asked['rank'] + 1}")

    out.append("")
    _render_gates_routing_decision(report, out)

    d = report["decision"]
    out.append("")
    out.append("DECISION")
    out.append(
        f"  would dispatch: {d['would_dispatch'] or 'nothing (no eligible node)'}"
    )
    out.append(f"  armed: {d['armed']} (rank={d['armed_rank']})")
    if d.get("note"):
        out.append(f"  {d['note']}")
    return "\n".join(out)


def _render_gates_routing_decision(report: dict, out: list) -> None:
    """The GATES and ROUTING sections, shared by both cascade renderers."""
    out.append("GATES")
    for g in report["gates"]:
        out.append(
            Gate(
                g["name"], g["measured"], g["threshold"], g["verdict"], g["key"], g["note"]
            ).line()
        )

    routing = report["routing"]
    out.append("")
    out.append("ROUTING")
    inputs = routing.get("inputs") or {}
    if inputs:
        out.append(
            f"  inputs: difficulty={inputs.get('difficulty')} "
            f"priority={inputs.get('priority')} role={inputs.get('role')} "
            f"plan={'yes' if inputs.get('plan_path') else 'no'}"
        )
        capacity = inputs.get("capacity") or {}
        if capacity:
            out.append(
                "  capacity: "
                + ", ".join(f"{h}={s}" for h, s in sorted(capacity.items()))
            )
    for step in routing.get("chain") or ["(no chain: nothing to route)"]:
        out.append(f"  {step}")
    candidate = routing.get("candidate")
    out.append(
        f"  -> {candidate['harness']} {candidate['model']}"
        if candidate
        else "  -> grid declined; the spawn falls back to caller defaults"
    )


def build_lane_fill_report(
    *,
    epic: str,
    project: Optional[str] = None,
    node_id: Optional[str] = None,
    top: int = 5,
    max_dispatch: Optional[int] = None,
) -> dict:
    """``--explain --epic``: the fan-out the daemon's drain would make, as a READ.

    The daemon's only walk is ``active_backlog`` shelling ``advance --epic``,
    whose fan-out runs ``_ready_leaf_children`` through the converge gates.
    This preview used to call ``select_lane_fill(mission=epic)`` instead, which
    reaches ``fno backlog ready --mission <epic>`` - a ``mission_id`` field 0 of
    2320 graph nodes carry - so it reported an empty mission for every epic
    (x-7f1f). It now classifies the SAME children through the SAME pre-spawn
    gates the drain runs (``_converge_gate`` plus the epic fan-out's own
    no-project / unmapped-project / lane-cap), so it cannot describe a
    selection the drain would not make.

    Never dispatches, never claims, never emits.
    """
    # Same guard as build_report: the census attributes the preview's graph-side
    # reads to this function, so the tracker-owned refusal lives here too.
    from fno.graph.cli import _refuse_tracker_owned_on_external_backend

    _refuse_tracker_owned_on_external_backend("advance")

    from fno.backlog import advance as adv
    from fno.graph._intake import project_root_from_settings

    width = adv._spawn_headroom()
    ready = adv._ready_leaf_children(epic)

    # Classify every child through the fan-out's gates, in the drain's order.
    counts: dict[str, int] = {}
    reasons_by_id: dict[str, str] = {}
    excluded: list[dict] = []
    selected: list[dict] = []
    for child in ready:
        proj = child.get("project")
        if not proj:
            reason = "no-project"
        else:
            root = project_root_from_settings(proj)
            if not root:
                reason = "unmapped-project"
            else:
                reason = adv._converge_gate(child, root) or (
                    "lane-cap" if len(selected) >= width else None
                )
        if reason is not None:
            reasons_by_id[child["id"]] = reason
            counts[reason] = counts.get(reason, 0) + 1
            excluded.append({"id": child["id"], "reason": reason})
        else:
            selected.append(child)

    # The live run's overall --max binds after the spawn-gate width does, so a
    # dry run that ignored it would promise more dispatches than the run makes.
    stop: Optional[str] = "cap-full" if counts.get("lane-cap") else None
    if max_dispatch is not None and len(selected) > max_dispatch:
        denied = selected[max_dispatch:]
        selected = selected[:max_dispatch]
        counts["max-dispatch"] = len(denied)
        excluded.extend({"id": c["id"], "reason": "max-dispatch"} for c in denied)
        stop = "max-dispatch"

    ordered_names = [
        "no-project",
        "unmapped-project",
        "walker-live",
        "lane-cap",
        "max-dispatch",
    ] + [n for n in counts if n not in (
        "no-project", "unmapped-project", "walker-live", "lane-cap", "max-dispatch"
    )]
    drops = [{"filter": n, "dropped": counts.get(n, 0)} for n in ordered_names]

    selected_ids = [e["id"] for e in selected]
    asked: dict = {}
    if node_id:
        rank = next((i for i, nid in enumerate(selected_ids) if nid == node_id), None)
        reason = reasons_by_id.get(node_id)
        if reason:
            asked = {"id": node_id, "dropped_by": reason, "rank": None}
        elif rank is not None:
            asked = {"id": node_id, "dropped_by": None, "rank": rank}
        else:
            asked = {
                "id": node_id,
                "dropped_by": None,
                "rank": None,
                "never_a_candidate": True,
            }

    subject = selected[0] if selected else None
    routing = routing_for(subject)
    armed, rank_source = adv._auto_continue_resolve()

    return {
        "mode": "lane-fill",
        "epic": epic,
        "selection": {
            "width": width,
            "pool": len(ready),
            "drops": drops,
            "would_fill": [
                {
                    "id": e.get("id"),
                    "priority": e.get("priority"),
                    "difficulty": e.get("difficulty"),
                    "project": e.get("project"),
                    "title": e.get("title"),
                }
                for e in selected[:top]
            ],
            "stop": stop,
            "excluded": excluded,
        },
        "asked": asked,
        "gates": [
            g.as_dict()
            for g in gates_for(subject, (routing.get("candidate") or {}).get("harness"))
        ],
        "routing": routing,
        "decision": {
            "would_dispatch": selected_ids,
            "max_dispatch": max_dispatch,
            "armed": armed,
            "armed_rank": rank_source,
            "note": (
                "advance is DISARMED, so nothing above would run automatically. "
                "This report is a dry run of the pipeline, not a record of a "
                "decision advance made."
            )
            if not armed
            else None,
        },
    }


def render_lane_fill_report(report: dict) -> str:
    """The lane-fill cascade as text: the same four sections, the fill's drops."""
    out: list[str] = []
    sel = report["selection"]
    out.append(
        f"SELECTION  lane fill (epic {report['epic']})  "
        f"width {sel['width']}  {sel['pool']} ready -> {len(sel['would_fill'])} would fill"
    )
    for row in sel["drops"]:
        out.append(f"  -{row['dropped']:<5} {row['filter']}")
    if sel.get("slot_note"):
        out.append(f"  {sel['slot_note']}")
    out.append(f"  stop: {sel['stop']}")
    for row in sel["excluded"]:
        out.append(f"    excluded {row.get('id')}: {row.get('reason')}")
    if sel["would_fill"]:
        out.append("  would fill:")
        for i, e in enumerate(sel["would_fill"]):
            marker = "->" if i == 0 else "  "
            out.append(
                f"   {marker} {i + 1}. {e['id']}  {e['priority'] or '-':<3} "
                f"{e['difficulty'] or '-':<7} {(e['title'] or '')[:60]}"
            )

    asked = report.get("asked") or {}
    if asked:
        out.append("")
        if asked.get("never_a_candidate"):
            out.append(f"ASKED  {asked['id']}: not in this epic's ready list")
        elif asked.get("dropped_by"):
            out.append(f"ASKED  {asked['id']}: dropped by {asked['dropped_by']}")
        else:
            out.append(f"ASKED  {asked['id']}: selectable, fill rank {asked['rank'] + 1}")

    out.append("")
    _render_gates_routing_decision(report, out)

    d = report["decision"]
    out.append("")
    out.append("DECISION")
    if d["would_dispatch"]:
        out.append(f"  would dispatch {len(d['would_dispatch'])} lane(s): "
                   f"{', '.join(d['would_dispatch'])}")
    else:
        out.append("  would dispatch: nothing (fill selected no node)")
    if d.get("max_dispatch") is not None:
        out.append(f"  overall --max {d['max_dispatch']} honored")
    out.append(f"  armed: {d['armed']} (rank={d['armed_rank']})")
    if d.get("note"):
        out.append(f"  {d['note']}")
    return "\n".join(out)
