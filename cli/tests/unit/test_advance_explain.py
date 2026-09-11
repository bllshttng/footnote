"""The advance --explain report over the native selection leg.

The report narrates survivors plus drops straight out of the keeper's
``ready`` reply; nothing here re-derives a filter. The first-filter
attribution, drop counts, and per-filter narration the old cascade unit
tests asserted are frozen in tests/golden/backlog_ready/ and proven by
crates/fno-agents/tests/backlog_ready_parity.rs now.

Journal isolation: the dry run writes no event; the conftest per-module pin
sets FNO_EVENTS_PATH to a per-test tmp journal anyway, so a future emitter
cannot reach the live file from here.
"""
from __future__ import annotations

# ---------------------------------------------------------------------------
# --explain --epic: the daemon's lane-fill cascade (task 5.1, LD5)
#
# The daemon's only walk is active_backlog shelling `advance --epic`, which
# reaches select_lane_fill - a second selector beside next. An epic question
# answered with the next cascade is the second-selector lie, so the epic
# explain runs the fill itself and names ITS drops.
# ---------------------------------------------------------------------------

def _lane_fill_world(monkeypatch, ready, *, max_lanes=2, gates=None):
    """Hermetic seams for build_lane_fill_report (x-7f1f): the preview runs the
    drain's own selection (_ready_leaf_children through _converge_gate), so the
    seams pin exactly those."""
    from fno.backlog import advance as adv

    monkeypatch.setattr(adv, "_spawn_headroom", lambda *a, **k: max_lanes)
    monkeypatch.setattr(adv, "_ready_leaf_children", lambda epic: ready)
    monkeypatch.setattr(adv, "_auto_continue_resolve", lambda: (False, "config"))
    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda project: f"/mapped/{project}",
    )
    refusals = gates or {}

    def _gate(child, root):
        return refusals.get(child["id"])

    monkeypatch.setattr(adv, "_converge_gate", _gate)
    return adv


def _ready_node(nid, **kw):
    base = {"id": nid, "title": f"t-{nid}", "priority": "p1", "difficulty": "m",
            "project": "fno", "domain": "code", "plan_path": "plan.md"}
    base.update(kw)
    return base


def test_epic_explain_reports_the_children_the_drain_would_dispatch(monkeypatch):
    """x-7f1f: the preview classifies through the drain's own gates, so a node
    the drain would refuse for walker-live is a drop here too - under the
    drain's name, never the old lane-fill selector's vocabulary."""
    _lane_fill_world(
        monkeypatch,
        [_ready_node("x-win"), _ready_node("x-walker")],
        gates={"x-walker": "walker-live"},
    )
    from fno.backlog.explain import build_lane_fill_report

    report = build_lane_fill_report(epic="x-epic")
    assert report["mode"] == "lane-fill"
    drops = {d["filter"]: d["dropped"] for d in report["selection"]["drops"]}
    assert drops["walker-live"] == 1
    assert drops["lane-cap"] == 0
    assert "in-flight-collision" not in drops
    assert "unevaluated" not in drops
    assert [e["id"] for e in report["selection"]["would_fill"]] == ["x-win"]


def test_epic_explain_names_the_gate_that_dropped_the_asked_node(monkeypatch):
    _lane_fill_world(
        monkeypatch,
        [_ready_node("x-win"), _ready_node("x-walker")],
        gates={"x-walker": "walker-live"},
    )
    from fno.backlog.explain import build_lane_fill_report

    asked = build_lane_fill_report(epic="x-epic", node_id="x-walker")["asked"]
    assert asked["dropped_by"] == "walker-live"

    winner = build_lane_fill_report(epic="x-epic", node_id="x-win")["asked"]
    assert winner["rank"] == 0

    stranger = build_lane_fill_report(epic="x-epic", node_id="x-elsewhere")["asked"]
    assert stranger["never_a_candidate"] is True


