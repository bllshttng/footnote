"""The crown ladder is derived from the scope, and implementers get no crown.

    0   several projects   scope names 2+ config projects (a portfolio)
    1   one project        scope names one config project
    2   one epic           scope is a backlog node with type == "epic"

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
    """A graph with one epic + one ordinary node, and two configured projects.

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
                    {"id": "n-1", "type": "feature", "project": "alpha"},
                ]
            }
        ),
        encoding="utf-8",
    )

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\n'
        'projects = [{ name = "alpha" }, { name = "beta" }]\n',
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


# --- containment is now real, not an honor system ----------------------------


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
