"""Why this node and not that one: the selection, made answerable.

`fno backlog advance --explain` narrates the selection the native leg makes:
survivors in selection order plus per-node drops, read straight out of the
keeper's ``ready`` reply (``backlog_ready::select``). The narrowing cascade
itself lives in one place, the Rust leg; this module renders its answer
instead of recomputing it, so an explanation cannot disagree with a
selection.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional, cast

from fno.graph.store import ready as store_ready
from fno.graph._intake import repo_root


# The cascade's shipped filter order, for stable drop-count rendering.
FILTER_ORDER: "list[str]" = [
    "roadmap",
    "mission",
    "parent-scope",
    "project",
    "live-claim",
    "unmerged-open-pr",
    "container",
    "batched",
    "selection-guard",
]

# Why a node dropped, in terms of what the operator can do about it. The
# selection-guard row is rendered with the drop's inner reason
# (dead-ancestor:<id>, stale-quarantine, design-stage, idea-stage,
# contained:<id>, or the dispatch-hold guard reason) when one is attached.
FILTER_WHY: "dict[str, str]" = {
    "roadmap": "not on the requested roadmap",
    "mission": "not in the requested mission",
    "parent-scope": "not a descendant of the requested epic",
    "project": "belongs to another project (pass --all to widen, or --project)",
    "live-claim": "a live session already holds node:<id>; check `fno agents claim status`",
    "unmerged-open-pr": "already carries a PR that has not merged; the work is in review, not waiting",
    "container": "an epic is never built directly; its work lives in its children",
    "batched": "committed to an open batch; it ships via the batch PR",
    "selection-guard": "under a dead ancestor, contained elsewhere, undesigned, or stale past the quarantine window",
}


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


#: Sentinel for "sample one"; None means the shared read ran and found the
#: gate unreadable.
_UNSAMPLED: object = object()


def _explain_load_decision() -> "Optional[tuple[str, str, dict]]":
    """One load-gate decision per report build, or None when unreadable.

    The gates row and the stop share the sample: two footprint reads on an
    already-loaded box is the preview costing more than the thing it previews.
    """
    try:
        from fno.agents.spawn_gate import load_gate_decision
        from fno.config import load_settings

        agents = load_settings().agents
        return load_gate_decision(
            float(agents.max_load_per_cpu),
            float(agents.max_fleet_cpu_share),
            float(agents.hard_max_load_per_cpu),
        )
    except Exception:  # noqa: BLE001 - an unreadable preview gate holds no opinion
        return None


def gates_for(
    node: Optional[dict],
    grid_harness: Optional[str] = None,
    load_decision: object = _UNSAMPLED,
) -> list[Gate]:
    """Every gate advance would consult for ``node``, each measured.

    Calls the measurement functions only - never ``preflight_gate``, which
    acquires the spawn mutex and a worker slot. An explain that queued behind
    the real gate would change the fleet it is describing.

    ``grid_harness`` is the harness the capacity grid picked, so the provider
    lane reported is the one the spawn would actually be counted against rather
    than the config default the grid was about to override.

    ``load_decision`` is a decision a caller already sampled for this report
    (one footprint read per build); leave it unsampled to read fresh.
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

    out.extend(_machine_gates(load_decision))
    return out


def _max_live() -> int:
    from fno.config import load_settings

    return int(load_settings().agents.max_live)


