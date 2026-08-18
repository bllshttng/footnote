"""The codex effective-context-window check.

Codex clamps ``model_context_window`` to the model's server-supplied
``max_context_window`` and then keeps ``effective_context_window_percent`` of
it.  The TUI footer echoes the CONFIGURED number, so a config asking for 1M
reads as 1M while every turn runs at the clamped value.  These tests pin the
arithmetic, the one condition that speaks, and the claims the emitted line is
allowed to make.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno import doctor

# One real fetch holds both, which is why no file-wide "tier" label is true.
MODELS = [
    {
        "slug": "gpt-5.6-sol",
        "context_window": 272000,
        "max_context_window": 272000,
        "effective_context_window_percent": 95,
    },
    {
        "slug": "gpt-5.6-luna",
        "context_window": 272000,
        "max_context_window": 872000,
        "effective_context_window_percent": 95,
    },
]


def _write(
    home: Path,
    *,
    configured: int | None,
    cached_max: int = 272000,
    config_extra: str = "",
    percent: object = 95,
) -> None:
    lines = ['model = "gpt-5.6-sol"']
    if configured is not None:
        lines.append(f"model_context_window = {configured}")
    if config_extra:
        lines.append(config_extra)
    (home / "config.toml").write_text("\n".join(lines) + "\n")
    models = [dict(m) for m in MODELS]
    models[0]["max_context_window"] = cached_max
    models[0]["effective_context_window_percent"] = percent
    (home / "models_cache.json").write_text(
        json.dumps(
            {
                "fetched_at": "2026-08-18T19:13:37.907036Z",
                "client_version": "0.147.0",
                "models": models,
            }
        )
    )


def test_configured_over_cap_reports_the_clamped_window(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=272000)

    report = doctor._codex_context_window_report()

    assert report["overstated"] is True
    assert report["configured"] == 1000000
    assert report["max_context_window"] == 272000
    assert report["effective"] == 258400


def test_extended_cap_in_cache_raises_the_same_config(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=872000)

    report = doctor._codex_context_window_report()

    assert report["overstated"] is True
    assert report["effective"] == 828400


def test_percent_shrink_alone_still_overstates(tmp_path, monkeypatch) -> None:
    """400000 under an 872000 cap still runs at 380000, and the footer says
    400000.  That is the same lie as the clamp, so it must not be silent."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=872000)

    report = doctor._codex_context_window_report()

    assert report["effective"] == 380000
    assert report["overstated"] is True


def test_a_profile_redirects_which_model_is_described(tmp_path, monkeypatch) -> None:
    """A `profile` key overrides model selection, so reading the top-level
    `model` past one describes a model no thread runs."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(
        tmp_path,
        configured=1000000,
        config_extra='profile = "big"\n[profiles.big]\nmodel = "gpt-5.6-luna"',
    )

    report = doctor._codex_context_window_report()

    assert report["model"] == "gpt-5.6-luna"
    assert report["model_source"] == "profiles.big.model"
    assert report["max_context_window"] == 872000
    assert report["effective"] == 828400


def test_no_configured_window_reports_a_reason(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=None)

    assert doctor._codex_context_window_report() == {"reason": "no-configured-window"}


def test_missing_cache_reports_a_reason(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    (tmp_path / "config.toml").write_text(
        'model = "gpt-5.6-sol"\nmodel_context_window = 1000000\n'
    )

    assert doctor._codex_context_window_report()["reason"].startswith("unreadable-codex-home")


def test_model_absent_from_cache_reports_a_reason(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000)
    (tmp_path / "config.toml").write_text(
        'model = "gpt-6-unreleased"\nmodel_context_window = 1000000\n'
    )

    report = doctor._codex_context_window_report()

    assert report == {"reason": "model-not-in-cache: gpt-6-unreleased"}


def test_a_float_percent_is_not_schema_drift(tmp_path, monkeypatch) -> None:
    """Upstream sending 95.0 instead of 95 must not silently retire the check."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, percent=95.0)

    assert doctor._codex_context_window_report()["effective"] == 258400


def _emit(report: dict) -> list[str]:
    lines: list[str] = []
    doctor._emit_codex_context_window(
        {"harness_surface": {"codex_context_window": report}}, out=lines.append
    )
    return lines


def test_emitted_line_names_the_numbers_and_the_cache_provenance(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=272000)

    lines = _emit(doctor._codex_context_window_report())

    assert len(lines) == 1
    line = lines[0]
    assert "1000000" in line
    assert "272000" in line
    assert "258400" in line
    assert "gpt-5.6-sol" in line
    assert "2026-08-18T19:13:37.907036Z" in line
    assert "records none" in line
    # No file-wide tier claim: one cache holds sol at base and luna above it.
    assert "tier" not in line


def test_emitted_line_omits_the_cap_clause_when_only_the_percent_bites(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=872000)

    lines = _emit(doctor._codex_context_window_report())

    assert len(lines) == 1
    assert "380000" in lines[0]
    assert "originator" not in lines[0]


def test_emitter_is_silent_when_the_configured_value_is_honest(tmp_path, monkeypatch) -> None:
    """percent=100 and no clamp: the footer tells the truth, so say nothing."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=200000, cached_max=872000, percent=100)

    assert doctor._codex_context_window_report()["overstated"] is False
    assert _emit(doctor._codex_context_window_report()) == []


def test_emitter_is_silent_on_a_reason_only_report() -> None:
    assert _emit({"reason": "no-configured-window"}) == []
