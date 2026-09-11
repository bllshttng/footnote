"""Unit tests for the human-naming surface: ``config.user.name`` and
``display_name()`` (the one resolver every address site reads)."""
from __future__ import annotations

from types import SimpleNamespace

import fno.config
from fno.user import UserBlock, display_name


def _settings(name: str) -> SimpleNamespace:
    return SimpleNamespace(user=UserBlock(name=name))


def test_user_block_default_is_empty() -> None:
    assert UserBlock().name == ""


def test_configured_name_wins(monkeypatch) -> None:
    monkeypatch.setattr(fno.config, "load_settings", lambda: _settings("Jason"))
    display_name.cache_clear()
    try:
        assert display_name() == "Jason"
    finally:
        display_name.cache_clear()


def test_git_identity_fallback(monkeypatch) -> None:
    monkeypatch.setattr(fno.config, "load_settings", lambda: _settings(""))
    monkeypatch.setattr(
        "fno.user.subprocess.run",
        lambda *a, **k: SimpleNamespace(stdout="J.N. Choi\n"),
    )
    display_name.cache_clear()
    try:
        assert display_name() == "J.N. Choi"
    finally:
        display_name.cache_clear()


def test_no_name_anywhere_says_you(monkeypatch) -> None:
    monkeypatch.setattr(fno.config, "load_settings", lambda: _settings(""))
    monkeypatch.setattr(
        "fno.user.subprocess.run",
        lambda *a, **k: SimpleNamespace(stdout="  \n"),
    )
    display_name.cache_clear()
    try:
        assert display_name() == "you"
    finally:
        display_name.cache_clear()


def test_subprocess_failure_says_you(monkeypatch) -> None:
    monkeypatch.setattr(fno.config, "load_settings", lambda: _settings(""))

    def _boom(*a, **k):
        raise OSError("no git")

    monkeypatch.setattr("fno.user.subprocess.run", _boom)
    display_name.cache_clear()
    try:
        assert display_name() == "you"
    finally:
        display_name.cache_clear()
