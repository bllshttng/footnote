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
        "status": "ready",
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
    got = {e["id"]: e["status"] for e in recompute_statuses(entries)}
    assert got == {"x-i": "idea", "x-d": "design", "x-r": "ready", "x-p": "in_progress"}


def test_recompute_rolls_container_status_up_from_real_child_edges(tmp_path):
    from fno.graph.statuses import recompute_statuses

    plan = tmp_path / "child.md"
    plan.write_text("---\nstatus: ready\n---\n")
    entries = [
        {"id": "x-parent", "type": "feature", "plan_path": None},
        {"id": "x-done", "parent": "x-parent", "completed_at": "2026-08-20T00:00:00Z"},
        {"id": "x-review", "parent": "x-parent", "plan_path": str(plan), "pr_number": 7},
        {"id": "x-open", "parent": "x-parent", "plan_path": str(plan)},
    ]

    recompute_statuses(entries)
    by_id = {entry["id"]: entry for entry in entries}
    assert by_id["x-parent"]["status"] == "in_progress"

    by_id["x-review"]["completed_at"] = "2026-08-20T00:01:00Z"
    by_id["x-open"]["completed_at"] = "2026-08-20T00:02:00Z"
    recompute_statuses(entries)
    assert by_id["x-parent"]["status"] == "done"


def test_recompute_preserves_explicit_container_terminal_marker(tmp_path):
    from fno.graph.statuses import recompute_statuses

    entries = [
        {"id": "x-parent", "deferred_at": "2026-08-20T00:00:00Z"},
        {"id": "x-child", "parent": "x-parent", "completed_at": "2026-08-20T00:00:00Z"},
    ]
    recompute_statuses(entries)
    assert entries[0]["status"] == "deferred"


def test_legacy_claimed_status_migrates_on_read(tmp_path):
    """A row persisted before the rename still reads as the current vocabulary."""
    from fno.graph.store import _apply_graph_defaults

    entries = _apply_graph_defaults([{"id": "x-a", "status": "claimed"}])
    assert entries[0]["status"] == "in_progress"


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
    assert node["status"] == stamped  # graph reads the doc

    assert project_node_to_plan(node, plan) is False  # doc already agrees
    assert _fm(plan) == stamped

    recompute_statuses([node])
    assert node["status"] == stamped  # and it stays put


def test_claiming_a_design_node_advances_the_doc_off_design(tmp_path):
    """Forward motion still projects: claiming beats the design rung."""
    from fno.graph.statuses import recompute_statuses
    from fno.plan._project import project_node_to_plan

    plan = tmp_path / "p.md"
    plan.write_text("---\nstatus: design\ntitle: T\n---\n\n# T\n\nbody\n")
    node = {"id": "x-a", "plan_path": str(plan), "locked_by": "w", "claimed_at": _now()}

    recompute_statuses([node])
    assert node["status"] == "in_progress"

    assert project_node_to_plan(node, plan) is True
    assert _fm(plan) == "in_progress"

    # Re-derived from the advanced doc, it is no longer design-stage.
    assert not is_design_stage(node)


def test_stale_graph_design_never_regresses_a_blueprinted_doc(tmp_path):
    """`plan sync` must not undo a fresh `/blueprint`.

    `/blueprint` rewrites the doc design -> ready without touching the graph,
    and `read_graph` does not recompute, so the persisted `status` can still
    say `design` when the sweep runs. Repainting from that stale value would
    stamp `design` back onto the doc and un-blueprint it. The forward-only rule
    in project_plan_status is what prevents it.
    """
    from fno.plan._project import project_node_to_plan

    plan = tmp_path / "p.md"
    plan.write_text("---\nstatus: ready\ntitle: T\n---\n\n# T\n\nbody\n")
    stale = {"id": "x-a", "plan_path": str(plan), "status": "design"}

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
    assert node["status"] == "ready"
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
    stale = {"id": "x-a", "plan_path": str(plan), "status": "idea"}

    assert project_node_to_plan(stale, plan) is False
    assert _fm(plan) == "ready"


