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


def test_a_profile_without_a_model_key_keeps_the_top_level_provenance(
    tmp_path, monkeypatch
) -> None:
    """A profile sets an arbitrary subset.  One that does not name a model
    leaves the top-level `model` in force, so claiming the profile key as the
    source is itself a false provenance line."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(
        tmp_path,
        configured=1000000,
        config_extra='profile = "fast"\n[profiles.fast]\nmodel_reasoning_effort = "high"',
    )

    report = doctor._codex_context_window_report()

    assert report["model"] == "gpt-5.6-sol"
    assert report["model_source"] == "model"


def test_a_profile_naming_no_table_keeps_the_top_level_provenance(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, config_extra='profile = "nope"')

    assert doctor._codex_context_window_report()["model_source"] == "model"


def test_a_raised_cap_that_spares_the_clamp_is_still_flagged(tmp_path, monkeypatch) -> None:
    """gpt-5.4 sits at base 272000 and cap 1000000.  A configured 400000 escapes
    the clamp only because the last fetch won the raised cap, and reverts to
    258400 without it, so the cache caveat has to appear."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=1000000)

    report = doctor._codex_context_window_report()

    assert report["effective"] == 380000
    assert report["leans_on_cached_cap"] is True
    line = _emit(report)[0]
    assert "above its 272000 base" in line
    assert "drops this to 258400" in line


def test_the_harness_surface_report_carries_and_gates_the_check(tmp_path, monkeypatch) -> None:
    """Pin the wiring, not just the pieces: the key name, the codex-on-PATH
    gate, and the fact the emitter reads the same key doctor writes."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000)

    monkeypatch.setattr(doctor.shutil, "which", lambda name: None)
    assert "codex_context_window" not in doctor._harness_surface_report()

    monkeypatch.setattr(doctor.shutil, "which", lambda name: "/usr/local/bin/codex")
    surface = doctor._harness_surface_report()
    assert surface["codex_context_window"]["effective"] == 258400
    assert _emit_from_surface(surface) != []


def _emit_from_surface(surface: dict) -> list[str]:
    lines: list[str] = []
    doctor._emit_codex_context_window({"harness_surface": surface}, out=lines.append)
    return lines


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
    # The cap turns on surface AND originator, so the line must not pin it to
    # the originator alone: codex exec and codex app-server are served
    # differently under one originator string.
    assert "surface and originator both" in line
    assert "records neither" in line
    assert "its base" in line
    # No file-wide tier claim: one cache holds sol at base and luna above it.
    assert "tier" not in line


def test_emitted_line_omits_the_cap_clause_when_only_the_percent_bites(
    tmp_path, monkeypatch
) -> None:
    """A configured value UNDER the model's base never consulted the cached
    cap, so the per-fetcher caveat does not apply and must not be pasted on."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=200000, cached_max=872000)

    report = doctor._codex_context_window_report()
    lines = _emit(report)

    assert report["leans_on_cached_cap"] is False
    assert len(lines) == 1
    assert "190000" in lines[0]
    assert "originator" not in lines[0]


def test_emitter_is_silent_when_the_configured_value_is_honest(tmp_path, monkeypatch) -> None:
    """percent=100 and no clamp: the footer tells the truth, so say nothing."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=200000, cached_max=872000, percent=100)

    assert doctor._codex_context_window_report()["overstated"] is False
    assert _emit(doctor._codex_context_window_report()) == []


def test_emitter_is_silent_on_a_reason_only_report() -> None:
    assert _emit({"reason": "no-configured-window"}) == []


def test_a_missing_percent_still_reports_the_clamp(tmp_path, monkeypatch) -> None:
    """The clamp needs no percent to compute, so dropping that key upstream
    must not silence the primary lie.  The line then claims no percent."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=272000, percent=None)

    report = doctor._codex_context_window_report()

    assert report["effective"] == 272000
    assert report["percent"] is None
    line = _emit(report)[0]
    assert "runs at an effective 272000." in line
    assert "codex keeps" not in line


def test_a_missing_percent_under_a_raised_cap_does_not_crash(tmp_path, monkeypatch) -> None:
    """The raised-cap branch multiplies by the percent.  A None percent there
    took down the whole `fno doctor` run, since the emitter is called bare."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=1000000, percent=None)

    report = doctor._codex_context_window_report()

    assert report["percent"] is None
    assert report["leans_on_cached_cap"] is True
    line = _emit(report)[0]
    assert "drops this to 272000." in line


def test_a_missing_cap_falls_back_to_the_base_window(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=None)

    assert doctor._codex_context_window_report()["effective"] == 258400


def test_the_footer_claim_is_withheld_when_the_footer_is_honest(tmp_path, monkeypatch) -> None:
    """percent 100 with a raised cap: the cache is load-bearing so the line
    speaks, but the effective window IS the configured one, so the line must
    not also claim the footer overstates."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=1000000, percent=100)

    report = doctor._codex_context_window_report()

    assert report["overstated"] is False
    assert report["leans_on_cached_cap"] is True
    line = _emit(report)[0]
    assert "The TUI footer" not in line
    assert "above its 272000 base" in line


