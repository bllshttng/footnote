"""`fno backlog update --status/--set`: the patch door, end to end (x-665f).

Every test drives the real CLI, which forwards to the native backlog-update
action, which writes through the real store - never a stubbed receipt
asserted against its own renderer. The lifecycle verbs are transports over
the same door, so the undefer/unsupersede contracts live here too.

Filter: ``fno doctor test cli/tests/unit/test_backlog_update_status.py``
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.graph.cli import cli
from fno.graph.store import locked_mutate_graph, read_graph

pytestmark = pytest.mark.usefixtures("native_backlog_door")

runner = CliRunner()


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "slug": f"slug-{node_id}",
        "title": f"node {node_id}",
        "project": "fno",
        "type": "feature",
        "parent": None,
        "priority": "p2",
        "status": "idea",
        "blocked_by": [],
        "completed_at": None,
        "deferred_at": None,
        "pr_number": None,
        "pr_url": None,
        "children": [],
    }
    base.update(overrides)
    return base


@pytest.fixture()
def tmp_graph(tmp_path, monkeypatch):
    from fno.graph import cli as graph_cli

    g = tmp_path / "graph.json"
    locked_mutate_graph(g, lambda entries: entries)
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: g)
    return g


def _seed(g: Path, *nodes: dict) -> None:
    locked_mutate_graph(g, lambda entries: entries + list(nodes))


def _entry(g: Path, node_id: str) -> dict:
    return next(e for e in read_graph(g) if e["id"] == node_id)


def _invoke(*args):
    return runner.invoke(cli, list(args), catch_exceptions=True)


# ---------------------------------------------------------------------------
# --status: the readback contract
# ---------------------------------------------------------------------------


def test_status_idea_on_a_superseded_node_clears_the_chain_in_one_call(tmp_graph):
    """AC1-HP: one write clears the supersession facts, the replacer backref,
    and the park; the receipt names each field and the status move."""
    _seed(
        tmp_graph,
        _node(
            "x-3873",
            status="superseded",
            superseded_by="x-aaaa",
            supersession={"cause": "moved", "session_id": "s1"},
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="parked before supersede",
        ),
        _node("x-aaaa", supersedes=["x-3873"], status="ready"),
    )

    r = _invoke("update", "x-3873", "--status", "idea")

    assert r.exit_code == 0, r.output
    assert "superseded_by" in r.output
    assert "supersedes" in r.output
    assert "status: superseded -> idea" in r.output
    revived = _entry(tmp_graph, "x-3873")
    assert revived["superseded_by"] is None
    assert revived["supersession"] is None
    assert revived["deferred_at"] is None
    assert revived["status"] == "idea"
    assert _entry(tmp_graph, "x-aaaa")["supersedes"] == []


def test_status_idea_on_a_node_whose_plan_reads_ready_refuses(tmp_graph, tmp_path):
    """AC1-ERR: the door refuses to make a plan'd node an idea and names the
    flag that would actually do it. Nothing is written."""
    plan = tmp_path / "ready-plan.md"
    plan.write_text("---\ncreated: 2026-09-13\nstatus: ready\n---\n# plan\n")
    _seed(
        tmp_graph,
        _node("x-0001", status="superseded", superseded_by="x-aaaa", plan_path=str(plan)),
        _node("x-aaaa", supersedes=["x-0001"], status="ready"),
    )
    before = read_graph(tmp_graph)

    r = _invoke("update", "x-0001", "--status", "idea")

    assert r.exit_code == 2, r.output
    assert "plan_path" in r.output
    assert "--plan-path null" in r.output
    assert read_graph(tmp_graph) == before


def test_status_ready_without_a_plan_refuses(tmp_graph):
    """AC2-HP: a plan-less node cannot read ready; the refusal names plan_path."""
    _seed(tmp_graph, _node("x-0001"))

    r = _invoke("update", "x-0001", "--status", "ready")

    assert r.exit_code == 2, r.output
    assert "plan_path" in r.output
    assert _entry(tmp_graph, "x-0001")["status"] == "idea"


def test_status_ready_on_a_deferred_node_clears_the_park(tmp_graph, tmp_path):
    """AC2-EDGE: leaving a park for a plan'd rung clears the deferral facts."""
    plan = tmp_path / "p.md"
    plan.write_text("---\ncreated: 2026-09-13\nstatus: ready\n---\n# plan\n")
    _seed(
        tmp_graph,
        _node(
            "x-0002",
            status="deferred",
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="waiting",
            deferred_kind="later",
            plan_path=str(plan),
        ),
    )

    r = _invoke("update", "x-0002", "--status", "ready")

    assert r.exit_code == 0, r.output
    node = _entry(tmp_graph, "x-0002")
    assert node["deferred_at"] is None
    assert node["deferred_kind"] is None
    assert node["status"] == "ready"


