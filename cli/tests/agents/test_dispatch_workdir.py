"""ab-77b691dc Wave 1.2: Python --fresh parity for the agents dispatch workdir.

``_resolve_dispatch_workdir`` mirrors the Rust client's ``effective_worker_cwd``
precedence (AC6): ``--cwd`` > ``--fresh`` (canonical) > caller cwd; ``--here``
suppresses ``--fresh``; a canonical that equals the caller is a no-op (AC5).

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

from fno.agents.cli import _resolve_dispatch_workdir


@pytest.fixture(autouse=True)
def _clear_repo_root_env(monkeypatch):
    # Each test sets FNO_REPO_ROOT explicitly; start clean so a developer's
    # ambient env never leaks the canonical root into the assertions.
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    yield


def test_explicit_cwd_wins_over_fresh(monkeypatch):
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir("/explicit/dir", fresh=True, here=False)
    assert got == Path("/explicit/dir").resolve()


def test_no_fresh_keeps_caller(monkeypatch):
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(None, fresh=False, here=False)
    assert got == Path("/worktree").resolve()


def test_here_suppresses_fresh(monkeypatch):
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(None, fresh=True, here=True)
    assert got == Path("/worktree").resolve()


def test_fresh_resolves_canonical(monkeypatch, capsys):
    monkeypatch.setattr(os, "getcwd", lambda: "/worktree")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(None, fresh=True, here=False)
    assert got == Path("/canonical").resolve()
    # The redirect is never silent (Failure Modes > Errors).
    assert "dispatching from canonical main" in capsys.readouterr().err


def test_fresh_noop_when_canonical_is_caller(monkeypatch, capsys):
    monkeypatch.setattr(os, "getcwd", lambda: "/canonical")
    monkeypatch.setenv("FNO_REPO_ROOT", "/canonical")
    got = _resolve_dispatch_workdir(None, fresh=True, here=False)
    assert got == Path("/canonical").resolve()
    # No note when dispatch already starts at canonical (AC5 no-op).
    assert "dispatching from canonical main" not in capsys.readouterr().err
