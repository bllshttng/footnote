"""``find_presiding_crown`` (x-3ecf): the crown one rung above a scope, so
``king escalate`` can climb to it instead of the operator (directive points
3/4 - a disagreement between two L2 kings reaches the L1 king that presides
over both, without either deciding to involve the operator).
"""
from __future__ import annotations

from fno.agents.court import find_presiding_crown


def test_an_epic_set_crown_finds_its_project_king():
    by_id = {"x-epic": {"project": "fno"}}
    crowns = [
        {"holder": "l1-king", "level": 1, "scope": "fno", "status": "working"},
        {"holder": "other", "level": 1, "scope": "other-project", "status": "working"},
    ]
    presiding = find_presiding_crown("x-epic", 2, crowns, by_id)
    assert presiding is not None
    assert presiding["holder"] == "l1-king"


def test_a_project_crown_finds_its_portfolio_king():
    crowns = [
        {"holder": "l0-king", "level": 0, "scope": "fno,other-project", "status": "working"},
    ]
    presiding = find_presiding_crown("fno", 1, crowns, by_id=None)
    assert presiding is not None
    assert presiding["holder"] == "l0-king"


def test_level_zero_has_nothing_above_it():
    crowns = [{"holder": "l0-king", "level": 0, "scope": "a,b", "status": "working"}]
    assert find_presiding_crown("a,b", 0, crowns, by_id=None) is None


def test_no_containing_crown_reads_as_none():
    crowns = [{"holder": "l1-king", "level": 1, "scope": "unrelated", "status": "working"}]
    by_id = {"x-epic": {"project": "fno"}}
    assert find_presiding_crown("x-epic", 2, crowns, by_id) is None


def test_epics_spanning_two_projects_have_no_single_presiding_l1():
    by_id = {"x-a": {"project": "fno"}, "x-b": {"project": "other"}}
    crowns = [
        {"holder": "l1-fno", "level": 1, "scope": "fno", "status": "working"},
        {"holder": "l1-other", "level": 1, "scope": "other", "status": "working"},
    ]
    assert find_presiding_crown("x-a,x-b", 2, crowns, by_id) is None


def test_a_manifest_only_crown_is_never_presiding():
    # A crown whose row is gone can never receive a message.
    by_id = {"x-epic": {"project": "fno"}}
    crowns = [
        {"holder": "ghost", "level": 1, "scope": "fno", "status": "manifest-only"},
    ]
    assert find_presiding_crown("x-epic", 2, crowns, by_id) is None


def test_an_unreadable_graph_refuses_rather_than_guessing():
    crowns = [{"holder": "l1-king", "level": 1, "scope": "fno", "status": "working"}]
    assert find_presiding_crown("x-epic", 2, crowns, by_id=None) is None
