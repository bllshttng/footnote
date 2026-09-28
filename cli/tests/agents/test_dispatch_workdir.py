"""x-85fe: Python dispatch-workdir precedence after the default inversion.

``_resolve_dispatch_workdir`` mirrors the Rust client's ``effective_worker_cwd``
precedence: ``--cwd`` > ``--here`` (caller) > default canonical. x-85fe inverted
the ab-77b691dc default: a spawn with NO explicit cwd source now lands on the
canonical (main) checkout so the identical command behaves the same regardless of
where the launcher stands; ``--here``/``--in-place`` is the explicit opt-in to
keep the caller's cwd; ``--fresh`` survives as an accepted no-op alias. A canonical
that equals the caller is a no-op (no redirect note).

Canonical resolution is driven through ``FNO_REPO_ROOT`` (the documented test
hook on ``resolve_canonical_repo_root``) so these stay git-fixture-free; the real
git-worktree resolution is proven on the Rust side
(``canonical_repo_root_resolves_main_from_linked_worktree``) and by
``test_resolve_canonical_worktree.py``.
"""
from __future__ import annotations

import os
from pathlib import Path

import pytest
import typer

from fno.agents.cli import _resolve_dispatch_workdir


@pytest.fixture(autouse=True)
def _clear_repo_root_env(monkeypatch):
    # Each test sets FNO_REPO_ROOT explicitly; start clean so a developer's
    # ambient env never leaks the canonical root into the assertions.
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    yield


def test_explicit_cwd_wins(monkeypatch):
    # --cwd is the highest-priority source and wins over every flag (AC2-ERR).
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir("/explicit/dir", fresh=True, here=True)
    assert got == Path("/explicit/dir").resolve()


def test_default_resolves_canonical(monkeypatch, capsys):
    # No flags -> the inverted default lands on canonical, never silent (AC1-HP).
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(None, fresh=False, here=False)
    assert got == Path("/canonical").resolve()
    assert "dispatching from canonical main" in capsys.readouterr().err


def test_here_keeps_caller(monkeypatch, capsys):
    # --here is the explicit opt-in to stay in the caller's worktree (AC2-HP).
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(None, fresh=False, here=True)
    assert got == Path("/worktree").resolve()
    # --here stays put, so no redirect note fires.
    assert "dispatching from canonical main" not in capsys.readouterr().err


def test_empty_cwd_is_absent(monkeypatch, capsys):
    # An empty --cwd is absent, never the empty-string path (Failure Modes >
    # Boundaries) -> falls through to the canonical default. The Rust twin
    # (resolve_dispatch_cwd) filters the empty string for the same reason
    # (x-85fe review; codex #2).
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir("", fresh=False, here=False)
    assert got == Path("/canonical").resolve()
    assert "dispatching from canonical main" in capsys.readouterr().err


def test_fresh_is_noop_alias(monkeypatch):
    # --fresh survives as an accepted no-op alias: identical to passing nothing
    # (AC2-EDGE), the default already being canonical.
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    with_fresh = _resolve_dispatch_workdir(None, fresh=True, here=False)
    without = _resolve_dispatch_workdir(None, fresh=False, here=False)
    assert with_fresh == without == Path("/canonical").resolve()


def test_default_noop_when_canonical_is_caller(monkeypatch, capsys):
    # Caller already on canonical -> byte-identical to today, no note (AC1-EDGE).
    monkeypatch.setattr(os, "getcwd", lambda: "/canonical")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(None, fresh=False, here=False)
    assert got == Path("/canonical").resolve()
    assert "dispatching from canonical main" not in capsys.readouterr().err


def test_resolution_failure_falls_back_to_caller(monkeypatch, capsys):
    # A canonical-resolution exception degrades to the caller cwd, never blocks
    # the dispatch (AC1-ERR; Failure Modes > Errors).
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")

    def _boom():
        raise RuntimeError("no git here")

    monkeypatch.setattr(
        "fno.paths.resolve_canonical_repo_root", _boom, raising=True
    )
    got = _resolve_dispatch_workdir(None, fresh=False, here=False)
    assert got == Path("/worktree").resolve()
    assert "dispatching from canonical main" not in capsys.readouterr().err


# ---------------------------------------------------------------------------
# Cross-layer parity (x-85fe US4)
#
# The precedence table below is the executable statement of the three-surface
# invariant: the Python resolver here and the Rust `effective_worker_cwd` unit
# tests (crates/fno-agents/src/bin/client.rs `effective_cwd_*`) assert the SAME
# rows. Inputs are (explicit_cwd, here, fresh) with a fixed canonical=/canonical
# and caller=/worktree; the Rust mirror uses /canon and /wt. Keep the two in
# lockstep - a one-surface change that breaks a row fails this table.
# ---------------------------------------------------------------------------

# (explicit, here, fresh) -> expected root (None expected => canonical)
_PRECEDENCE_TABLE = [
    (None, False, False, "/canonical"),  # default -> canonical
    (None, False, True, "/canonical"),   # --fresh no-op alias -> canonical
    (None, True, False, "/worktree"),    # --here -> caller
    (None, True, True, "/worktree"),     # --here wins over --fresh alias
    ("/explicit", False, False, "/explicit"),  # --cwd wins
    ("/explicit", True, True, "/explicit"),    # --cwd beats every flag
]