def test_the_owned_transitions_refuse_naming_their_owner(tmp_graph):
    """AC8-HP: the moves another door owns refuse and name it; leaving done
    names reopen whatever the target."""
    _seed(
        tmp_graph,
        _node("x-0001"),
        _node("x-0009", status="done", completed_at="2026-01-01T00:00:00+00:00"),
    )

    for word, owner in [
        ("superseded", "fno backlog supersede"),
        ("done", "fno backlog done"),
        ("in_review", "--pr-number"),
        ("in_progress", "fno do target start"),
        ("blocked", "--add-blocker"),
    ]:
        r = _invoke("update", "x-0001", "--status", word)
        assert r.exit_code == 2, (word, r.output)
        assert owner in r.output, (word, r.output)

    r = _invoke("update", "x-0009", "--status", "idea")
    assert r.exit_code == 2, r.output
    assert "fno backlog reopen" in r.output


# ---------------------------------------------------------------------------
# --set: the field policy
# ---------------------------------------------------------------------------


def test_an_unknown_field_refuses_listing_the_settable_set(tmp_graph):
    """AC3-HP: an unknown name refuses by name and teaches the settable set."""
    _seed(tmp_graph, _node("x-0001"))

    r = _invoke("update", "x-0001", "--set", "nonsense=1")

    assert r.exit_code == 2, r.output
    assert "nonsense" in r.output
    assert "title" in r.output


def test_derived_and_owned_fields_refuse_naming_their_owner(tmp_graph):
    """AC3-ERR: status names --status, children names --parent, completed_at
    names the done verb."""
    _seed(tmp_graph, _node("x-0001"))

    for field, value, owner in [
        ("status", "done", "--status"),
        ("children", "[]", "--parent"),
        ("completed_at", "2026-09-13", "fno backlog done"),
    ]:
        r = _invoke("update", "x-0001", "--set", f"{field}={value}")
        assert r.exit_code == 2, (field, r.output)
        assert owner in r.output, (field, r.output)


def test_two_sets_land_in_one_write_with_two_receipt_lines(tmp_graph):
    """AC3-EDGE: ordered pairs commit in one write; an out-of-enum priority
    refuses listing the vocabulary."""
    _seed(tmp_graph, _node("x-0001"))

    r = _invoke("update", "x-0001", "--set", "title=New", "--set", "priority=p1")

    assert r.exit_code == 0, r.output
    assert "title:" in r.output
    assert "priority:" in r.output
    node = _entry(tmp_graph, "x-0001")
    assert node["title"] == "New"
    assert node["priority"] == "p1"

    r = _invoke("update", "x-0001", "--set", "priority=p9")
    assert r.exit_code == 2, r.output
    assert "p0" in r.output and "p3" in r.output


# ---------------------------------------------------------------------------
# addressing: id, slug, ambiguity
# ---------------------------------------------------------------------------


def test_a_slug_resolves_and_an_ambiguous_token_refuses_naming_candidates(tmp_graph):
    """AC11-HP: the door resolves exact slugs; two nodes sharing one slug
    refuse with the candidate ids."""
    _seed(
        tmp_graph,
        _node("x-0001", slug="same-slug"),
        _node("x-0002", slug="same-slug"),
        _node("x-0003", slug="only-slug", priority="p1"),
    )

    # A unique slug resolves: the priority write lands on x-0003.
    r = _invoke("update", "only-slug", "--set", "priority=p0")
    assert r.exit_code == 0, r.output
    assert _entry(tmp_graph, "x-0003")["priority"] == "p0"

    # An ambiguous slug refuses with the candidates, writing nothing.
    r = _invoke("update", "same-slug", "--set", "priority=p1")
    assert r.exit_code == 2, r.output
    assert "x-0001" in r.output and "x-0002" in r.output
    for nid in ("x-0001", "x-0002"):
        assert _entry(tmp_graph, nid)["priority"] == "p2"


# ---------------------------------------------------------------------------
# the lifecycle transports
# ---------------------------------------------------------------------------


def test_undefer_a_still_superseded_node_refuses_naming_the_door(tmp_graph):
    """AC5-HP: the false-receipt shape. A node carrying both facts cannot be
    undeferred into a lie; the refusal names the route that revives it."""
    _seed(
        tmp_graph,
        _node(
            "x-3873",
            status="superseded",
            superseded_by="x-aaaa",
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="stale",
        ),
        _node("x-aaaa", supersedes=["x-3873"], status="ready"),
    )

    r = _invoke("undefer", "x-3873")

    assert r.exit_code == 2, r.output
    assert "fno backlog update x-3873 --status idea" in r.output
    assert "Undeferred" not in r.output
    node = _entry(tmp_graph, "x-3873")
    assert node["deferred_at"] == "2026-01-01T00:00:00+00:00"
    assert node["status"] == "superseded"


