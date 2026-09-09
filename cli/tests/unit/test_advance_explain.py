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


def test_load_gate_row_reports_refuse_from_the_shared_decision(monkeypatch):
    """x-5e6f: the load row renders the gate's own verdict with its numbers.
    The old "over trigger; attribution decides" text never predicted an exit
    79, and a king was sent looking at the wrong symptom because of it."""
    from types import SimpleNamespace

    from fno.agents import spawn_gate
    from fno.backlog import explain

    monkeypatch.setattr(
        spawn_gate,
        "load_gate_decision",
        lambda *a, **k: (
            "fleet_cpu_share",
            "the fleet holds 96.20/12.00 cores; refusing to spawn (--force to bypass)",
            {"share": 8.017},
        ),
    )
    monkeypatch.setattr(
        spawn_gate,
        "_load_snapshot",
        lambda per_cpu: SimpleNamespace(
            spawn_load_status="exceeded",
            load_1m=255.3,
            load_cpu_count=12,
            load_ceiling=per_cpu * 12,
            max_load_per_cpu=per_cpu,
        ),
    )
    row = {g.name: g for g in explain._machine_gates()}["load-trigger"]
    assert row.verdict == "refuse"
    assert "refusing to spawn" in (row.note or "")


def test_preview_stops_when_the_load_gate_would_refuse(monkeypatch):
    """x-5e6f: the dry run passed every gate at load 255/120 while the real
    spawn exited 79. The preview now reads the gate's own decision."""
    _lane_fill_world(monkeypatch, [_ready_node("x-win")])
    from fno.agents import spawn_gate
    from fno.backlog.explain import build_lane_fill_report

    monkeypatch.setattr(
        spawn_gate,
        "load_gate_decision",
        lambda *a, **k: (
            "fleet_cpu_share",
            "the fleet holds 9.00/12.00 cores; refusing to spawn",
            {"share": 0.75},
        ),
    )
    report = build_lane_fill_report(epic="x-epic")
    assert report["selection"]["stop"] == "load-refused"
    assert report["decision"]["would_dispatch"] == ["x-win"]
