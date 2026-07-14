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
