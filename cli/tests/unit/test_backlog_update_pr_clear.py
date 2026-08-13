"""`backlog update --pr-number/--pr-url null` clears the link.

Every other nullable scalar on this verb documents `'null' clears`; these two
did not honor it, and the graph is hand-edit-forbidden - so a node linked to the
wrong PR could not be unlinked at all, and the mislink would ride to a merge and
close a node that shipped nothing.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.harness_identity import AMBIENT_IDENTITY_ENV as _MARKERS

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _first(g: Path) -> dict:
    return json.loads(g.read_text())["entries"][0]


def _node(g: Path, nid: str) -> dict:
    for e in json.loads(g.read_text())["entries"]:
        if e.get("id") == nid:
            return e
    raise AssertionError(f"node {nid} missing from graph")


def test_pr_link_can_be_cleared(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_number": 504, "pr_url": "https://example.com/pull/504",
    }])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-number", "null", "--pr-url", "null",
    ])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] is None
    assert node["pr_url"] is None


def test_pr_number_still_sets_an_int(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "77", "--pr-url", "https://github.com/o/r/pull/77",
    ])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] == 77
    assert node["pr_url"] == "https://github.com/o/r/pull/77"


def test_pr_number_alone_derives_the_url(tmp_graph, monkeypatch):
    """A url-less pr_number names no repo, and PR numbers collide across repos."""
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: f"https://github.com/o/r/pull/{pr}")
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-number", "77"])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["pr_url"] == "https://github.com/o/r/pull/77"


def test_pr_url_alone_derives_the_number(tmp_graph):
    """A url-only update derives pr_number from the url. node_pr_refs gates on
    isinstance(pr_number, int), so without this a node linked by --pr-url alone
    is invisible to merge detection - the x-9ab2 shape."""
    from fno.graph._reconcile import node_pr_refs

    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-url", "https://github.com/o/r/pull/77",
    ])

    assert result.exit_code == 0, result.output
    node = _first(tmp_graph)
    assert node["pr_number"] == 77
    assert node["pr_url"] == "https://github.com/o/r/pull/77"
    assert node_pr_refs(node), "reconcile must see the PR"


def test_pr_number_refused_when_repo_unresolvable(tmp_graph, monkeypatch):
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: None)
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-number", "77"])

    assert result.exit_code != 0
    # Both remedies must be named: naming only one leaves the caller stuck.
    assert "gh auth login" in result.output and "--pr-url" in result.output
    assert _first(tmp_graph).get("pr_number") is None


def test_unparseable_pr_url_is_rejected(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-number", "77", "--pr-url", "not-a-url",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_url") is None


def test_clearing_the_url_while_setting_a_number_is_refused(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-number", "77", "--pr-url", "null",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_number") is None


def test_plan_path_can_be_cleared(tmp_graph):
    """A literal 'null' would bind the node to a plan file named "null", which
    reads as bound to every gate that only checks presence."""
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "plan_path": "/plans/old.md",
    }])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", "null",
    ])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["plan_path"] is None


def test_plan_path_still_binds_a_real_path(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", "/plans/new.md",
    ])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["plan_path"] == "/plans/new.md"


def test_second_plan_path_binding_is_refused(tmp_graph):
    """AC1: a plan is one PR is one node. A second node cannot bind an owned plan;
    it exits non-zero naming the owner, the adopt alternative, and --force, and
    leaves the graph unmodified."""
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "owner", "domain": "code", "project": "p",
         "plan_path": "/plans/shared.md"},
        {"id": "ab-00000002", "title": "contender", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000002", "--plan-path", "/plans/shared.md",
    ])

    assert result.exit_code != 0
    assert "ab-00000001" in result.output  # names the owner
    assert "adopt" in result.output         # names the legal alternative
    assert "--force" in result.output       # names the escape
    # Graph unmodified: contender stays unbound, owner untouched.
    assert _node(tmp_graph, "ab-00000002").get("plan_path") is None
    assert _node(tmp_graph, "ab-00000001")["plan_path"] == "/plans/shared.md"


def test_abs_rel_mismatch_cannot_smuggle_a_second_binding(tmp_graph):
    """The same plan reached two ways is still one plan: normalization closes the
    abs/rel hole a naive string compare would leave open."""
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "owner", "domain": "code", "project": "p",
         "plan_path": "/plans/shared.md"},
        {"id": "ab-00000002", "title": "contender", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000002", "--plan-path", "/plans/./shared.md",
    ])

    assert result.exit_code != 0
    assert "ab-00000001" in result.output
    assert _node(tmp_graph, "ab-00000002").get("plan_path") is None


def test_force_binds_a_second_holder_and_names_the_owner(tmp_graph):
    """AC2: --force binds the contender AND tells the operator the owner still
    holds the plan, so a deliberate repoint is never silent."""
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "owner", "domain": "code", "project": "p",
         "plan_path": "/plans/shared.md"},
        {"id": "ab-00000002", "title": "contender", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000002", "--plan-path", "/plans/shared.md", "--force",
    ])

    assert result.exit_code == 0, result.output
    assert _node(tmp_graph, "ab-00000002")["plan_path"] == "/plans/shared.md"
    assert "ab-00000001" in result.output  # told the owner still holds it


def test_node_can_rebind_the_plan_it_already_owns(tmp_graph):
    """Re-binding the same plan to the same node is a no-op repoint, not a
    refusal: the node is its own owner, not a 'second' holder."""
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "owner", "domain": "code", "project": "p",
         "plan_path": "/plans/shared.md"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", "/plans/shared.md",
    ])

    assert result.exit_code == 0, result.output
    assert _node(tmp_graph, "ab-00000001")["plan_path"] == "/plans/shared.md"


def test_clearing_the_url_alone_is_refused_when_a_number_remains(tmp_graph):
    """Clearing only the url strands the pr_number the node already carries."""
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_number": 77, "pr_url": "https://github.com/o/r/pull/77",
    }])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-url", "null"])

    assert result.exit_code != 0
    node = _first(tmp_graph)
    assert node["pr_number"] == 77
    assert node["pr_url"] == "https://github.com/o/r/pull/77"


def test_clearing_the_url_alone_is_fine_when_no_number_remains(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_url": "https://github.com/o/r/pull/77",
    }])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-url", "null"])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["pr_url"] is None


def test_unparseable_pr_url_rejected_without_a_pr_number(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--pr-url", "not-a-url"])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_url") is None


def test_add_pr_derives_its_url(tmp_graph, monkeypatch):
    """additional_prs entries are read by the same repo-scoped matcher, so a
    bare --add-pr is unattributable for the same reason a bare --pr-number is."""
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: f"https://github.com/o/r/pull/{pr}")
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--add-pr", "88"])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["additional_prs"] == [
        {"number": 88, "url": "https://github.com/o/r/pull/88"}
    ]


def test_add_pr_refused_when_repo_unresolvable(tmp_graph, monkeypatch):
    import fno.graph._reconcile as rec
    monkeypatch.setattr(rec, "pr_url_for_repo", lambda pr, cwd=None: None)
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, ["backlog", "update", "ab-00000001", "--add-pr", "88"])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("additional_prs") in (None, [])


def test_pr_url_naming_a_different_pr_is_refused(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "123", "--pr-url", "https://github.com/o/r/pull/999",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("pr_number") is None


def test_add_pr_url_naming_a_different_pr_is_refused(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"},
    ])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--add-pr", "88", "--add-pr-url", "https://github.com/o/r/pull/99",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph).get("additional_prs") in (None, [])


def test_url_only_update_must_name_the_number_the_node_already_carries(tmp_graph):
    """Without --pr-number the node keeps its number, so a url naming a
    different PR re-creates the two-different-PRs row the paired path refuses."""
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_number": 123, "pr_url": "https://github.com/o/r/pull/123",
    }])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-url", "https://github.com/o/r/pull/999",
    ])

    assert result.exit_code != 0
    assert _first(tmp_graph)["pr_url"] == "https://github.com/o/r/pull/123"


def test_url_only_update_is_allowed_when_it_names_the_same_pr(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "pr_number": 123, "pr_url": "https://github.com/old/repo/pull/123",
    }])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--pr-url", "https://github.com/o/r/pull/123",
    ])

    assert result.exit_code == 0, result.output
    assert _first(tmp_graph)["pr_url"] == "https://github.com/o/r/pull/123"


# ---- ship provenance: the consolidated ship writer lives in `update` ----

# _MARKERS is imported at the top of the file (AMBIENT_IDENTITY_ENV). Several
# modules read a session marker directly rather than through the resolver
# (carveout/core.py, done/cli.py, adapters/hermes.py), so a
# hand-maintained copy stops covering a marker the moment one is added - the
# canonical tuple stays in sync with the resolver's scrub set.


def _set_ambient_claude(monkeypatch, sid="SESSION-A"):
    for m in _MARKERS:
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sid)


def _clear_ambient(monkeypatch):
    for m in _MARKERS:
        monkeypatch.delenv(m, raising=False)


def test_ship_stamp_fires_on_first_pr_link(tmp_graph, monkeypatch):
    """The ship row lands when pr_number transitions unset->set, recording the
    implementer's ambient identity. This is the one site every shipped node
    passes through, so the row names the implementer, not the merger."""
    _set_ambient_claude(monkeypatch)
    _seed(tmp_graph, [{"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"}])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "77", "--pr-url", "https://github.com/o/r/pull/77",
    ])

    assert result.exit_code == 0, result.output
    ship = [r for r in _node(tmp_graph, "ab-00000001").get("sessions", [])
            if r.get("phase") == "ship"]
    assert len(ship) == 1
    assert ship[0]["harness"] == "claude"
    assert ship[0]["session_id"] == "SESSION-A"


def test_ship_stamp_skips_with_no_identity_and_exit_zero(tmp_graph, monkeypatch):
    """No ambient identity -> the stamp skips with a named stderr reason, exit 0
    (the link itself still lands), and no ship row is invented. Silent skips are
    the defect this stamp exists to remove, so the reason names what refused."""
    _clear_ambient(monkeypatch)
    _seed(tmp_graph, [{"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"}])

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "77", "--pr-url", "https://github.com/o/r/pull/77",
    ])

    assert result.exit_code == 0, result.output
    assert _node(tmp_graph, "ab-00000001")["pr_number"] == 77  # link still lands
    assert "ship provenance" in result.output                   # skip names itself
    assert _node(tmp_graph, "ab-00000001").get("sessions", []) == []


def test_ship_stamp_does_not_refire_on_an_already_linked_node(tmp_graph, monkeypatch):
    """A second update on an already-linked node is not an unset->set transition,
    so it adds no second ship row. The pr-number link is the choke point; the
    stamp fires once, on first link."""
    _set_ambient_claude(monkeypatch)
    _seed(tmp_graph, [{"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"}])

    first = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "77", "--pr-url", "https://github.com/o/r/pull/77",
    ])
    assert first.exit_code == 0, first.output
    # Already linked -> not a transition -> no second ship row.
    second = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--pr-number", "77", "--pr-url", "https://github.com/o/r/pull/77",
    ])
    assert second.exit_code == 0, second.output
    ship = [r for r in _node(tmp_graph, "ab-00000001").get("sessions", [])
            if r.get("phase") == "ship"]
    assert len(ship) == 1


def test_blueprint_stamp_fires_on_first_plan_bind(tmp_graph, monkeypatch, tmp_path):
    """The blueprint row lands when plan_path transitions unset->set. plan-bind
    is blueprint's end, so the row carries ended_at and no started_at (the
    roster renders it 'end only'). This is the code choke point that replaces
    the skill-prose stamp."""
    _set_ambient_claude(monkeypatch)
    _seed(tmp_graph, [{"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"}])
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\n---\n# plan\n")

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", str(plan),
    ])

    assert result.exit_code == 0, result.output
    bp = [r for r in _node(tmp_graph, "ab-00000001").get("sessions", [])
          if r.get("phase") == "blueprint"]
    assert len(bp) == 1
    assert bp[0]["harness"] == "claude"
    assert bp[0]["session_id"] == "SESSION-A"
    assert bp[0]["ended_at"] and "started_at" not in bp[0]


def test_blueprint_stamp_skips_with_no_identity(tmp_graph, monkeypatch, tmp_path):
    _clear_ambient(monkeypatch)
    _seed(tmp_graph, [{"id": "ab-00000001", "title": "t", "domain": "code", "project": "p"}])
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\n---\n# plan\n")

    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", str(plan),
    ])

    assert result.exit_code == 0, result.output
    assert "blueprint provenance" in result.output  # skip names what refused
    assert _node(tmp_graph, "ab-00000001").get("sessions", []) == []


def test_blueprint_stamp_does_not_refire_on_a_rebind(tmp_graph, monkeypatch, tmp_path):
    _set_ambient_claude(monkeypatch)
    _seed(tmp_graph, [{
        "id": "ab-00000001", "title": "t", "domain": "code", "project": "p",
        "plan_path": str(tmp_path / "old.md"),
    }])
    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\n---\n# plan\n")

    # plan_path is already set -> not an unset->set transition -> no blueprint row.
    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--plan-path", str(plan),
    ])

    assert result.exit_code == 0, result.output
    assert _node(tmp_graph, "ab-00000001").get("sessions", []) == []
