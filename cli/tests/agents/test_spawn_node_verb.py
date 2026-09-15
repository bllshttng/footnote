"""A ``--node`` spawn runs the node's derived lifecycle verb or refuses (x-2c0d).

The authority is the same derivation ``backlog advance`` runs
(``resolve_effective_verb``); ``node_seed`` is the seam-side helper and
``rust_runtime._node_seed_at_seam`` its argv adapter. ``None`` from
``node_seed`` means pass the seed through untouched.
"""
from __future__ import annotations

import pytest

from fno.agents import harness_map
from fno.agents.harness_map import DispatchResolveError, node_seed, resolve_effective_verb


def _planless(difficulty="medium", **kw):
    rec = {"id": "x-1", "dispatch_verb": None, "difficulty": difficulty}
    rec.update(kw)
    return rec


# --- AC1-HP: derive or refuse -------------------------------------------------


def test_a_planless_medium_node_composes_blueprint():
    seed = node_seed(None, "x-1", _planless())
    assert seed == "/blueprint x-1"


def test_prose_gets_the_command_in_front_of_it():
    seed = node_seed("port the parser tests", "x-1", _planless())
    assert seed == "/blueprint x-1\n\nport the parser tests"


@pytest.mark.parametrize("raw", ["/fno:target x-1", "$fno:target x-1", "/target x-1"])
def test_a_disagreeing_family_verb_refuses_naming_both(raw):
    with pytest.raises(DispatchResolveError) as exc:
        node_seed(raw, "x-1", _planless())
    assert "/target" in str(exc.value)
    assert "/blueprint" in str(exc.value)


def test_the_dollar_spelling_derives_like_the_slash_spelling():
    assert resolve_effective_verb(
        verb="$fno:blueprint", difficulty="medium", plan_rung="none"
    ) == resolve_effective_verb(verb="/fno:blueprint", difficulty="medium", plan_rung="none")


def test_a_matching_family_verb_passes_through():
    assert node_seed("/fno:target x-1", "x-1", _planless(difficulty="low")) is None


# --- AC2-EDGE: seeds the table does not own pass unchanged ---------------------


@pytest.mark.parametrize(
    "seed", ["/fno:pr merged x-1", "/think x-1", "/fno:target x-1 --reconcile m.json"]
)
def test_seeds_the_table_does_not_own_pass(seed):
    assert node_seed(seed, "x-1", {"id": "x-1", "status": "done"}) is None


def test_an_empty_seed_composes_at_the_helper():
    # The helper composes a verbless empty seed; the SEAM never asks for it
    # (it skips empty seeds so the door's render_node_seed keeps the brief
    # env and the worktree ensure - asserted at the seam level).
    assert node_seed(None, "x-1", _planless()) == "/blueprint x-1"
    assert node_seed("   ", "x-1", _planless()) == "/blueprint x-1"


def test_an_unreadable_row_refuses_naming_the_node():
    with pytest.raises(DispatchResolveError) as exc:
        node_seed("port the parser tests", "x-1", None)
    assert "x-1" in str(exc.value)
    with pytest.raises(DispatchResolveError):
        node_seed("/fno:target x-1", "x-1", None)


def test_a_path_is_never_a_verb():
    assert harness_map._canonical_verb("/usr/bin/script") is None


def test_an_unanswerable_lifecycle_refuses():
    with pytest.raises(DispatchResolveError):
        node_seed("port it", "x-1", {"id": "x-1", "dispatch_verb": None, "plan_path": ""})
