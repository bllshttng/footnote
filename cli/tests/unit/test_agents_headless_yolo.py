"""config.agents.<provider>.headless_yolo schema + resolver (bounded-posture amendment).

`headless_yolo` selects FULL yolo (`true`, unsandboxed bypass) vs the BOUNDED
posture (`false`/absent, the default: sandboxed AND never-prompt). Both never
prompt, so the resolver degrades to the hang-safe BOUNDED default (`False`) on
any read failure - a typo can never re-introduce the headless hang AND never
silently drops the sandbox into a full bypass.
"""
from __future__ import annotations

from pathlib import Path

from fno import config as config_mod


def _load(tmp_path: Path, monkeypatch, content: str) -> None:
    d = tmp_path / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    f = d / "settings.yaml"
    f.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(f))
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def test_default_false_bounded_both_providers(tmp_path, monkeypatch):
    """No agents block -> BOUNDED default (False) for both providers."""
    _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert config_mod.agents_headless_yolo("codex") is False
    assert config_mod.agents_headless_yolo("gemini") is False


def test_full_yolo_opt_in_is_per_provider(tmp_path, monkeypatch):
    """`headless_yolo: true` opts into full yolo, per-provider (no leak)."""
    _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    gemini:\n      headless_yolo: true\n",
    )
    assert config_mod.agents_headless_yolo("gemini") is True
    # codex untouched -> still the bounded default.
    assert config_mod.agents_headless_yolo("codex") is False


def test_codex_full_yolo_opt_in(tmp_path, monkeypatch):
    _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    codex:\n      headless_yolo: true\n",
    )
    assert config_mod.agents_headless_yolo("codex") is True
    assert config_mod.agents_headless_yolo("gemini") is False


def test_explicit_false_is_bounded(tmp_path, monkeypatch):
    """An explicit `false` is the bounded default (read back as False)."""
    _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    gemini:\n      headless_yolo: false\n",
    )
    assert config_mod.agents_headless_yolo("gemini") is False


def test_malformed_provider_block_degrades_bounded_without_crashing(tmp_path, monkeypatch):
    """A non-mapping provider block degrades to the hang-safe BOUNDED default,
    and must NOT raise out of the whole settings load."""
    _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  agents:\n    gemini: banana\n",
    )
    assert config_mod.load_settings() is not None
    assert config_mod.agents_headless_yolo("gemini") is False


def test_unknown_provider_defaults_bounded(tmp_path, monkeypatch):
    """An unrecognized provider (e.g. claude, which is unaffected) is bounded False."""
    _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert config_mod.agents_headless_yolo("claude") is False
