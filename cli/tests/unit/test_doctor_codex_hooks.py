"""Focused tests for the advisory ``fno doctor --codex-hooks`` mode."""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import doctor
from fno.cli import app

runner = CliRunner()
HOOK = "/opt/footnote/hooks/session-start.sh"


def _write_toml(config: Path, *, trusted: bool = True) -> str:
    state_key = f"{config.absolute()}:session_start:0:0"
    trust = (
        f"\n[hooks.state.{json.dumps(state_key)}]\ntrusted_hash = \"sha256:test\"\n"
        if trusted
        else ""
    )
    config.write_text(
        "[[hooks.SessionStart]]\n\n"
        "[[hooks.SessionStart.hooks]]\n"
        'type = "command"\n'
        f"command = {json.dumps(f'env FNO_PLATFORM=codex {HOOK}')}\n"
        f"{trust}"
    )
    return state_key


def _write_json(path: Path, *commands: str) -> None:
    path.write_text(
        json.dumps(
            {
                "hooks": {
                    "SessionStart": [
                        {
                            "hooks": [
                                {"type": "command", "command": command}
                                for command in commands
                            ]
                        }
                    ]
                }
            }
        )
    )


def test_toml_footnote_hook_with_recorded_hash_is_unverified(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    state_key = _write_toml(config)

    result = runner.invoke(app, ["doctor", "--codex-hooks"])

    assert result.exit_code == 0
    assert "fno doctor: codex hooks: warn" in result.stdout
    assert "preferred=config.toml" in result.stdout
    assert "footnote SessionStart=wired" in result.stdout
    assert f"trust state recorded-unverified: {state_key}" in result.stdout
    assert "trusted_hash was not locally verified" in result.stdout


def test_both_layers_with_foreign_json_warns_and_preserves_paths(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    foreign = "bash '/Users/test/.codex/herdr-agent-state.sh' session"
    _write_toml(config)
    _write_json(legacy, foreign)

    result = runner.invoke(app, ["doctor", "--codex-hooks"])

    assert result.exit_code == 0
    assert "fno doctor: codex hooks: warn" in result.stdout
    assert f"loading hooks from both {legacy} and {config}" in result.stdout
    assert f"foreign legacy JSON hook preserved: {foreign}" in result.stdout
    assert f"manually consolidate it into {config}" in result.stdout
    assert "--migrate-legacy-hooks-json" not in result.stdout


def test_foreign_non_session_event_keeps_json_layer_visible(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    _write_toml(config)
    legacy.write_text(
        json.dumps(
            {
                "hooks": {
                    "Stop": [
                        {
                            "hooks": [
                                {"type": "command", "command": "foreign-stop.sh"}
                            ]
                        }
                    ]
                }
            }
        )
    )

    result = runner.invoke(app, ["doctor", "--codex-hooks"])

    assert result.exit_code == 0
    assert "layers=both" in result.stdout
    assert f"loading hooks from both {legacy} and {config}" in result.stdout
    assert "foreign legacy JSON hook preserved: foreign-stop.sh" in result.stdout
    assert "manually consolidate" in result.stdout


@pytest.mark.parametrize("malformed", ["toml", "json"])
def test_malformed_layer_reports_error_but_exits_zero(
    tmp_path, monkeypatch, malformed
) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    if malformed == "toml":
        config.write_text("[[not valid")
        bad_path = config
    else:
        _write_toml(config)
        legacy.write_text("{not json")
        bad_path = legacy

    result = runner.invoke(app, ["doctor", "--codex-hooks"])

    assert result.exit_code == 0
    assert "fno doctor: codex hooks: error" in result.stdout
    assert "parse error" in result.stdout
    assert str(bad_path) in result.stdout


def test_neither_layer_warns_with_setup_action(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))

    result = runner.invoke(app, ["doctor", "--codex-hooks"])

    assert result.exit_code == 0
    assert "fno doctor: codex hooks: warn" in result.stdout
    assert "layers=neither" in result.stdout
    assert "`fno setup cli-hooks-codex`" in result.stdout


def test_json_output_is_one_parseable_object(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    state_key = _write_toml(config)

    result = runner.invoke(app, ["doctor", "--codex-hooks", "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.stdout.strip())
    assert payload == {
        "status": "warn",
        "preferred_layer": "config.toml",
        "state": "toml-only",
        "config_path": str(config),
        "hooks_json_path": str(tmp_path / "hooks.json"),
        "footnote_toml_wired": True,
        "footnote_toml_trust_verified": False,
        "footnote_toml_trust": {state_key: "recorded-unverified"},
        "duplicate_layers": False,
        "footnote_json_hooks": [],
        "foreign_json_hooks": [],
        "errors": [],
    }
    assert "fno doctor: codex hooks: warn" in result.stderr


def test_dedicated_mode_skips_normal_doctor_collectors(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    _write_toml(tmp_path / "config.toml")
    monkeypatch.setattr(
        doctor,
        "_resolve_source",
        lambda _source: (_ for _ in ()).throw(AssertionError("collector ran")),
    )

    result = runner.invoke(app, ["doctor", "--codex-hooks"])

    assert result.exit_code == 0
    assert result.exception is None


def test_owned_json_gets_narrow_migration_action(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    _write_toml(config)
    _write_json(tmp_path / "hooks.json", f"env FNO_PLATFORM=codex {HOOK}")

    result = runner.invoke(app, ["doctor", "--codex-hooks"])

    assert result.exit_code == 0
    assert "`fno setup cli-hooks-codex --migrate-legacy-hooks-json`" in result.stdout
    assert "remove only footnote-owned legacy JSON hooks" in result.stdout


def test_codex_hooks_rejects_mutating_or_unrelated_modes(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))

    result = runner.invoke(app, ["doctor", "--codex-hooks", "--fix"])

    assert result.exit_code == 2
    assert "--codex-hooks may only be combined with --json" in result.output