def test_design_node_is_never_autonomously_selected(tmp_path):
    """Every autonomous path filters `status == "ready"` before selecting.

    Once the rung is persisted the node is excluded upstream, so the guard is
    no longer what saves us here - this pins the property itself rather than
    the mechanism, and fails if a future selection path drops the filter.
    """
    from fno.graph.statuses import recompute_statuses

    plan = tmp_path / "d.md"
    plan.write_text(DESIGN_FM)
    node = {"id": "x-d", "plan_path": str(plan)}
    recompute_statuses([node])
    assert node["status"] == "design"
    assert node["status"] != "ready"  # the filter every selector applies


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
    assert node["status"] == "design"
    assert node["status"] != "ready"  # the filter every autonomous selector applies
    assert _starvation_receipts([node], None, True, None, set(), now, 21) == [
        ("x-8af8", "design")
    ]

    # /blueprint flips the doc; the very same node is now dispatchable.
    doc.write_text("---\nstatus: ready\nnode: x-8af8\n---\n\n## Execution Strategy\n")
    recompute_statuses([node])
    assert node["status"] == "ready"
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
        "status": "ready",
        "plan_path": str(plan),
        "created_at": datetime.now(timezone.utc).isoformat(),
    }
    out = _starvation_receipts(
        [node], None, True, None, set(), datetime.now(timezone.utc), 21
    )
    assert out == [("x-aaaa", "design")]


# ---------------------------------------------------------------------------
# plan_rung + the two named policies (x-3571 wave 1)
# ---------------------------------------------------------------------------


def _undecodable(tmp_path, name: str = "bin.md"):
    target = tmp_path / name
    target.write_bytes(b"\xff\xfe\x00\x80not utf-8 at all")
    return {"id": "x-test", "plan_path": str(target)}


@pytest.mark.parametrize(
    "status,expected",
    [
        ("idea", "IDEA"),
        ("stub", "IDEA"),  # retired spelling, still on disk in old scaffolds
        ("design", "DESIGN"),
        ("ready", "READY"),
        ("in_progress", "IN_PROGRESS"),
        ("in_review", "IN_REVIEW"),
        ("shipped", "IN_REVIEW"),  # retired spelling
        ("done", "DONE"),
        ("superseded", "SUPERSEDED"),
        ("archived", "SUPERSEDED"),  # retired spelling
    ],
)
def test_every_vocabulary_word_maps_to_its_rung(tmp_path, status, expected):
    from fno.graph.ladder import Rung, plan_rung

    entry = _plan(tmp_path, f"---\nstatus: {status}\n---\n")
    assert plan_rung(entry) is getattr(Rung, expected)


def test_no_plan_path_is_NONE_not_IDEA(tmp_path):
    """Distinct on purpose: only NONE means there is nothing on disk to fill."""
    from fno.graph.ladder import Rung, plan_rung

    assert plan_rung({"id": "x-test"}) is Rung.NONE
    assert plan_rung({"id": "x-test", "plan_path": ""}) is Rung.NONE
    assert plan_rung(_plan(tmp_path, "---\nstatus: idea\n---\n")) is Rung.IDEA


def test_AC4_ERR_unreadable_is_distinguished_from_status_less(tmp_path):
    """The collapse `plan_rung` must not inherit.

    `_read_plan_frontmatter` returns {} for missing, unreadable, malformed AND
    status-less alike. Those last two must route to opposite failure policies,
    so a resolver built on it would be unable to tell them apart at all.
    """
    from fno.graph.ladder import Rung, plan_rung

    status_less = _plan(tmp_path, "---\ntitle: a plan\n---\n\n# Doc\n", name="q.md")
    undecodable = _undecodable(tmp_path)

    assert plan_rung(status_less) is not plan_rung(undecodable)
    assert plan_rung(status_less) is Rung.READY
    assert plan_rung(undecodable) is Rung.UNREADABLE


def test_an_absent_status_is_not_the_same_defect_as_a_stub_status(tmp_path):
    """Silence stays READY; only a pre-design WORD demotes.

    `status: stub` read as `ready` because it was in no vocabulary - that is the
    bug. A doc with no `status:` at all is the older, legitimate shape (most of
    a mature vault), and `fno backlog intake` on one must still yield a workable
    node. Demoting silence too would empty the board to fix a bug it never had.
    """
    from fno.graph.ladder import Rung, is_dispatchable, plan_rung

    silent = _plan(tmp_path, "---\ntitle: An older plan\n---\n\n# Body\n", "s.md")
    stubbed = _plan(tmp_path, "---\nstatus: stub\n---\n", "t.md")

    assert plan_rung(silent) is Rung.READY
    assert is_dispatchable(silent) is True
    assert plan_rung(stubbed) is Rung.IDEA
    assert is_dispatchable(stubbed) is False