def _machine_gates(load_decision: object = _UNSAMPLED) -> list[Gate]:
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
        from fno.agents.spawn_gate import (
            _LOAD_REFUSAL_REASONS,
            _load_snapshot,
            load_gate_decision,
        )

        snapshot = _load_snapshot(per_cpu)
        decision = load_gate_decision(per_cpu) if load_decision is _UNSAMPLED else cast(
            "Optional[tuple[str, str, dict]]", load_decision
        )
    except Exception as exc:  # noqa: BLE001
        out.append(_unreadable("load-trigger", exc, key="agents.max_load_per_cpu"))
    else:
        if snapshot.spawn_load_status == "unavailable":
            out.append(
                _unreadable(
                    "load-trigger",
                    RuntimeError("load average unreadable"),
                    key="agents.max_load_per_cpu",
                )
            )
        else:
            refusing = decision is not None and decision[0] in _LOAD_REFUSAL_REASONS
            out.append(
                Gate(
                    "load-trigger",
                    "-" if snapshot.load_1m is None else f"{snapshot.load_1m:.1f}",
                    f"{snapshot.load_ceiling:.1f} ({per_cpu:g} x {snapshot.load_cpu_count} cpu)",
                    # Same decision function the real gate runs, so the dry
                    # run cannot pass a box the spawn would refuse.
                    "refuse" if refusing else "pass",
                    key="agents.max_load_per_cpu",
                    note=None if decision is None else decision[1],
                )
            )
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
    """What the slot resolver picks for ``node``, and from which inputs.

    The chain is RECOVERED, not constructed: `route_resolve.resolve_slot`
    already returns ``(candidate, chain)`` whose last element is its terminal
    reason. The strings are the existing receipt vocabulary and are surfaced
    verbatim - reformatting them would fork it.
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
        candidate, chain = route_resolve.resolve_slot(
            "target",
            node,
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
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    # One call into the native leg. The narration reads the reply's drops;
    # nothing here re-derives a filter.
    result = store_ready(project=project, all=project is None, repo_root=repo_root())
    survivors = result["rows"]
    drops = result["drops"]
    drop_by_id = {d["id"]: d for d in drops if isinstance(d, dict) and d.get("id")}

    pool = len(survivors) + len(drops)
    counts: dict = {}
    for d in drops:
        name = d.get("filter") or "unknown"
        counts[name] = counts.get(name, 0) + 1
    drop_rows = [{"filter": name, "dropped": counts.get(name, 0)} for name in FILTER_ORDER]

    winner = survivors[0] if survivors else None
    subject_id = node_id or (winner or {}).get("id")
    by_id = {e.get("id"): e for e in read_graph(graph_json()) if e.get("id")}
    subject = by_id.get(subject_id) if subject_id else None

    asked: dict = {}
    if node_id:
        rank = next(
            (i for i, e in enumerate(survivors) if e.get("id") == node_id), None
        )
        dropped = drop_by_id.get(node_id)
        asked = {
            "id": node_id,
            "known": node_id in by_id,
            "dropped_by": (dropped or {}).get("filter"),
            "drop_reason": (dropped or {}).get("reason"),
            "rank": rank,
            # A node in neither place was never a candidate: not `ready`, or
            # already carrying completed_at. Reported as its own answer rather
            # than as a silent absence.
            "never_a_candidate": (
                node_id in by_id
                and rank is None
                and dropped is None
            ),
        }
        if asked["never_a_candidate"]:
            asked["status"] = by_id[node_id].get("status")

    routing = routing_for(subject)
    armed, rank_source = adv._auto_continue_resolve()

    return {
        "selection": {
            "pool": pool,
            "drops": drop_rows,
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
            "why": dict(FILTER_WHY),
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
            # A selection-guard drop narrates its inner reason
            # (dead-ancestor:<id>, stale-quarantine, ...) over the static why.
            reason = asked.get("drop_reason")
            if asked["dropped_by"] == "selection-guard" and reason:
                out.append(f"ASKED  {asked['id']}: dropped by {reason}")
            else:
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
        reason: Optional[str]
        proj = child.get("project")
        if not proj:
            reason = "no-project"
        else:
            root = project_root_from_settings(proj)
            if not root:
                reason = "unmapped-project"
            elif len(selected) >= width:
                # The drain checks the cap BEFORE the converge gates
                # (advance_epic), so the preview must name the same drop first.
                reason = "lane-cap"
            else:
                reason = adv._converge_gate(child, root)
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
        for child in denied:
            # Recorded for `asked` too, or a max-denied node would report
            # never_a_candidate instead of the filter that dropped it.
            reasons_by_id[child["id"]] = "max-dispatch"
        excluded.extend({"id": c["id"], "reason": "max-dispatch"} for c in denied)
        stop = "max-dispatch"

    # The load gate refuses machine-wide; a preview that left stop empty
    # would promise a dispatch the real spawn refuses. One decision sample
    # feeds both this stop and the gates row below.
    from fno.agents.spawn_gate import _LOAD_REFUSAL_REASONS

    load_decision = _explain_load_decision()
    if stop is None and load_decision is not None and load_decision[0] in _LOAD_REFUSAL_REASONS:
        stop = "load-refused"

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
            for g in gates_for(
                subject, (routing.get("candidate") or {}).get("harness"), load_decision
            )
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