def test_epic_explain_renders_no_next_cascade(monkeypatch):
    """The rendered text carries the drain's gate names and none of the next cascade."""
    _lane_fill_world(
        monkeypatch,
        [_ready_node("x-win"), _ready_node("x-walker")],
        gates={"x-walker": "walker-live"},
    )
    from fno.backlog.explain import build_lane_fill_report, render_lane_fill_report

    text = render_lane_fill_report(build_lane_fill_report(epic="x-epic"))
    assert "lane fill" in text
    assert "walker-live" in text
    assert "unmerged-open-pr" not in text
    assert "selection-guard" not in text


def test_cap_denied_counts_under_lane_cap(monkeypatch):
    """x-7f1f: the fan-out's width is the spawn-gate headroom; picks beyond it
    are lane-cap drops, the same reason the live pass emits."""
    _lane_fill_world(
        monkeypatch,
        [_ready_node("x-a"), _ready_node("x-b"), _ready_node("x-c")],
        max_lanes=1,
    )
    from fno.backlog.explain import build_lane_fill_report

    report = build_lane_fill_report(epic="x-epic")
    drops = {d["filter"]: d["dropped"] for d in report["selection"]["drops"]}
    assert report["selection"]["stop"] == "cap-full"
    assert drops["lane-cap"] == 2
    assert [e["id"] for e in report["selection"]["would_fill"]] == ["x-a"]


def test_unmapped_and_projectless_children_get_the_drains_own_drops(monkeypatch):
    from fno.backlog import advance as adv

    _lane_fill_world(
        monkeypatch,
        [_ready_node("x-noproj", project=None), _ready_node("x-orphan")],
    )
    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings", lambda project: None
    )
    from fno.backlog.explain import build_lane_fill_report

    report = build_lane_fill_report(epic="x-epic")
    drops = {d["filter"]: d["dropped"] for d in report["selection"]["drops"]}
    assert drops["no-project"] == 1
    assert drops["unmapped-project"] == 1
    assert [e["id"] for e in report["selection"]["would_fill"]] == []
    assert {r["id"] for r in report["selection"]["excluded"]} == {"x-noproj", "x-orphan"}
    assert adv  # the seams patched the drain module, not a copy


def test_advance_explain_epic_routes_to_the_fill_not_the_next_cascade(monkeypatch):
    """The CLI wiring: --explain --epic never builds the next cascade."""
    from fno.backlog import explain

    calls = {"lane_fill": 0, "next": 0}

    def _fake_fill(**kw):
        calls["lane_fill"] += 1
        return {"mode": "lane-fill", "epic": kw.get("epic"), "selection": {},
               "asked": {}, "gates": [], "routing": {}, "decision": {}}

    def _fail_next(**kw):
        calls["next"] += 1
        raise AssertionError("next cascade built for an epic question")

    monkeypatch.setattr(explain, "build_lane_fill_report", _fake_fill)
    monkeypatch.setattr(explain, "render_lane_fill_report", lambda r: "LANE FILL")
    monkeypatch.setattr(explain, "build_report", _fail_next)

    import fno.graph.cli as gcli

    monkeypatch.setattr(
        "fno.backlog.explain.build_lane_fill_report", _fake_fill
    )
    monkeypatch.setattr(gcli, "_display_entries", lambda *a, **k: [])
    from fno.cli import app
    from typer.testing import CliRunner

    result = CliRunner().invoke(
        app, ["backlog", "advance", "--explain", "--epic", "x-epic"]
    )
    assert result.exit_code == 0, result.output
    assert "LANE FILL" in result.output
    assert calls["lane_fill"] == 1 and calls["next"] == 0