def test_a_profile_supplied_window_is_named_as_such(tmp_path, monkeypatch) -> None:
    """A reader who greps config.toml for the printed number has to find it."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(
        tmp_path,
        configured=100000,
        config_extra='profile = "big"\n[profiles.big]\nmodel_context_window = 1000000',
    )

    report = doctor._codex_context_window_report()

    assert report["configured"] == 1000000
    assert report["window_source"] == "profiles.big.model_context_window"
    assert report["model_source"] == "model"
    assert "profiles.big.model_context_window=1000000" in _emit(report)[0]


def test_a_live_app_server_daemon_is_named_as_the_thing_holding_the_cap_down(
    tmp_path, monkeypatch
) -> None:
    """Every app-server fetch lands the base cap whatever clientInfo name it
    presents, so a live daemon keeps pulling a raised cap back down.  That is
    the actionable half, and the operator only sees it if the line says it."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=272000)
    sock = tmp_path / "app-server-control" / "app-server-control.sock"
    sock.parent.mkdir(parents=True)

    assert "app-server daemon is live" not in _emit(doctor._codex_context_window_report())[0]

    sock.touch()
    assert "app-server daemon is live" in _emit(doctor._codex_context_window_report())[0]


def test_the_daemon_warning_reaches_the_raised_cap_branch(tmp_path, monkeypatch) -> None:
    """The raised-cap case is the one that NEEDS the warning: the daemon is
    what pulls 828400 back to 258400.  The clause used to sit in the other
    branch, so the run about to lose 570k tokens was the one told nothing."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=872000)
    (tmp_path / "app-server-control").mkdir()
    (tmp_path / "app-server-control" / "app-server-control.sock").touch()

    line = _emit(doctor._codex_context_window_report())[0]

    assert "above its 272000 base" in line
    assert "drops this to 258400" in line
    assert "app-server daemon is live" in line


def test_a_malformed_profiles_table_returns_a_reason_not_a_raise(
    tmp_path, monkeypatch
) -> None:
    """The caller swallows exceptions, so a raise drops the key from --json
    and destroys the difference between nothing-to-report and could-not-tell."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, config_extra='profile = "big"\nprofiles = "oops"')

    assert doctor._codex_context_window_report()["model"] == "gpt-5.6-sol"

    _write(
        tmp_path,
        configured=1000000,
        config_extra='profile = "big"\n[profiles]\nbig = "also-oops"',
    )

    assert doctor._codex_context_window_report()["model"] == "gpt-5.6-sol"


def test_a_malformed_cache_shape_returns_a_reason_not_a_raise(tmp_path, monkeypatch) -> None:
    """Same contract as the profiles branch: the caller swallows a raise, so a
    raise here drops the key from --json and hides that anything went wrong."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    (tmp_path / "config.toml").write_text(
        'model = "gpt-5.6-sol"\nmodel_context_window = 1000000\n'
    )

    for shape in ('{"models": "nope"}', '{"models": ["nope"]}', '"nope"', "[]"):
        (tmp_path / "models_cache.json").write_text(shape)
        report = doctor._codex_context_window_report()
        assert "reason" in report, shape


def test_a_float_cap_is_honored_like_a_float_percent(tmp_path, monkeypatch) -> None:
    """Rejecting a float cap substituted the base and reported a clamp that is
    not real, a false positive in the direction the check exists to catch."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=400000, cached_max=872000.0)

    report = doctor._codex_context_window_report()

    assert report["max_context_window"] == 872000
    assert report["effective"] == 380000


def test_a_synthesized_cap_says_the_cache_carries_none(tmp_path, monkeypatch) -> None:
    """A missing max_context_window makes `cap` a copy of the base that the
    cache never carried.  Calling that 'its base' is a false cache fact."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=None)

    report = doctor._codex_context_window_report()

    assert report["cap_synthesized"] is True
    line = _emit(report)[0]
    assert "carries no cap for gpt-5.6-sol" in line
    assert "its base" not in line


def test_a_synthesized_cap_is_not_called_the_base(tmp_path, monkeypatch) -> None:
    """A missing max_context_window makes `cap` a copy of the base that the
    cache never carried, and a cap BELOW base is upstream nonsense.  Neither
    earns the phrase 'its base' in a check about numbers that lie."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write(tmp_path, configured=1000000, cached_max=200000)

    line = _emit(doctor._codex_context_window_report())[0]

    assert "puts gpt-5.6-sol at 200000" in line
    assert "its base" not in line