@pytest.mark.parametrize("explicit,here,fresh,expected", _PRECEDENCE_TABLE)
def test_cross_layer_precedence_parity(monkeypatch, explicit, here, fresh, expected):
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(explicit, fresh=fresh, here=here)
    assert got == Path(expected).resolve()


# ---------------------------------------------------------------------------
# Node-named spawn guard
#
# A spawn whose prompt names a node launches from that node's project cwd; a
# caller standing in another git repo is refused naming both. Mirrors the Rust
# `spawn_node_cwd_in` tests (crates/fno-agents/src/node_seed.rs): two canonical
# repos, /repo and /other; anything else is outside git.
# ---------------------------------------------------------------------------

_ROWS = {
    "x-1111": {"id": "x-1111", "cwd": "/repo/footnote"},
    "x-2222": {"id": "x-2222", "cwd": ""},
    "x-3333": {"id": "x-3333", "cwd": "/other/project"},
}


def _install_graph_and_repos(monkeypatch):
    monkeypatch.setattr(
        "fno.agents.node_dispatch.find_node_row",
        lambda node: _ROWS.get(node),
        raising=True,
    )

    def _repo_of(cwd):
        text = str(cwd)
        if text.startswith("/repo"):
            return Path("/repo")
        if text.startswith("/other"):
            return Path("/other")
        return None

    monkeypatch.setattr(
        "fno.paths.resolve_canonical_worktree", _repo_of, raising=True
    )


def test_node_named_seed_dispatches_from_node_project(monkeypatch):
    _install_graph_and_repos(monkeypatch)
    monkeypatch.setattr(os, "getcwd", lambda: "/repo/wt")
    got = _resolve_dispatch_workdir(
        None, fresh=False, here=False, message="/fno:target x-1111"
    )
    assert got == Path("/repo/footnote")


def test_foreign_repo_caller_is_refused_naming_both(monkeypatch, capsys):
    _install_graph_and_repos(monkeypatch)
    monkeypatch.setattr(os, "getcwd", lambda: "/repo/wt")
    with pytest.raises(typer.Exit) as exc_info:
        _resolve_dispatch_workdir(
            None, fresh=False, here=False, message="/fno:target x-3333"
        )
    assert exc_info.value.exit_code == 2
    err = capsys.readouterr().err
    assert "/other/project" in err
    assert "/repo" in err


def test_caller_outside_git_dispatches_from_node_project(monkeypatch):
    _install_graph_and_repos(monkeypatch)
    monkeypatch.setattr(os, "getcwd", lambda: "/nowhere/tmp")
    got = _resolve_dispatch_workdir(
        None, fresh=False, here=False, message="/fno:target x-3333"
    )
    assert got == Path("/other/project")


def test_unnamed_seed_keeps_canonical_default(monkeypatch, capsys):
    _install_graph_and_repos(monkeypatch)
    monkeypatch.setattr(os, "getcwd", lambda: "/repo/wt")
    monkeypatch.setenv("FNO_REPO_ROOT", "/repo")
    # Prose names no node; an unknown id names no readable row. Both fall
    # through to the pre-existing canonical default.
    for message in ("port the auth flow", "/fno:target x-9999", "/fno:target x-2222"):
        got = _resolve_dispatch_workdir(
            None, fresh=False, here=False, message=message
        )
        assert got == Path("/repo"), message
    assert "node project" not in capsys.readouterr().err


def test_quoted_title_never_binds_a_node(monkeypatch):
    _install_graph_and_repos(monkeypatch)
    monkeypatch.setattr(os, "getcwd", lambda: "/repo/wt")
    monkeypatch.setenv("FNO_REPO_ROOT", "/repo")
    got = _resolve_dispatch_workdir(
        None,
        fresh=False,
        here=False,
        message='/fno:target "stop treating x-1111 as a node"',
    )
    assert got == Path("/repo")


def test_explicit_node_flag_beats_the_seed_scan(monkeypatch):
    _install_graph_and_repos(monkeypatch)
    monkeypatch.setattr(os, "getcwd", lambda: "/repo/wt")
    with pytest.raises(typer.Exit):
        _resolve_dispatch_workdir(
            None,
            fresh=False,
            here=False,
            message="/fno:target x-1111",
            node="x-3333",
        )


def test_here_and_cwd_bypass_the_guard(monkeypatch):
    _install_graph_and_repos(monkeypatch)
    monkeypatch.setattr(os, "getcwd", lambda: "/repo/wt")
    got = _resolve_dispatch_workdir(
        None, fresh=False, here=True, message="/fno:target x-3333"
    )
    assert got == Path("/repo/wt")
    got = _resolve_dispatch_workdir(
        "/explicit/dir", fresh=False, here=False, message="/fno:target x-3333"
    )
    assert got == Path("/explicit/dir").resolve()
