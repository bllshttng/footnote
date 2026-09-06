"""The crown ladder is derived from the scope, and implementers get no crown.

    0   several projects   scope names 2+ config projects (a portfolio)
    1   one project        scope names one config project
    2   a set of epics     every scope names a backlog node with type == "epic"

There is no operator-supplied level anywhere in the surface. That is the point of
these tests: the old grammar made callers hand-type an altitude on a ladder whose
direction reads backwards (0 is the TOP), and a wrong guess minted authority at
the wrong rung with no error. The rung is now a fact about the territory, so the
only way to get it wrong is to name the wrong territory - which is checkable, and
checked here.
"""
from __future__ import annotations

import json

import pytest

from fno.paths_testing import use_tmpdir
from fno.projects import resolve as proj_resolve


@pytest.fixture
def territory(tmp_path, monkeypatch):
    """A graph with two epics + one ordinary node, and two configured projects.

    The projects go through the REAL resolver against a temp config rather than a
    monkeypatched stub: "is this a project?" is half the ladder, so a stub would
    test the test.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths

    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "e-1", "type": "epic", "project": "alpha"},
                    {"id": "e-2", "type": "epic", "project": "beta"},
                    {"id": "n-1", "type": "feature", "project": "alpha"},
                ]
            }
        ),
        encoding="utf-8",
    )

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\n'
        'projects = [{ name = "alpha", short_name = "a" }, { name = "beta" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()
    yield tmp_path
    proj_resolve._clear_cache()


def _derive(scopes):
    from fno.agents.crown import derive_crown_level

    return derive_crown_level(scopes)


def _refusal(scopes) -> str:
    from fno.agents.crown import CrownScopeError, derive_crown_level

    with pytest.raises(CrownScopeError) as exc:
        derive_crown_level(scopes)
    return str(exc.value)


# --- the three rungs ---------------------------------------------------------


def test_two_projects_is_a_portfolio(territory) -> None:
    assert _derive(["alpha", "beta"]) == 0


def test_one_project_is_a_project_king(territory) -> None:
    assert _derive(["alpha"]) == 1


def test_one_epic_is_a_director(territory) -> None:
    assert _derive(["e-1"]) == 2


def test_order_and_duplicates_do_not_change_the_rung(territory) -> None:
    """The scope is a set, so a portfolio spelled two ways is one portfolio."""
    assert _derive(["beta", "alpha"]) == _derive(["alpha", "beta", "alpha"]) == 0


# --- implementers get no crown ----------------------------------------------


def test_a_non_epic_node_is_refused_not_stamped_at_a_bottom_rung(territory) -> None:
    """The old ladder had an IC rung, so any node minted a crown. A single task is
    work, not a territory: nobody reigns for a day over it."""
    msg = _refusal(["n-1"])
    assert "not an epic" in msg
    assert "Implementers get no crowns" in msg
    # AC2-HP: the refusal names the exact fix when the node IS meant to be an
    # epic - one command away, not a dead end.
    assert "fno backlog update n-1 --type epic" in msg


def test_an_unresolvable_scope_is_refused(territory) -> None:
    """A typo used to fall through to level 0, minting a VP over a scope nobody
    could find. Refusing is the whole reason derivation returns no default."""
    assert "neither a configured project nor a backlog node" in _refusal(["banana"])


def test_an_empty_scope_is_refused(territory) -> None:
    assert "needs a scope" in _refusal([])


# --- mixed scopes are a mistake about what is being ruled --------------------


@pytest.mark.parametrize("scopes", [["alpha", "e-1"], ["alpha", "banana"]])
def test_a_mixed_scope_is_refused_rather_than_coerced(territory, scopes) -> None:
    """A portfolio rules PROJECTS. Naming a project alongside an epic is not a
    level-0 crown over both, it is a mistake about the territory - so it is
    refused rather than silently reduced to whichever half parses."""
    assert "rules PROJECTS" in _refusal(scopes)


def test_a_mixed_scope_names_each_scopes_rung(territory) -> None:
    """The refusal names what each scope IS, so the operator can see the
    mistake (a project beside an epic) rather than just that one exists."""
    msg = _refusal(["alpha", "e-1"])
    assert "alpha (a project)" in msg
    assert "e-1 (an epic)" in msg


# --- rung 2 rules a SET of epics ----------------------------------------------
#
# The operator's mux king oversees two epics at once; before the set grammar
# that king had no legal crown: two epics was read as a failed portfolio.


def test_two_epics_crown_as_one_set_at_rung_2(territory) -> None:
    from fno.agents.crown import resolve_crown

    assert resolve_crown(["e-1", "e-2"]) == (2, "e-1,e-2")


def test_the_epic_set_is_canonical_order_and_dedup(territory) -> None:
    """The stored form is the canonical join, so two spellings of one set are
    one crown - the one-live-crown guard compares stored scopes."""
    from fno.agents.crown import resolve_crown

    assert resolve_crown(["e-2", "e-1", "e-2"]) == (2, "e-1,e-2")


def test_a_set_of_epics_and_projects_refuses_naming_the_mixed_rungs(
    territory,
) -> None:
    msg = _refusal(["e-1", "alpha", "e-2"])
    assert "alpha (a project)" in msg
    assert "e-1 (an epic)" in msg


def test_a_non_epic_node_in_a_multi_scope_crown_is_refused(territory) -> None:
    """Implementers get no crowns, alone or inside a set."""
    msg = _refusal(["e-1", "n-1"])
    assert "n-1" in msg
    assert "not an epic" in msg


def test_an_unknown_member_of_a_set_is_refused_as_a_typo(territory) -> None:
    assert "neither a configured project nor a backlog node" in _refusal(
        ["e-1", "banana"]
    )


# --- containment is now real, not an honor system ----------------------------


# --- an alias is the same territory, not a second one ------------------------
#
# `resolve_project_name` accepts a project's `short_name`, so one project can be
# spelled two ways. The stored scope must be the CANONICAL name, or the
# equality-based one-live-crown guard sees two territories where there is one.


def test_an_alias_stores_as_the_canonical_project(territory) -> None:
    from fno.agents.crown import resolve_crown

    assert resolve_crown(["a"]) == resolve_crown(["alpha"]) == (1, "alpha")


def test_one_project_named_twice_is_not_a_portfolio(territory) -> None:
    """`-k alpha -k a` is one project spelled two ways. Deduping the raw strings
    would count two members and mint portfolio authority over a single project."""
    from fno.agents.crown import resolve_crown

    assert resolve_crown(["alpha", "a"]) == (1, "alpha")


def test_a_real_portfolio_still_derives_level_0_through_aliases(territory) -> None:
    from fno.agents.crown import resolve_crown

    assert resolve_crown(["a", "beta"]) == (0, "alpha,beta")


def test_containment_normalizes_aliases_on_both_sides(territory) -> None:
    from fno.agents.crown import scope_contains

    assert scope_contains("alpha,beta", "a") is True
    assert scope_contains("a,beta", "alpha") is True


def test_containment_is_checkable_across_the_ladder(territory) -> None:
    """The rule this replaces could only check that two scopes DIFFERED, because
    scopes were opaque and project>epic containment was not derivable. It is now:
    a project sits in a portfolio by name, and an epic carries its project."""
    from fno.agents.crown import scope_contains

    assert scope_contains("alpha,beta", "alpha") is True     # project in portfolio
    assert scope_contains("alpha", "e-1") is True            # epic in its project
    assert scope_contains("alpha,beta", "e-1") is True       # epic in the portfolio
    assert scope_contains("beta", "e-1") is False            # epic of another project
    assert scope_contains("alpha", "alpha") is False         # a peer, not below


def test_a_rung_2_set_is_a_union_for_containment(territory) -> None:
    """A crown over two epics contains a crown over either member: the king
    over both epics can grant inside each. The set itself is a peer of its own
    spelling, and an epic outside the set is not contained."""
    from fno.agents.crown import scope_contains

    assert scope_contains("e-1,e-2", "e-1") is True
    assert scope_contains("e-1,e-2", "e-2") is True
    assert scope_contains("e-2,e-1", "e-1") is True      # stored order-free
    assert scope_contains("e-1,e-2", "e-1,e-2") is False  # a peer, not below
    assert scope_contains("e-1", "e-1,e-2") is False       # narrower holds less


def test_an_epic_set_falls_under_the_crown_holding_every_member(territory) -> None:
    """A set of epics is below whatever crown holds EVERY member, not below a
    set of project names. The subset shortcut read epic ids against project
    names and always fell through, so no king could grant two epics at once."""
    from fno import paths

    paths.graph_json().write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "e-1", "type": "epic", "project": "alpha"},
                    {"id": "e-2", "type": "epic", "project": "alpha"},
                    {"id": "e-3", "type": "epic", "project": "beta"},
                ]
            }
        ),
        encoding="utf-8",
    )
    from fno.agents.crown import scope_contains

    assert scope_contains("alpha", "e-1,e-2") is True     # both members of alpha
    assert scope_contains("alpha", "e-1,e-3") is False    # spans two projects
    assert scope_contains("alpha,beta", "e-1,e-3") is True
    assert scope_contains("e-1,e-2", "e-1,e-2,e-3") is False  # peer sets


def test_one_graph_parse_serves_every_member_of_a_set(territory, monkeypatch) -> None:
    """A mixed or invalid set resolves through ONE graph read, not one full
    parse per member: _graph_index exists so refusal messages do not pay a
    graph parse apiece."""
    import fno.agents.crown as crown_mod
    from fno.agents.crown import CrownScopeError, resolve_crown

    calls = {"n": 0}
    real = crown_mod._graph_index

    def counting() -> object:
        calls["n"] += 1
        return real()

    monkeypatch.setattr(crown_mod, "_graph_index", counting)

    with pytest.raises(CrownScopeError):
        resolve_crown(["e-1", "n-1", "e-2"])

    assert calls["n"] == 1