def test_AC3_ERR_dispatch_fails_closed_on_unreadable(tmp_path):
    """An unreadable plan parks rather than launching a worker."""
    from fno.graph.ladder import Rung, is_dispatchable, plan_rung

    entry = _undecodable(tmp_path)
    assert plan_rung(entry) is Rung.UNREADABLE
    assert is_dispatchable(entry) is False


@pytest.mark.parametrize(
    "body",
    [
        "---\nstatus: design\n",  # frontmatter opened, never closed
        "---\nstatus: [unclosed\n---\n",  # malformed YAML
        "---\njust a scalar\n---\n",  # not a mapping
        "---\nstatus: brand_new_word\n---\n",  # newer vocabulary, or corrupt
    ],
)
def test_uncertain_documents_park(tmp_path, body):
    from fno.graph.ladder import Rung, is_dispatchable, plan_rung

    entry = _plan(tmp_path, body)
    assert plan_rung(entry) is Rung.UNREADABLE
    assert is_dispatchable(entry) is False


def test_missing_file_is_unreadable_and_not_dispatchable(tmp_path):
    from fno.graph.ladder import Rung, is_dispatchable, plan_rung

    entry = {"id": "x-test", "plan_path": str(tmp_path / "gone.md")}
    assert plan_rung(entry) is Rung.UNREADABLE
    assert is_dispatchable(entry) is False


@pytest.mark.parametrize("status", ["idea", "stub", "design"])
def test_AC2_EDGE_a_linked_pre_design_plan_is_not_dispatchable(tmp_path, status):
    """The case that read `ready` before this change."""
    from fno.graph.ladder import is_dispatchable

    entry = _plan(tmp_path, f"---\nstatus: {status}\n---\n")
    assert is_dispatchable(entry) is False


@pytest.mark.parametrize("status", ["ready", "in_progress", "in_review", "shipped"])
def test_dispatchable_set_matches_the_handoff_vocabulary(tmp_path, status):
    """Exactly what handoff.sh accepted before it delegated to the verb."""
    from fno.graph.ladder import is_dispatchable

    assert is_dispatchable(_plan(tmp_path, f"---\nstatus: {status}\n---\n")) is True


@pytest.mark.parametrize("status", ["done", "superseded"])
def test_terminal_plans_are_not_dispatchable(tmp_path, status):
    from fno.graph.ladder import is_dispatchable

    assert is_dispatchable(_plan(tmp_path, f"---\nstatus: {status}\n---\n")) is False


def test_a_plan_less_node_is_cold_dispatchable():
    """x-e24a: Rung.NONE is cold-dispatchable via is_cold_dispatchable - /target
    authors the plan. is_dispatchable stays False (no plan to launch against);
    the cold path is its own predicate, not an overload of _DISPATCHABLE."""
    from fno.graph.ladder import Rung, is_cold_dispatchable, is_dispatchable, plan_rung

    entry = {"id": "x-noplan", "status": "idea"}  # no plan_path
    assert plan_rung(entry) is Rung.NONE
    assert is_dispatchable(entry) is False
    assert is_cold_dispatchable(entry) is True


def test_is_cold_dispatchable_is_the_autonomous_drain_gate(tmp_path):
    """Admit status 'idea' AND rung NONE only (x-e24a).

    A linked-but-undesigned decompose stub (Rung.IDEA) stays excluded, and the
    status conjunct drops blocked / in_progress / ready plan-less nodes.
    """
    from fno.graph.ladder import is_cold_dispatchable

    # Plan-less idea -> admitted.
    assert is_cold_dispatchable({"id": "a", "status": "idea"}) is True
    # A linked undesigned doc (Rung.IDEA) is NOT admitted.
    stub = _plan(tmp_path, "---\nstatus: idea\n---\n\n# Child\n")
    stub["status"] = "idea"
    assert is_cold_dispatchable(stub) is False
    # The status conjunct excludes non-idea plan-less nodes.
    assert is_cold_dispatchable({"id": "b", "status": "blocked"}) is False
    assert is_cold_dispatchable({"id": "c", "status": "in_progress"}) is False
    assert is_cold_dispatchable({"id": "d", "status": "ready"}) is False


def test_plan_rung_never_raises_on_a_malformed_entry():
    from fno.graph.ladder import Rung, plan_rung

    for entry in (None, "a string", 42, [], {"plan_path": 7}):
        assert plan_rung(entry) in set(Rung)


