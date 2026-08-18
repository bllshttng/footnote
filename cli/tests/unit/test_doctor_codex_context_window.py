"""The codex effective-context-window check.

Codex clamps ``model_context_window`` to the model's server-supplied
``max_context_window`` and then keeps ``effective_context_window_percent`` of
it.  The TUI footer echoes the CONFIGURED number, so a config asking for 1M
reads as 1M while every turn runs at the clamped value.  These tests pin the
arithmetic and the one condition that speaks.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno import doctor


def _write(home: Path, *, configured: int | None, cached_max: int, base: int = 272000) -> None:
    lines = ['model = "gpt-5.6-sol"']
    if configured is not None:
        lines.append(f"model_context_window = {configured}")
    (home / "config.toml").write_text("\n".join(lines) + "\n")
    (home / "models_cache.json").write_text(
        json.dumps(
            {
                "fetched_at": "2026-08-18T19:13:37.907036Z",
                "client_version": "0.147.0",
                "models": [
                    {
                        "slug": "gpt-5.6-sol",
                        "context_window": base,
                        "max_context_window": cached_max,
                        "effective_context_window_percent": 95,
                    }
                ],
            }
        )
    )


def test_configured_over_cap_reports_the_clamped_window(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=272000)

    report = doctor._codex_context_window_report()

    assert report["capped"] is True
    assert report["configured"] == 1000000
    assert report["max_context_window"] == 272000
    assert report["effective"] == 258400
    assert report["tier"] == "base"


def test_extended_tier_in_cache_raises_the_same_config(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=872000)

    report = doctor._codex_context_window_report()

    assert report["capped"] is True
    assert report["effective"] == 828400
    assert report["tier"] == "extended"


def test_configured_under_cap_is_silent(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=872000)

    report = doctor._codex_context_window_report()

    assert report["capped"] is False
    assert report["effective"] == 380000


def test_no_configured_window_is_silent(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=None, cached_max=872000)

    assert doctor._codex_context_window_report() == {}


def test_missing_cache_is_silent(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    (tmp_path / "config.toml").write_text(
        'model = "gpt-5.6-sol"\nmodel_context_window = 1000000\n'
    )

    assert doctor._codex_context_window_report() == {}


def test_emitted_line_names_the_numbers_and_the_cache_provenance(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=272000)

    lines: list[str] = []
    doctor._emit_codex_context_window(
        {"harness_surface": {"codex_context_window": doctor._codex_context_window_report()}},
        out=lines.append,
    )

    assert len(lines) == 1
    line = lines[0]
    assert "1000000" in line
    assert "272000" in line
    assert "258400" in line
    assert "gpt-5.6-sol" in line
    assert "base tier" in line
    assert "2026-08-18T19:13:37.907036Z" in line
    assert "no originator" in line


def test_emitter_is_silent_when_uncapped(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=872000)

    lines: list[str] = []
    doctor._emit_codex_context_window(
        {"harness_surface": {"codex_context_window": doctor._codex_context_window_report()}},
        out=lines.append,
    )

    assert lines == []