def test_undefer_a_plain_deferred_node_clears_and_prints_the_ack(tmp_graph):
    """AC5-ERR: a real undefer clears the facts, prints Undeferred."""
    _seed(
        tmp_graph,
        _node(
            "x-0001",
            status="deferred",
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="stale",
        ),
    )

    r = _invoke("undefer", "x-0001")

    assert r.exit_code == 0, r.output
    assert "Undeferred x-0001" in r.output
    node = _entry(tmp_graph, "x-0001")
    assert not node.get("deferred_at")
    assert node["status"] == "idea"


def test_undefer_a_node_that_was_not_deferred_is_an_unchanged_no_op(tmp_graph):
    """AC5-EDGE: exit 0, the unchanged receipt, no Undeferred line."""
    _seed(tmp_graph, _node("x-0001"))

    r = _invoke("undefer", "x-0001")

    assert r.exit_code == 0, r.output
    assert "unchanged" in r.output
    assert "Undeferred" not in r.output


def test_unsupersede_lands_on_the_surviving_park(tmp_graph):
    """AC6-HP: leaving superseded keeps the deferral; the node reads deferred."""
    _seed(
        tmp_graph,
        _node(
            "x-0005",
            status="superseded",
            superseded_by="x-0004",
            supersession={"cause": "moved on"},
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="waiting",
        ),
        _node("x-0004", supersedes=["x-0005"], status="ready"),
    )

    r = _invoke("unsupersede", "x-0005")

    assert r.exit_code == 0, r.output
    assert "Unsuperseded x-0005" in r.output
    assert "x-0004" in r.output
    node = _entry(tmp_graph, "x-0005")
    assert node["superseded_by"] is None
    assert node["deferred_at"] == "2026-01-01T00:00:00+00:00"
    assert node["status"] == "deferred"
    assert _entry(tmp_graph, "x-0004")["supersedes"] == []


def test_defer_and_retract_land_the_same_rows_through_the_door(tmp_graph):
    """AC7-HP: a locked node defers cleanly (lock facts clear); retract is
    defer with the kind forced; both routes share the door's validator set."""
    _seed(tmp_graph, _node("x-0003", status="ready", locked_by="sess-1", locked_at="2026-01-01T00:00:00+00:00"))

    r = _invoke("defer", "x-0003", "--reason", "waiting on x")
    assert r.exit_code == 0, r.output
    node = _entry(tmp_graph, "x-0003")
    assert node["locked_by"] is None
    assert node["locked_at"] is None
    assert node["deferred_reason"] == "waiting on x"
    assert node["status"] == "deferred"

    r = _invoke("defer", "x-0003", "--reason", "again")
    assert r.exit_code == 0, r.output
    assert _entry(tmp_graph, "x-0003")["deferred_reason"] == "again"

    r = _invoke("retract", "x-0003", "filed on a false premise")
    assert r.exit_code == 0, r.output
    node = _entry(tmp_graph, "x-0003")
    assert node["deferred_kind"] == "retracted"
    assert node["status"] == "deferred"


def test_defer_stamps_the_exact_match_kind_without_a_flag(tmp_graph):
    """The machine-stamp classifier rides the transport: the drain reason
    self-classifies as expired; prose stays unknown and clears a stale kind."""
    _seed(tmp_graph, _node("x-0001", status="ready"))

    r = _invoke("defer", "x-0001", "--reason", "stale >30d, drained by maintain")
    assert r.exit_code == 0, r.output
    assert _entry(tmp_graph, "x-0001")["deferred_kind"] == "expired"

    r = _invoke("defer", "x-0001", "--reason", "just prose")
    assert r.exit_code == 0, r.output
    node = _entry(tmp_graph, "x-0001")
    assert "deferred_kind" not in node or node["deferred_kind"] is None


def test_the_door_flags_cannot_mix_with_a_legacy_flag(tmp_graph):
    """One call, one door: a mixed call refuses naming both calls to run."""
    _seed(tmp_graph, _node("x-0001"))

    r = _invoke("update", "x-0001", "--status", "idea", "--priority", "p1")

    assert r.exit_code == 2, r.output
    assert "--priority" in r.output
    assert _entry(tmp_graph, "x-0001")["priority"] == "p2"

def test_the_mix_refusal_catches_a_zero_valued_legacy_flag(tmp_graph):
    """`--fixes-pr 0` means clear, and `0 == False`: the mix check must use
    identity, or the door silently eats the legacy flag."""
    _seed(tmp_graph, _node("x-0001"))

    r = _invoke("update", "x-0001", "--status", "idea", "--fixes-pr", "0")

    assert r.exit_code == 2, r.output
    assert "--fixes-pr" in r.output