def test_is_design_stage_is_now_one_rung_of_the_table(tmp_path):
    """The old name survives so its four callers do not churn."""
    from fno.graph.ladder import Rung, is_design_stage, plan_rung

    entry = _plan(tmp_path, DESIGN_FM)
    assert is_design_stage(entry) is (plan_rung(entry) is Rung.DESIGN) is True


# selection re-probes every undesigned rung, not just `design` (codex P1) ------


def _ready_row(plan_path: str) -> dict:
    """A graph row PERSISTED as `ready` whose doc may say otherwise."""
    from datetime import datetime, timezone

    return {
        "id": "x-stale01",
        "status": "ready",
        "plan_path": plan_path,
        "created_at": datetime.now(timezone.utc).isoformat(),
    }


@pytest.mark.parametrize(
    "status,expected",
    [("design", "design-stage"), ("idea", "idea-stage"), ("stub", "idea-stage")],
)
def test_a_stale_ready_row_is_re_probed_for_every_undesigned_rung(
    tmp_path, status, expected
):
    """The persisted status can lie; the live doc decides.

    `read_graph` does not recompute, so a doc rewritten down to `idea` (or an
    old scaffold still spelled `stub`) sits behind a `ready` row. A DESIGN-only
    probe waved those straight through to dispatch.
    """
    from datetime import datetime, timezone

    from fno.backlog.advance import selection_guards

    plan = tmp_path / "p.md"
    plan.write_text(f"---\nstatus: {status}\n---\n")
    node = _ready_row(str(plan))
    verdict = selection_guards(
        node, {node["id"]: node}, datetime.now(timezone.utc)
    )
    assert verdict == expected


def test_a_stale_ready_row_with_a_real_plan_still_selects(tmp_path):
    """The guard must not hold back a genuinely ready node."""
    from datetime import datetime, timezone

    from fno.backlog.advance import selection_guards

    plan = tmp_path / "r.md"
    plan.write_text("---\nstatus: ready\n---\n")
    node = _ready_row(str(plan))
    assert selection_guards(
        node, {node["id"]: node}, datetime.now(timezone.utc)
    ) is None


def test_a_missing_plan_remains_no_hold_signal(tmp_path):
    """A cross-project or unmounted plan has no declaration to validate here."""
    from datetime import datetime, timezone

    from fno.backlog.advance import selection_guards

    node = _ready_row(str(tmp_path / "gone.md"))
    assert selection_guards(
        node, {node["id"]: node}, datetime.now(timezone.utc)
    ) is None


def test_the_policy_set_is_the_one_selection_uses():
    """One definition of "undesigned", shared by the bool and the reason path."""
    from fno.graph.ladder import UNSELECTABLE_RUNGS, Rung

    assert UNSELECTABLE_RUNGS == frozenset({Rung.IDEA, Rung.DESIGN})


def _held_plan(tmp_path, *, set_by="king:119e3c52", name="held.md"):
    plan = tmp_path / name
    plan.write_text(
        "---\n"
        "status: ready\n"
        "dispatch_hold:\n"
        "  reason: Blocking review finding is unresolved\n"
        "  release_when: The finding is fixed and re-reviewed\n"
        "  review_on: 2026-08-20\n"
        f"  set_by: {set_by}\n"
        "---\n"
    )
    return plan


def test_dispatch_hold_is_attributable_and_remains_active_on_review_date(tmp_path):
    from fno.graph.ladder import DispatchHoldState, dispatch_hold

    hold = dispatch_hold(_plan(tmp_path, _held_plan(tmp_path).read_text()))
    assert hold.state is DispatchHoldState.HELD
    assert hold.reason == "Blocking review finding is unresolved"
    assert hold.release_when == "The finding is fixed and re-reviewed"
    assert hold.review_on == "2026-08-20"
    assert hold.set_by == "king:119e3c52"


@pytest.mark.parametrize(
    "declaration",
    [
        "dispatch_hold: blocked",
        "dispatch_hold:\n  reason: why",
        "dispatch_hold:\n  reason: why\n  release_when: fixed\n  review_on: soon\n  set_by: king",
        "dispatch_hold:\n  reason: '   '\n  release_when: fixed\n  review_on: 2026-08-20\n  set_by: king",
        "dispatch_hold:\n  reason: why\n  release_when: fixed\n  review_on: 2026-08-20\n  set_by: '   '",
    ],
)
def test_malformed_dispatch_hold_fails_closed(tmp_path, declaration):
    from fno.graph.ladder import DispatchHoldState, dispatch_hold

    entry = _plan(tmp_path, f"---\nstatus: ready\n{declaration}\n---\n")
    assert dispatch_hold(entry).state is DispatchHoldState.INVALID


