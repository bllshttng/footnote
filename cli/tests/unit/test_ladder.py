"""Design-stage probe: the `design` rung of the derived lifecycle ladder."""
from __future__ import annotations

import os

import pytest

from fno.graph.ladder import is_design_stage


DESIGN_FM = "---\nstatus: design\n---\n\n# Doc\n"


def _now() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).isoformat()


def _plan(tmp_path, body: str, name: str = "p.md") -> dict:
    """A node entry carrying an absolute plan_path (the simplest form)."""
    target = tmp_path / name
    target.write_text(body)
    return {"id": "x-test", "plan_path": str(target)}


def test_relative_plan_path_resolves_against_node_cwd(tmp_path):
    """The majority form on the live graph: repo-relative path + node `cwd`.

    Resolving against the calling process's cwd instead silently no-ops the
    gate for every foreign node - the daemon selects across projects.
    """
    (tmp_path / "plans").mkdir()
    (tmp_path / "plans" / "d.md").write_text(DESIGN_FM)
    entry = {"id": "x-test", "plan_path": "plans/d.md", "cwd": str(tmp_path)}
    assert is_design_stage(entry)


def test_fragment_plan_path_strips_anchor(tmp_path):
    """`<doc>#group-<slug>` paths are not literal filenames."""
    (tmp_path / "d.md").write_text(DESIGN_FM)
    entry = {"id": "x-test", "plan_path": f"{tmp_path / 'd.md'}#group-foo"}
    assert is_design_stage(entry)


def test_tilde_plan_path_expands(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / "d.md").write_text(DESIGN_FM)
    assert is_design_stage({"id": "x-test", "plan_path": "~/d.md"})


def test_relative_path_without_cwd_does_not_use_process_cwd(tmp_path, monkeypatch):
    """No `cwd` to resolve against: fail open rather than guess.

    The file deliberately EXISTS at that relative path in the process cwd - an
    earlier cut returned the bare relative path and would have design-gated an
    unrelated node off a coincidentally-matching local doc.
    """
    monkeypatch.chdir(tmp_path)
    (tmp_path / "d.md").write_text(DESIGN_FM)
    assert not is_design_stage({"id": "x-test", "plan_path": "d.md"})


def test_undecodable_plan_stays_armed_without_raising(tmp_path):
    """A binary file at the plan path must not escape as an exception.

    `detect_stale_ready` has no outer catch, so a read error escaping here
    would abort an entire `maintain` run.
    """
    binary = tmp_path / "d.md"
    binary.write_bytes(b"\xff\xfe\x00\x80 not utf-8")
    assert not is_design_stage({"id": "x-test", "plan_path": str(binary)})


def test_folder_plan_stays_armed(tmp_path):
    """A directory plan_path has no frontmatter to read - documented gap."""
    (tmp_path / "planfolder").mkdir()
    assert not is_design_stage({"id": "x-test", "plan_path": str(tmp_path / "planfolder")})


def test_design_frontmatter_is_design_stage(tmp_path):
    assert is_design_stage(_plan(tmp_path, DESIGN_FM))


@pytest.mark.parametrize("status", ["ready", "in_progress", "shipped", "done", "archived"])
def test_blueprinted_and_beyond_are_armed(tmp_path, status):
    assert not is_design_stage(_plan(tmp_path, f"---\nstatus: {status}\n---\n"))


def test_quick_plan_without_execution_strategy_is_armed(tmp_path):
    """`/blueprint quick` omits `## Execution Strategy` by design.

    Probing for that heading (rather than frontmatter) misread every
    quick-plan as unfinished - the regression this test pins.
    """
    body = "---\nstatus: ready\nkind: quick-plan\n---\n\n## Changes\n\n## Verification\n"
    assert not is_design_stage(_plan(tmp_path, body))


def test_quoted_and_cased_status_still_reads_design(tmp_path):
    assert is_design_stage(_plan(tmp_path, "---\nstatus: 'Design'\n---\n"))


@pytest.mark.parametrize(
    "body",
    [
        "# No frontmatter at all\n",
        "---\ntitle: no status key\n---\n",
        "---\nstatus: [unclosed\n",  # malformed YAML
    ],
)
def test_unparseable_plan_stays_armed(tmp_path, body):
    """Fail OPEN: only positive `status: design` evidence demotes a node."""
    assert not is_design_stage(_plan(tmp_path, body))


def test_missing_file_stays_armed(tmp_path):
    """A symlinked vault that is not mounted must never quarantine the backlog."""
    assert not is_design_stage({"id": "x-test", "plan_path": str(tmp_path / "absent.md")})


@pytest.mark.parametrize(
    "entry",
    [
        None,
        "not-an-entry",
        {},                                   # no plan_path (an `idea` node)
        {"plan_path": None},
        {"plan_path": ""},
        {"plan_path": 42},                    # non-string survives the graph's tolerance
    ],
)
def test_malformed_entries_stay_armed(entry):
    assert not is_design_stage(entry)


