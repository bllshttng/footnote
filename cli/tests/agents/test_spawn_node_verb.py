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


# --- the spawn seam (rust_runtime._node_seed_at_seam) -------------------------


def _seam(monkeypatch, rec):
    import fno.agents.rust_runtime as rr

    monkeypatch.setattr("fno.agents.node_dispatch.find_node_row", lambda node: rec)
    return rr._node_seed_at_seam


def test_the_seam_composes_a_prose_seed(monkeypatch):
    seam = _seam(monkeypatch, {"id": "x-1", "dispatch_verb": None, "difficulty": "medium"})
    args = ["spawn", "--name", "w", "--node", "x-1", "port the parser"]
    out = list(seam(args))
    from fno.agents.spawn_defaults import _seed_of

    assert _seed_of(out[1:]) == "/blueprint x-1\n\nport the parser"
    assert out[0] == "spawn" and "--node" in out


def test_the_seam_passes_a_matching_family_verb_byte_identically(monkeypatch):
    seam = _seam(monkeypatch, {"id": "x-1", "dispatch_verb": None, "difficulty": "low"})
    args = ["spawn", "--name", "w", "--node", "x-1", "/fno:target x-1", "--cwd", "/tmp/w"]
    assert list(seam(args)) == args


@pytest.mark.parametrize(
    "args",
    [
        ["spawn", "--name", "w", "--node", "x-1", "/fno:pr merged x-1"],
        ["spawn", "--name", "w", "--node", "x-1", "/fno:target x-1 --reconcile m.json"],
        ["spawn", "--name", "w", "--node", "x-1", "port the parser", "--crown"],
        ["spawn", "--name", "w", "-kfno", "--node", "x-1", "port the parser"],
        ["spawn", "--name", "w", "--node", "x-1", "--resume", "abc", "port the parser"],
        ["spawn", "--name", "w", "--node", "x-1"],
        ["spawn", "--name", "w", "just prose"],
        ["spawn", "--name", "w", "prose with FNO_NODE set"],
    ],
)
def test_the_seam_leaves_non_dispatch_spawns_alone(monkeypatch, args):
    seam = _seam(monkeypatch, {"id": "x-1", "dispatch_verb": None, "difficulty": "medium"})
    assert list(seam(args)) == args


def test_the_seam_reads_only_the_flag_never_the_env(monkeypatch):
    import fno.agents.rust_runtime as rr

    def _boom(node):
        raise AssertionError("no graph read may happen without the --node flag")

    monkeypatch.setattr("fno.agents.node_dispatch.find_node_row", _boom)
    monkeypatch.setenv("FNO_NODE", "x-1")
    args = ["spawn", "--name", "w", "port the parser"]
    assert list(rr._node_seed_at_seam(args)) == args


def test_the_seam_refuses_an_unreadable_row(monkeypatch):
    seam = _seam(monkeypatch, None)
    with pytest.raises(SystemExit) as exc:
        seam(["spawn", "--name", "w", "--node", "x-1", "/fno:target x-1"])
    assert exc.value.code == 2


def test_make_context_refuses_a_disagreeing_verb_before_defaults_or_route(
    monkeypatch,
):
    """AC7-HP: the refusal sits before inject_spawn_defaults and before the
    Rust route, and names the verb the node actually derives."""
    from typer.testing import CliRunner

    import fno.agents.rust_runtime as rr
    import fno.agents.spawn_defaults as sd
    from fno.agents.cli import agents_app

    injected = []
    monkeypatch.setattr(
        sd, "inject_spawn_defaults", lambda args: injected.append(args) or args
    )
    monkeypatch.setattr(
        "fno.agents.node_dispatch.find_node_row",
        lambda node: {"id": "x-1", "dispatch_verb": None, "difficulty": "medium"},
    )
    routed = []
    monkeypatch.setattr("fno.agents.rust_runtime.route_to_rust", lambda args, **k: routed.append(args))
    res = CliRunner().invoke(
        agents_app, ["spawn", "--name", "w", "/fno:target x-1", "--node", "x-1"]
    )
    assert res.exit_code == 2, res.output
    assert "/blueprint" in res.output
    assert injected == []
    assert routed == []