def test_unreadable_bound_plan_fails_closed_for_hold_policy(tmp_path):
    from fno.graph.ladder import DispatchHoldState, dispatch_hold

    malformed = tmp_path / "malformed.md"
    malformed.write_text("---\nstatus: ready\ndispatch_hold: [\n")
    entry = {"id": "x-held", "plan_path": str(malformed)}
    assert dispatch_hold(entry).state is DispatchHoldState.INVALID


def test_dispatch_hold_walks_parent_and_contained_owner(tmp_path):
    from fno.graph.ladder import dispatch_hold_verdict

    owner_plan = _held_plan(tmp_path)
    owner = {"id": "x-owner", "plan_path": str(owner_plan)}
    parent = {"id": "x-parent", "parent": "x-owner"}
    child = {"id": "x-child", "contained_in": "x-parent"}
    by_id = {row["id"]: row for row in (owner, parent, child)}
    verdict = dispatch_hold_verdict(child, by_id)
    assert verdict is not None
    assert verdict.guard_reason == "dispatch-hold:x-owner"


def test_selection_guards_refuse_held_node_and_held_ancestry(tmp_path):
    from datetime import datetime, timezone

    from fno.backlog.advance import selection_guards

    owner = {
        "id": "x-owner",
        "status": "ready",
        "plan_path": str(_held_plan(tmp_path)),
    }
    child = {"id": "x-child", "status": "ready", "parent": "x-owner"}
    by_id = {row["id"]: row for row in (owner, child)}
    now = datetime.now(timezone.utc)
    assert selection_guards(owner, by_id, now) == "dispatch-hold:x-owner"
    assert selection_guards(child, by_id, now) == "dispatch-hold:x-owner"


# every readiness path re-probes the rung, not just `design` (sigma panel) -----


def test_maintain_never_quarantines_an_undesigned_node(tmp_path):
    """`maintain --apply` must not defer a scaffold off the board.

    `is_stale_ready` exempted DESIGN only, so an `idea`-rung row aged into
    stale-quarantine and got `deferred_at` stamped. Two of its three callers
    never pass through `selection_guards`, so the rung probe has to live here.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.maintain import is_stale_ready

    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=572)).isoformat()
    for status in ("idea", "stub", "design"):
        plan = tmp_path / f"{status}.md"
        plan.write_text(f"---\nstatus: {status}\n---\n")
        node = {
            "id": f"x-{status[:4]}",
            "status": "ready",
            "plan_path": str(plan),
            "created_at": old,
        }
        assert is_stale_ready(node, now, 21) is False, status


def test_entry_status_uses_the_same_rung_table_as_recompute(tmp_path):
    """`types._derive_status` was a second derivation and had gone divergent.

    It reported `ready` for a linked `idea` scaffold - the very bug this node
    removes, surviving on the pydantic read path - and made every such load emit
    a spurious `graph_status_drift` event.
    """
    from fno.graph.statuses import recompute_statuses
    from fno.graph.types import _derive_status

    for status, expected in (("idea", "idea"), ("stub", "idea"),
                             ("design", "design"), ("ready", "ready")):
        plan = tmp_path / f"{status}.md"
        plan.write_text(f"---\nstatus: {status}\n---\n")
        row = {
            "id": "x-derive1",
            "title": "t",
            "plan_path": str(plan),
            "blocked_by": [],
            "completed_at": None,
            "session_id": None,
            "claimed_at": None,
            "status": "ready",
        }
        assert _derive_status(row) == expected, status
        assert recompute_statuses([dict(row)])[0]["status"] == _derive_status(row)


def test_an_empty_plan_file_is_unreadable_not_ready(tmp_path):
    """A truncated write must not read as a plan that predates the vocabulary."""
    from fno.graph.ladder import Rung, is_dispatchable, plan_rung

    empty = tmp_path / "e.md"
    empty.write_text("")
    entry = {"id": "x-empty01", "plan_path": str(empty)}
    assert plan_rung(entry) is Rung.UNREADABLE
    assert is_dispatchable(entry) is False
