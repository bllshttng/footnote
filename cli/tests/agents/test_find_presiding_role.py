"""``find_presiding_role`` (x-3ecf): the role one rung above a scope, so
``lead escalate`` can climb to it instead of the operator (directive points
3/4 - a disagreement between two L2 leads reaches the L1 lead that presides
over both, without either deciding to involve the operator).
"""
from __future__ import annotations

from fno.agents.team import find_presiding_role


def test_an_epic_set_role_finds_its_project_lead():
    by_id = {"x-epic": {"project": "fno"}}
    roles = [
        {"holder": "l1-lead", "level": 1, "scope": "fno", "status": "working"},
        {"holder": "other", "level": 1, "scope": "other-project", "status": "working"},
    ]
    presiding = find_presiding_role("x-epic", 2, roles, by_id)
    assert presiding is not None
    assert presiding["holder"] == "l1-lead"


def test_a_project_role_finds_its_portfolio_lead():
    roles = [
        {"holder": "l0-lead", "level": 0, "scope": "fno,other-project", "status": "working"},
    ]
    presiding = find_presiding_role("fno", 1, roles, by_id=None)
    assert presiding is not None
    assert presiding["holder"] == "l0-lead"


def test_level_zero_has_nothing_above_it():
    roles = [{"holder": "l0-lead", "level": 0, "scope": "a,b", "status": "working"}]
    assert find_presiding_role("a,b", 0, roles, by_id=None) is None


def test_no_containing_role_reads_as_none():
    roles = [{"holder": "l1-lead", "level": 1, "scope": "unrelated", "status": "working"}]
    by_id = {"x-epic": {"project": "fno"}}
    assert find_presiding_role("x-epic", 2, roles, by_id) is None


def test_epics_spanning_two_projects_have_no_single_presiding_l1():
    by_id = {"x-a": {"project": "fno"}, "x-b": {"project": "other"}}
    roles = [
        {"holder": "l1-fno", "level": 1, "scope": "fno", "status": "working"},
        {"holder": "l1-other", "level": 1, "scope": "other", "status": "working"},
    ]
    assert find_presiding_role("x-a,x-b", 2, roles, by_id) is None


def test_a_manifest_only_role_is_never_presiding():
    # A role whose row is gone can never receive a message.
    by_id = {"x-epic": {"project": "fno"}}
    roles = [
        {"holder": "ghost", "level": 1, "scope": "fno", "status": "manifest-only"},
    ]
    assert find_presiding_role("x-epic", 2, roles, by_id) is None


def test_an_unreadable_graph_refuses_rather_than_guessing():
    roles = [{"holder": "l1-lead", "level": 1, "scope": "fno", "status": "working"}]
    assert find_presiding_role("x-epic", 2, roles, by_id=None) is None