def test_overall_max_bounds_the_epic_explain_decision(monkeypatch):
    """A live `--max 1` dispatches one lane; the dry run must say the same."""
    _lane_fill_world(
        monkeypatch,
        [_ready_node("x-a"), _ready_node("x-b"), _ready_node("x-c")],
        max_lanes=5,
    )
    from fno.backlog.explain import build_lane_fill_report, render_lane_fill_report

    report = build_lane_fill_report(epic="x-epic", max_dispatch=1)
    assert len(report["selection"]["would_fill"]) == 1
    assert report["decision"]["would_dispatch"] == ["x-a"]
    assert report["selection"]["stop"] == "max-dispatch"
    assert "overall --max 1" in render_lane_fill_report(report)
    # A max-denied node was a candidate: the report must name the filter that
    # dropped it, never claim it was never a candidate.
    asked = build_lane_fill_report(epic="x-epic", max_dispatch=1, node_id="x-b")["asked"]
    assert asked["dropped_by"] == "max-dispatch"


def test_cpu_share_row_reports_refuse_from_the_shared_decision(monkeypatch):
    """The cpu-share row renders the gate's own admission with its numbers
    and the backstop row sits beside it (x-7783: two axes, two rows)."""
    from fno.agents import spawn_gate
    from fno.backlog import explain
    from fno.footprint import Admission

    admission = Admission(
        verdict="undecidable",
        axis="fleet_cpu_share",
        reason=(
            "spawn-gate: the fleet's CPU share cannot be decided: attributed "
            "17.5% of capacity, up to 60.0% with rows unattributed"
        ),
        share_low=0.175,
        share_high=0.6,
        bound="upper",
        fleet_cores=2.1,
        machine_cores=7.2,
        capacity_cores=12.0,
        ceiling=0.5,
        gap="3 pidless row(s)",
        load_15m=45.0,
        backstop=480.0,
    )
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: admission)
    rows = {g.name: g for g in explain._machine_gates()}
    assert rows["cpu-share"].verdict == "refuse"
    assert "cannot be decided" in (rows["cpu-share"].note or "")
    assert rows["cpu-share"].measured == "2.10/12.00 cores"
    assert rows["load-backstop"].measured == "45.0"
    assert rows["load-backstop"].threshold == "480.0"
    assert rows["load-backstop"].verdict == "pass"


def test_preview_stops_when_the_cpu_axis_would_refuse(monkeypatch):
    """The dry run passes no gate the real spawn would refuse on. The
    preview reads the gate's own admission."""
    _lane_fill_world(monkeypatch, [_ready_node("x-win")])
    from fno.agents import spawn_gate
    from fno.backlog.explain import build_lane_fill_report
    from fno.footprint import Admission

    admission = Admission(
        verdict="undecidable",
        axis="fleet_cpu_share",
        reason="spawn-gate: cannot decide",
        share_low=0.175,
        share_high=0.6,
        bound="upper",
        fleet_cores=2.1,
        machine_cores=7.2,
        capacity_cores=12.0,
        ceiling=0.5,
        gap="3 pidless row(s)",
        load_15m=45.0,
        backstop=480.0,
    )
    monkeypatch.setattr(spawn_gate, "_cpu_axis", lambda *a, **k: admission)
    report = build_lane_fill_report(epic="x-epic")
    assert report["selection"]["stop"] == "load-refused"
    assert report["decision"]["would_dispatch"] == ["x-win"]


# ---------------------------------------------------------------------------
# ROUTING derives the slot from the node's verb (x-4890)
#
# routing_for used to hand resolve_slot the literal "target", so the preview
# narrated the target slot for every node whatever its dispatch_verb - the
# exact mismatch that produced two wrong diagnoses. The dispatch grid pick
# derives the slot from the verb through advance's one wrapper; the preview
# derives through the SAME wrapper, so the two readers cannot drift.
# ---------------------------------------------------------------------------

def _explain_node(nid, **kw):
    base = {"id": nid, "title": f"t-{nid}", "priority": "p1", "difficulty": "medium",
            "project": "fno", "domain": "code", "plan_path": None}
    base.update(kw)
    return base


