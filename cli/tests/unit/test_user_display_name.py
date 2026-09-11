"""Unit tests for the human-naming surface: ``config.user.name`` and
``display_name()`` (the one resolver every address site reads)."""
from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import fno.config
from fno.user import UserBlock, display_name, display_name_at

_ROOT = Path("/tmp/does-not-exist-dd7b")


def _settings(name: str) -> SimpleNamespace:
    return SimpleNamespace(user=UserBlock(name=name))


def test_user_block_default_is_empty() -> None:
    assert UserBlock().name == ""


def test_configured_name_wins(monkeypatch) -> None:
    monkeypatch.setattr(
        fno.config, "load_settings_for_repo", lambda root: _settings("Jason")
    )
    display_name_at.cache_clear()
    try:
        assert display_name_at(_ROOT) == "Jason"
    finally:
        display_name_at.cache_clear()


def test_git_identity_fallback(monkeypatch) -> None:
    monkeypatch.setattr(
        fno.config, "load_settings_for_repo", lambda root: _settings("")
    )
    monkeypatch.setattr(
        "fno.user.subprocess.run",
        lambda *a, **k: SimpleNamespace(stdout="J.N. Choi\n"),
    )
    display_name_at.cache_clear()
    try:
        assert display_name_at(_ROOT) == "J.N. Choi"
    finally:
        display_name_at.cache_clear()


def test_no_name_anywhere_says_you(monkeypatch) -> None:
    monkeypatch.setattr(
        fno.config, "load_settings_for_repo", lambda root: _settings("")
    )
    monkeypatch.setattr(
        "fno.user.subprocess.run",
        lambda *a, **k: SimpleNamespace(stdout="  \n"),
    )
    display_name_at.cache_clear()
    try:
        assert display_name_at(_ROOT) == "you"
    finally:
        display_name_at.cache_clear()


def test_subprocess_failure_says_you(monkeypatch) -> None:
    monkeypatch.setattr(
        fno.config, "load_settings_for_repo", lambda root: _settings("")
    )

    def _boom(*a, **k):
        raise OSError("no git")

    monkeypatch.setattr("fno.user.subprocess.run", _boom)
    display_name_at.cache_clear()
    try:
        assert display_name_at(_ROOT) == "you"
    finally:
        display_name_at.cache_clear()


def test_wrapper_resolves_this_sessions_root(monkeypatch) -> None:
    import fno.paths

    monkeypatch.setattr(
        fno.paths, "resolve_repo_root", lambda: _ROOT, raising=True
    )
    monkeypatch.setattr(
        "fno.user.display_name_at", lambda root: f"named-at-{root}"
    )
    assert display_name() == f"named-at-{_ROOT}"