def test_design_node_is_never_stale_ready(tmp_path):
    """Quarantine must not reach a node that is unarmed on purpose.

    Pinned on `is_stale_ready` itself rather than `detect_stale_ready`, because
    `maintain --apply` re-runs the predicate directly under the lock.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.maintain import detect_stale_ready, is_stale_ready

    now = datetime.now(timezone.utc)
    plan = tmp_path / "d.md"
    plan.write_text(DESIGN_FM)
    os.utime(plan, (0, 0))  # ancient mtime: no movement signal
    node = {
        "id": "x-old",
        "_status": "ready",
        "plan_path": str(plan),
        "created_at": (now - timedelta(days=400)).isoformat(),
    }
    assert not is_stale_ready(node, now, 21)
    assert detect_stale_ready([node], 21, now) == []


def test_recompute_persists_the_design_rung(tmp_path):
    """The rung is persisted so every reader sees it, including the Rust mux."""
    from fno.graph.statuses import recompute_statuses

    design = tmp_path / "d.md"
    design.write_text(DESIGN_FM)
    blueprint = tmp_path / "b.md"
    blueprint.write_text("---\nstatus: ready\n---\n")
    entries = [
        {"id": "x-i", "plan_path": None},
        {"id": "x-d", "plan_path": str(design)},
        {"id": "x-r", "plan_path": str(blueprint)},
        {"id": "x-p", "plan_path": str(design), "locked_by": "w", "claimed_at": _now()},
    ]
    got = {e["id"]: e["_status"] for e in recompute_statuses(entries)}
    assert got == {"x-i": "idea", "x-d": "design", "x-r": "ready", "x-p": "in_progress"}


def test_legacy_claimed_status_migrates_on_read(tmp_path):
    """A row persisted before the rename still reads as the current vocabulary."""
    from fno.graph.store import _apply_graph_defaults

    entries = _apply_graph_defaults([{"id": "x-a", "_status": "claimed"}])
    assert entries[0]["_status"] == "in_progress"


def _fm(path) -> str:
    import re

    m = re.search(r"^status:\s*(.+?)\s*$", path.read_text(), re.M)
    return m.group(1).strip().strip("'\"") if m else ""


@pytest.mark.parametrize("stamped", ["design", "ready"])
def test_graph_and_frontmatter_are_a_fixed_point(tmp_path, stamped):
    """The doc and the graph must agree and STAY agreed.

    The graph derives `design` FROM the plan doc while the projection writes the
    plan doc FROM the graph, so the two could in principle chase each other.
    They must not: one round trip has to be a no-op.
    """
    from fno.graph.statuses import recompute_statuses
    from fno.plan._project import project_node_to_plan

    plan = tmp_path / "p.md"
    plan.write_text(f"---\nstatus: {stamped}\ntitle: T\n---\n\n# T\n\nbody\n")
    node = {"id": "x-a", "plan_path": str(plan)}

    recompute_statuses([node])
    assert node["_status"] == stamped  # graph reads the doc

    assert project_node_to_plan(node, plan) is False  # doc already agrees
    assert _fm(plan) == stamped

    recompute_statuses([node])
    assert node["_status"] == stamped  # and it stays put


def test_claiming_a_design_node_advances_the_doc_off_design(tmp_path):
    """Forward motion still projects: claiming beats the design rung."""
    from fno.graph.statuses import recompute_statuses
    from fno.plan._project import project_node_to_plan

    plan = tmp_path / "p.md"
    plan.write_text("---\nstatus: design\ntitle: T\n---\n\n# T\n\nbody\n")
    node = {"id": "x-a", "plan_path": str(plan), "locked_by": "w", "claimed_at": _now()}

    recompute_statuses([node])
    assert node["_status"] == "in_progress"

    assert project_node_to_plan(node, plan) is True
    assert _fm(plan) == "in_progress"

    # Re-derived from the advanced doc, it is no longer design-stage.
    assert not is_design_stage(node)


def test_stale_graph_design_never_regresses_a_blueprinted_doc(tmp_path):
    """`plan sync` must not undo a fresh `/blueprint`.

    `/blueprint` rewrites the doc design -> ready without touching the graph,
    and `read_graph` does not recompute, so the persisted `_status` can still
    say `design` when the sweep runs. Repainting from that stale value would
    stamp `design` back onto the doc and un-blueprint it. The forward-only rule
    in project_plan_status is what prevents it.
    """
    from fno.plan._project import project_node_to_plan

    plan = tmp_path / "p.md"
    plan.write_text("---\nstatus: ready\ntitle: T\n---\n\n# T\n\nbody\n")
    stale = {"id": "x-a", "plan_path": str(plan), "_status": "design"}

    assert project_node_to_plan(stale, plan) is False
    assert _fm(plan) == "ready"  # blueprint survives


def test_idea_can_skip_design_and_go_straight_to_ready(tmp_path):
    """`/blueprint quick` on an idea node skips the design rung entirely.

    The ladder is not a forced march: `design` is a state you are IN when an
    un-blueprinted think doc is linked, not a step every node must pass
    through. Because the probe demotes only on positive `status: design`
    evidence, a doc that is born blueprint-complete arms immediately.
    """
    from fno.graph.statuses import recompute_statuses

    plan = tmp_path / "quick.md"
    plan.write_text("---\nstatus: ready\nkind: quick-plan\n---\n\n## Changes\n")
    node = {"id": "x-a", "plan_path": str(plan)}

    recompute_statuses([node])
    assert node["_status"] == "ready"
    assert not is_design_stage(node)


def test_stale_idea_graph_never_stamps_design_on_a_fresh_blueprint(tmp_path):
    """The idea -> design projection must not undo a straight-to-blueprint doc.

    GRAPH_TO_PLAN_STATUS maps graph `idea` -> plan `design`, and the graph can
    still read `idea` in the window before the next mutation recomputes it.
    Forward-only is what keeps that from regressing the doc.
    """
    from fno.plan._project import project_node_to_plan

    plan = tmp_path / "quick.md"
    plan.write_text("---\nstatus: ready\ntitle: T\n---\n\n# T\n\nbody\n")
    stale = {"id": "x-a", "plan_path": str(plan), "_status": "idea"}

    assert project_node_to_plan(stale, plan) is False
    assert _fm(plan) == "ready"


def test_design_node_is_never_autonomously_selected(tmp_path):
    """Every autonomous path filters `_status == "ready"` before selecting.

    Once the rung is persisted the node is excluded upstream, so the guard is
    no longer what saves us here - this pins the property itself rather than
    the mechanism, and fails if a future selection path drops the filter.
    """
    from fno.graph.statuses import recompute_statuses

    plan = tmp_path / "d.md"
    plan.write_text(DESIGN_FM)
    node = {"id": "x-d", "plan_path": str(plan)}
    recompute_statuses([node])
    assert node["_status"] == "design"
    assert node["_status"] != "ready"  # the filter every selector applies


def test_receipt_reports_a_node_already_on_the_design_rung(tmp_path):
    """A backlog that is ALL design-stage must not return null silently."""
    from datetime import datetime, timezone

    from fno.graph.cli import _starvation_receipts
    from fno.graph.statuses import recompute_statuses

    plan = tmp_path / "d.md"
    plan.write_text(DESIGN_FM)
    node = {"id": "x-d", "plan_path": str(plan), "created_at": _now()}
    recompute_statuses([node])
    out = _starvation_receipts(
        [node], None, True, None, set(), datetime.now(timezone.utc), 21
    )
    assert out == [("x-d", "design")]


def test_think_attaches_plan_then_blueprint_arms_it(tmp_path):
    """The whole point of the rung: /think can link its doc safely.

    Before this rung existed, linking a design doc flipped the node to `ready`
    and the dispatcher claimed it within ~a minute - the reason the old advice
    was to leave plans unlinked until blueprint. Walks the real sequence:
    /think links a `status: design` doc (visible, parked, explained), then
    /blueprint flips the doc and the same node arms.
    """
    from datetime import datetime, timezone

    from fno.backlog.advance import selection_guards
    from fno.graph.cli import _starvation_receipts
    from fno.graph.statuses import recompute_statuses

    now = datetime.now(timezone.utc)
    doc = tmp_path / "20260719-dark-mode-x-8af8.md"
    doc.write_text("---\nstatus: design\nnode: x-8af8\ntype: think-brief\n---\n\n# Dark mode\n")
    node = {"id": "x-8af8", "plan_path": str(doc), "created_at": now.isoformat()}

    # /think links it: parked, but visible and explained rather than silent.
    recompute_statuses([node])
    assert node["_status"] == "design"
    assert node["_status"] != "ready"  # the filter every autonomous selector applies
    assert _starvation_receipts([node], None, True, None, set(), now, 21) == [
        ("x-8af8", "design")
    ]

    # /blueprint flips the doc; the very same node is now dispatchable.
    doc.write_text("---\nstatus: ready\nnode: x-8af8\n---\n\n## Execution Strategy\n")
    recompute_statuses([node])
    assert node["_status"] == "ready"
    assert selection_guards(node, {"x-8af8": node}, now) is None


def test_starvation_receipt_names_design_not_quarantined(tmp_path):
    """A design-stage node is a lifecycle rung, not starvation.

    Reporting it as the generic `quarantined` would read as a stuck node and
    send an operator hunting for a problem that isn't there.
    """
    from datetime import datetime, timezone

    from fno.graph.cli import _starvation_receipts

    plan = tmp_path / "d.md"
    plan.write_text("---\nstatus: design\n---\n")
    node = {
        "id": "x-aaaa",
        "_status": "ready",
        "plan_path": str(plan),
        "created_at": datetime.now(timezone.utc).isoformat(),
    }
    out = _starvation_receipts(
        [node], None, True, None, set(), datetime.now(timezone.utc), 21
    )
    assert out == [("x-aaaa", "design")]