def _slot_world(monkeypatch):
    """Spy resolve_slot, the one resolver both the preview and the dispatch
    grid pick call. The fake answers per verb so a hardcoded verb is visible
    in the chain, the candidate, and the grid pick's model."""
    from fno import route_resolve

    calls = []

    def _fake_slot(verb, node, capacity, **kw):
        calls.append(verb)
        return (
            {"harness": "claude", "model": f"model-for-{verb}"},
            [f"slot agents.profiles.{verb} lanes walked in declared order"],
            "armed",
        )

    monkeypatch.setattr(route_resolve, "resolve_inventory", lambda: object())
    monkeypatch.setattr(route_resolve, "runtime_capacity", lambda inventory=None: {})
    monkeypatch.setattr(route_resolve, "resolve_slot", _fake_slot)
    return calls


def test_explain_routes_a_blueprint_verb_node_through_the_blueprint_slot(monkeypatch):
    calls = _slot_world(monkeypatch)
    from fno.backlog.explain import routing_for

    routing = routing_for(_explain_node("x-f188", dispatch_verb="/fno:blueprint"))
    assert calls == ["blueprint"]
    assert routing["candidate"]["model"] == "model-for-blueprint"
    assert any("agents.profiles.blueprint" in s for s in routing["chain"])


def test_explain_routes_a_target_verb_node_through_the_target_slot(monkeypatch):
    """Not a constant swap: a target verb, and no verb at low difficulty,
    still walk the target profile."""
    calls = _slot_world(monkeypatch)
    from fno.backlog.explain import routing_for

    routing_for(_explain_node("x-t", dispatch_verb="/fno:target", difficulty="low"))
    routing_for(_explain_node("x-noverb", difficulty="low"))
    assert calls == ["target", "target"]


def test_explain_renders_the_blueprint_profile_for_a_blueprint_node(monkeypatch):
    _slot_world(monkeypatch)
    from fno.backlog.explain import _render_gates_routing_decision, routing_for

    out = []
    _render_gates_routing_decision(
        {"gates": [], "routing": routing_for(_explain_node("x-f188", dispatch_verb="/fno:blueprint"))},
        out,
    )
    text = "\n".join(out)
    assert "agents.profiles.blueprint" in text
    assert "agents.profiles.target" not in text


def test_explain_and_the_dispatch_grid_pick_name_the_same_model(monkeypatch):
    """The acceptance pair: the model ROUTING names equals the model the
    dispatch path resolves for the same node, on both verbs."""
    _slot_world(monkeypatch)
    from fno.backlog import advance as adv
    from fno.backlog.explain import routing_for

    for verb, difficulty in (("/fno:blueprint", "medium"), ("/fno:target", "low")):
        node = _explain_node("x-parity", dispatch_verb=verb, difficulty=difficulty)
        explained = routing_for(node)["candidate"]["model"]
        _h, model, _r, _a, why = adv._grid_lane_for(
            node, model=None, provider=None, verb=adv._node_effective_verb(node)
        )
        assert why is None
        assert explained == model
        assert explained == f"model-for-{verb.lstrip('/').split(':')[-1]}"


def test_explain_reports_an_unanswerable_verb_instead_of_the_target_slot(monkeypatch):
    """A node no lifecycle rung answers gets the refusal in the chain, never a
    silent walk of the target profile."""
    calls = _slot_world(monkeypatch)
    from fno.backlog.explain import routing_for

    routing = routing_for(_explain_node("x-undecided", difficulty=None))
    assert calls == []
    assert routing["candidate"] is None
    assert "verb unresolved" in routing["chain"][0]


# ---------------------------------------------------------------------------
# The abandoned-do-row settle arm (x-f714): two positive controls in one file.
# The gone arm proves the instrument FIRES (row reaped, node leaves
# in_progress, advance names it a candidate again); the held arm proves it can
# REFUSE (fresh transcript keeps the row, report stamps held). Neither is
# trusted alone.
# ---------------------------------------------------------------------------

import json as _json
from datetime import datetime as _dt, timedelta as _td, timezone as _tz

from typer.testing import CliRunner as _CliRunner

_AB_SID_GONE = "9f06a492-1111-4222-8333-444455556666"
_AB_SID_LIVE = "aa5b6c93-1111-4222-8333-444455556666"


def _ab_world(tmp_path, monkeypatch, sid, age_hours, node_id="x-abt0001"):
    """A hermetic graph with ONE node whose only open do row names `sid`,
    plus a real fixture transcript for that session."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    import fno.graph.cli as gcli

    monkeypatch.setattr(gcli, "_live_claimed_node_ids", lambda **k: set())
    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", lambda **k: {})

    stamp = (_dt.now(_tz.utc) - _td(hours=age_hours)).strftime("%Y-%m-%dT%H:%M:%SZ")
    root = tmp_path / "projects" / "-some-worktree"
    root.mkdir(parents=True)
    record = {
        "type": "assistant",
        "timestamp": stamp,
        "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "<promise>done</promise>"}]},
    }
    (root / f"{sid}.jsonl").write_text(_json.dumps(record) + "\n")
    import fno.provenance.resolver as resolver

    monkeypatch.setattr(resolver, "_DEFAULT_PROJECTS_ROOT", tmp_path / "projects")

    g.write_text(_json.dumps({"entries": [{
        "id": node_id, "title": "abandoned arm", "priority": "p2",
        "project": "fno", "domain": "code", "cwd": "/some/worktree",
        "status": "in_progress",
        "sessions": [{"phase": "do", "harness": "claude", "session_id": sid,
                      "started_at": "2026-09-09T15:46:29Z"}],
    }]}))
    return g


def _ab_maintain_apply(monkeypatch):
    from fno.cli import app

    result = _CliRunner().invoke(
        app, ["backlog", "maintain", "--apply", "--no-validity"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    return result


def test_abandoned_arm_row_settles_and_advance_names_the_node_a_candidate(
    tmp_path, monkeypatch
):
    """AC1-HP + AC2-HP: quiet transcript past the bar -> the apply receipt
    reads row_removed true with status_after idea, and advance --explain
    answers with a candidate line, never `never a candidate`. A planless
    idea with no intake difficulty now reads as an attributed selection
    drop, so the answer narrates the drop instead of an eligible rank."""
    g = _ab_world(tmp_path, monkeypatch, _AB_SID_GONE, age_hours=72)
    result = _ab_maintain_apply(monkeypatch)
    assert "row_removed true" in result.output
    assert "status_after idea" in result.output

    entries = _json.loads(g.read_text())["entries"]
    assert entries[0]["sessions"] == []
    assert entries[0]["status"] == "idea"

    from fno.cli import app

    explain = _CliRunner().invoke(
        app, ["backlog", "advance", "--explain", "--explain-node", "x-abt0001"],
        catch_exceptions=False,
    )
    assert explain.exit_code == 0, explain.output
    assert "never a candidate" not in explain.output
    assert "x-abt0001" in explain.output
    assert "dropped by no-difficulty" in explain.output


def test_held_arm_fresh_transcript_keeps_the_row_open(tmp_path, monkeypatch):
    """AC3-HP: a last event inside the bar holds the row - report stamps held
    with the active-transcript reason, and the node still carries the open do
    row for that exact session."""
    g = _ab_world(tmp_path, monkeypatch, _AB_SID_LIVE, age_hours=0)
    result = _ab_maintain_apply(monkeypatch)
    assert "held" in result.output
    assert "transcript active" in result.output

    entries = _json.loads(g.read_text())["entries"]
    rows = entries[0]["sessions"]
    assert len(rows) == 1
    assert rows[0]["session_id"] == _AB_SID_LIVE
    assert "ended_at" not in rows[0]
    assert entries[0]["status"] == "in_progress"
