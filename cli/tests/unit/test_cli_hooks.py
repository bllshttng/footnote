"""Tests for fno.setup.cli_hooks (Codex/Gemini SessionStart hook install).

Covers: fresh install, idempotent re-run, never-clobber of existing
hooks/settings, backup-before-write, Codex needs_trust, and the CLI surface.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.setup.cli_hooks import (
    inspect_codex_hooks,
    install_codex_hook,
    install_gemini_hook,
)

CMD = "/opt/footnote/hooks/session-start.sh"


# --- Gemini -----------------------------------------------------------------


def test_gemini_fresh_install(tmp_path):
    settings = tmp_path / "settings.json"
    res = install_gemini_hook(CMD, settings_path=settings)
    assert res.changed and not res.already_present
    data = json.loads(settings.read_text())
    group = data["hooks"]["SessionStart"][0]
    # No matcher -> fires on all SessionStart sources (startup/resume/clear).
    assert "matcher" not in group
    hook = group["hooks"][0]
    assert hook["name"] == "fno-session-start"
    # Command is wrapped with FNO_PLATFORM so the wrapper detects the platform.
    assert hook["command"] == f"env FNO_PLATFORM=gemini {CMD}"


def test_gemini_idempotent(tmp_path):
    settings = tmp_path / "settings.json"
    install_gemini_hook(CMD, settings_path=settings)
    before = settings.read_text()
    res = install_gemini_hook(CMD, settings_path=settings)
    assert res.already_present and not res.changed
    assert settings.read_text() == before  # untouched on the second run


def test_gemini_preserves_existing_settings_and_hooks(tmp_path):
    settings = tmp_path / "settings.json"
    settings.write_text(
        json.dumps(
            {
                "theme": "dark",
                "hooks": {
                    "AfterTool": [{"matcher": "x", "hooks": [{"type": "command", "command": "user.sh"}]}],
                    "SessionStart": [{"matcher": "startup", "hooks": [{"type": "command", "command": "their-init.sh"}]}],
                },
            }
        )
    )
    res = install_gemini_hook(CMD, settings_path=settings)
    assert res.changed and res.backup is not None and res.backup.exists()
    data = json.loads(settings.read_text())
    # User's unrelated settings + hooks survive.
    assert data["theme"] == "dark"
    assert data["hooks"]["AfterTool"][0]["hooks"][0]["command"] == "user.sh"
    ss = data["hooks"]["SessionStart"]
    cmds = [h["command"] for g in ss for h in g["hooks"]]
    # Both present, none clobbered (footnote's is wrapped with FNO_PLATFORM).
    assert "their-init.sh" in cmds and any(CMD in c for c in cmds)


def test_gemini_malformed_left_unchanged(tmp_path):
    settings = tmp_path / "settings.json"
    settings.write_text("{not json")
    res = install_gemini_hook(CMD, settings_path=settings)
    assert not res.changed and res.note and "malformed" in res.note
    assert settings.read_text() == "{not json"


# --- Codex ------------------------------------------------------------------


def test_codex_fresh_install(tmp_path):
    config = tmp_path / "config.toml"
    res = install_codex_hook(CMD, config_path=config)
    assert res.changed and res.needs_trust and not res.already_present
    text = config.read_text()
    assert "[[hooks.SessionStart]]" in text
    assert CMD in text
    # Parses as valid TOML with the hook reachable.
    import tomllib

    parsed = tomllib.loads(text)
    cmds = [
        h["command"]
        for g in parsed["hooks"]["SessionStart"]
        for h in g["hooks"]
    ]
    # Command is wrapped with FNO_PLATFORM=codex so the wrapper detects codex
    # even though Codex does not set CODEX_PLUGIN_ROOT for user-config hooks.
    assert any(CMD in c for c in cmds)
    assert any("FNO_PLATFORM=codex" in c for c in cmds)


def test_codex_idempotent(tmp_path):
    config = tmp_path / "config.toml"
    install_codex_hook(CMD, config_path=config)
    before = config.read_text()
    res = install_codex_hook(CMD, config_path=config)
    assert res.already_present and not res.changed and res.needs_trust
    assert config.read_text() == before


def test_codex_preserves_existing_config_and_comments(tmp_path):
    config = tmp_path / "config.toml"
    original = (
        "# my codex config\n"
        'model = "gpt-5"\n\n'
        "[[hooks.Stop]]\n\n"
        "[[hooks.Stop.hooks]]\n"
        'type = "command"\n'
        'command = "my-stop.sh"\n'
    )
    config.write_text(original)
    res = install_codex_hook(CMD, config_path=config)
    assert res.changed and res.backup is not None and res.backup.exists()
    text = config.read_text()
    # Original content + comment survive verbatim.
    assert "# my codex config" in text
    assert 'model = "gpt-5"' in text
    assert "my-stop.sh" in text
    # New SessionStart hook added.
    import tomllib

    parsed = tomllib.loads(text)
    assert parsed["hooks"]["Stop"][0]["hooks"][0]["command"] == "my-stop.sh"
    ss_cmds = [h["command"] for g in parsed["hooks"]["SessionStart"] for h in g["hooks"]]
    assert any(CMD in c for c in ss_cmds)


def test_codex_backup_only_when_file_exists(tmp_path):
    config = tmp_path / "config.toml"
    res = install_codex_hook(CMD, config_path=config)
    assert res.backup is None  # nothing to back up on a fresh file


def _write_codex_toml(path, command):
    path.write_text(
        "[[hooks.SessionStart]]\n\n"
        "[[hooks.SessionStart.hooks]]\n"
        'type = "command"\n'
        f"command = {json.dumps(command)}\n"
    )


def _write_codex_json(path, *commands):
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


@pytest.mark.parametrize(
    ("toml_command", "json_command", "state"),
    [
        (None, None, "neither"),
        (CMD, None, "toml-only"),
        (None, CMD, "json-only"),
        (CMD, CMD, "both"),
    ],
)
def test_codex_inspector_distinguishes_hook_layers(
    tmp_path, toml_command, json_command, state
):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    if toml_command:
        _write_codex_toml(config, toml_command)
    if json_command:
        _write_codex_json(legacy, json_command)

    diagnostics = inspect_codex_hooks(
        config_path=config, hooks_json_path=legacy
    )

    assert diagnostics.state == state
    assert diagnostics.has_toml_hooks is bool(toml_command)
    assert diagnostics.has_json_hooks is bool(json_command)


def test_codex_inspector_classifies_owned_and_foreign_commands(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    wrapper_only = "env FNO_PLATFORM=codex custom-session-command"
    other_plugin = "/opt/other-plugin/hooks/session-start.sh"
    foreign = "bash '/Users/bb16/.codex/herdr-agent-state.sh' session"
    _write_codex_toml(config, CMD)
    _write_codex_json(legacy, wrapper_only, other_plugin, foreign)

    diagnostics = inspect_codex_hooks(
        config_path=config, hooks_json_path=legacy
    )

    assert diagnostics.toml_footnote_commands == (CMD,)
    assert diagnostics.json_footnote_commands == ()
    assert diagnostics.json_foreign_commands == (
        wrapper_only,
        other_plugin,
        foreign,
    )


def test_codex_migration_preserves_other_plugins_session_start(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    owned = f"env FNO_PLATFORM=codex {CMD}"
    other_plugin = "/opt/other-plugin/hooks/session-start.sh"
    _write_codex_toml(config, owned)
    _write_codex_json(legacy, owned, other_plugin)

    result = install_codex_hook(
        owned,
        config_path=config,
        hooks_json_path=legacy,
        migrate_legacy_hooks_json=True,
    )

    remaining = json.loads(legacy.read_text())
    commands = [
        hook["command"]
        for group in remaining["hooks"]["SessionStart"]
        for hook in group["hooks"]
    ]
    assert commands == [other_plugin]
    assert result.note and "manual consolidation" in result.note


def test_codex_inspector_reports_footnote_trust_key(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    state_key = f"{config.absolute()}:session_start:0:0"
    config.write_text(
        "[[hooks.SessionStart]]\n\n"
        "[[hooks.SessionStart.hooks]]\n"
        'type = "command"\n'
        f"command = {json.dumps(CMD)}\n\n"
        f"[hooks.state.{json.dumps(state_key)}]\n"
        'trusted_hash = "sha256:test"\n'
    )

    diagnostics = inspect_codex_hooks(config_path=config, hooks_json_path=legacy)

    assert diagnostics.toml_footnote_state_keys == (state_key,)
    assert diagnostics.toml_footnote_state_recorded == (True,)
    assert not diagnostics.all_toml_footnote_hooks_verified


def test_codex_duplicate_layers_warn_without_mutating_json(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    _write_codex_toml(config, CMD)
    _write_codex_json(legacy, CMD)
    before = legacy.read_bytes()

    res = install_codex_hook(
        CMD, config_path=config, hooks_json_path=legacy
    )

    assert not res.changed and res.already_present
    assert res.note and "both Codex hook layers" in res.note
    assert str(config) in res.note and str(legacy) in res.note
    assert "TOML is preferred" in res.note
    assert legacy.read_bytes() == before
    assert not legacy.with_name("hooks.json.fno-bak").exists()


def test_codex_explicit_migration_backs_up_and_removes_owned_json(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    _write_codex_toml(config, CMD)
    _write_codex_json(legacy, CMD)
    before = legacy.read_bytes()

    res = install_codex_hook(
        CMD,
        config_path=config,
        hooks_json_path=legacy,
        migrate_legacy_hooks_json=True,
    )

    assert res.changed and res.legacy_backup is not None
    assert res.legacy_backup.read_bytes() == before
    assert not legacy.exists()
    assert res.note and "migrated footnote-owned" in res.note


def test_codex_migration_removes_owned_json_with_description_metadata(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    _write_codex_toml(config, CMD)
    original = (
        json.dumps(
            {
                "description": "Legacy footnote Codex hooks",
                "hooks": {
                    "SessionStart": [
                        {"hooks": [{"type": "command", "command": CMD}]}
                    ]
                },
            },
            indent=2,
        )
        + "\n"
    ).encode()
    legacy.write_bytes(original)

    res = install_codex_hook(
        CMD,
        config_path=config,
        hooks_json_path=legacy,
        migrate_legacy_hooks_json=True,
    )

    assert not legacy.exists()
    assert res.legacy_backup is not None
    assert res.legacy_backup.read_bytes() == original


def test_codex_migration_preserves_foreign_events_and_exact_backup(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    _write_codex_toml(config, CMD)
    original = (
        json.dumps(
            {
                "description": "Mixed legacy hooks",
                "hooks": {
                    "SessionStart": [
                        {"hooks": [{"type": "command", "command": CMD}]}
                    ],
                    "Stop": [
                        {
                            "hooks": [
                                {"type": "command", "command": "foreign-stop.sh"}
                            ]
                        }
                    ],
                },
            },
            indent=4,
        )
        + "\n"
    ).encode()
    legacy.write_bytes(original)

    res = install_codex_hook(
        CMD,
        config_path=config,
        hooks_json_path=legacy,
        migrate_legacy_hooks_json=True,
    )

    remaining = json.loads(legacy.read_text())
    assert "SessionStart" not in remaining["hooks"]
    assert remaining["hooks"]["Stop"][0]["hooks"][0]["command"] == "foreign-stop.sh"
    assert remaining["description"] == "Mixed legacy hooks"
    assert res.legacy_backup is not None
    assert res.legacy_backup.read_bytes() == original
    assert res.note and "manual consolidation" in res.note
    assert "foreign hooks were preserved" in res.note
    assert "foreign SessionStart hooks" not in res.note

    diagnostics = inspect_codex_hooks(config_path=config, hooks_json_path=legacy)
    assert diagnostics.state == "both"
    assert diagnostics.has_json_hooks
    assert diagnostics.json_foreign_commands == ("foreign-stop.sh",)


def test_codex_migration_preserves_foreign_json_and_requests_manual_consolidation(
    tmp_path,
):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    foreign = "bash '/Users/bb16/.codex/herdr-agent-state.sh' session"
    _write_codex_toml(config, CMD)
    _write_codex_json(legacy, CMD, foreign)

    res = install_codex_hook(
        CMD,
        config_path=config,
        hooks_json_path=legacy,
        migrate_legacy_hooks_json=True,
    )

    remaining = json.loads(legacy.read_text())
    commands = [
        hook["command"]
        for group in remaining["hooks"]["SessionStart"]
        for hook in group["hooks"]
    ]
    assert commands == [foreign]
    assert res.legacy_backup is not None
    assert res.note and "manual consolidation" in res.note
    assert "TOML is preferred" in res.note


@pytest.mark.parametrize("malformed", ["toml", "json"])
def test_codex_inspector_returns_malformed_diagnostics(tmp_path, malformed):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    if malformed == "toml":
        config.write_text("[[not valid")
    else:
        legacy.write_text("{not json")

    diagnostics = inspect_codex_hooks(
        config_path=config, hooks_json_path=legacy
    )

    assert diagnostics.state == "malformed"
    assert len(diagnostics.errors) == 1
    assert str(config if malformed == "toml" else legacy) in diagnostics.errors[0]


@pytest.mark.parametrize("root", ["[]", "null", '"string"'])
def test_codex_inspector_rejects_non_object_json_roots(tmp_path, root):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    legacy.write_text(root)

    diagnostics = inspect_codex_hooks(config_path=config, hooks_json_path=legacy)

    assert diagnostics.state == "malformed"
    assert diagnostics.errors
    assert "expected a JSON object" in diagnostics.errors[0]


def test_codex_migration_does_not_mutate_non_object_json(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    legacy.write_text("[]")

    result = install_codex_hook(
        CMD,
        config_path=config,
        hooks_json_path=legacy,
        migrate_legacy_hooks_json=True,
    )

    assert result.error
    assert not result.changed
    assert legacy.read_text() == "[]"
    assert not legacy.with_name("hooks.json.fno-bak").exists()


def test_codex_malformed_toml_is_not_modified(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    config.write_text("[[not valid")

    res = install_codex_hook(CMD, config_path=config, hooks_json_path=legacy)

    assert not res.changed
    assert res.error
    assert not res.needs_trust
    assert res.note and "malformed" in res.note
    assert config.read_text() == "[[not valid"


def test_cli_codex_malformed_config_exits_nonzero_without_success_output(
    tmp_path, monkeypatch
):
    import fno.paths as paths
    from fno.setup_cli import app

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)
    config = tmp_path / "config.toml"
    config.write_text("[[not valid")

    result = CliRunner().invoke(
        app,
        ["cli-hooks-codex", "--codex-config", str(config)],
    )

    assert result.exit_code == 1
    assert "malformed" in result.stderr
    assert "UNTRUSTED" not in result.output
    assert "Nothing to do" not in result.output


def test_codex_malformed_legacy_json_returns_failure_without_writing_toml(tmp_path):
    config = tmp_path / "config.toml"
    legacy = tmp_path / "hooks.json"
    legacy.write_text("{not json")

    result = install_codex_hook(
        CMD,
        config_path=config,
        hooks_json_path=legacy,
        migrate_legacy_hooks_json=True,
    )

    assert result.error and "malformed" in result.error
    assert not result.changed
    assert not result.needs_trust
    assert not config.exists()
    assert legacy.read_text() == "{not json"


# --- CLI surface ------------------------------------------------------------


def test_cli_cli_hooks_writes_both(tmp_path, monkeypatch):
    # Point the plugin-script resolver at a fake hook path.
    import fno.paths as paths

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)

    from fno.setup_cli import app

    gset = tmp_path / "g" / "settings.json"
    cconf = tmp_path / "c" / "config.toml"
    res = CliRunner().invoke(
        app,
        ["cli-hooks", "--gemini-settings", str(gset), "--codex-config", str(cconf)],
    )
    assert res.exit_code == 0, res.output
    assert gset.exists() and cconf.exists()
    assert "UNTRUSTED" in res.output  # codex trust instruction surfaced
    assert str(fake_entry) in json.loads(gset.read_text())["hooks"]["SessionStart"][0]["hooks"][0]["command"]


def test_install_cli_hooks_core_returns_after_success(tmp_path, monkeypatch):
    import fno.paths as paths
    from fno.setup_cli import _install_cli_hooks

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)

    result = _install_cli_hooks(
        codex=True,
        gemini=True,
        gemini_settings=tmp_path / "gemini" / "settings.json",
        codex_config=tmp_path / "codex" / "config.toml",
        codex_hooks_json=None,
        migrate_legacy_hooks_json=False,
    )

    assert result is None


def test_cli_cli_hooks_refuses_missing_plugin_hook(tmp_path, monkeypatch):
    import fno.paths as paths
    from fno.setup_cli import app

    missing_entry = tmp_path / "missing-plugin" / "hooks" / "session-start.sh"
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: missing_entry)
    gemini_settings = tmp_path / "gemini" / "settings.json"
    codex_config = tmp_path / "codex" / "config.toml"

    result = CliRunner().invoke(
        app,
        [
            "cli-hooks",
            "--gemini-settings",
            str(gemini_settings),
            "--codex-config",
            str(codex_config),
        ],
    )

    assert result.exit_code == 1
    assert "could not locate the installed footnote hooks dir" in result.output
    assert not gemini_settings.exists()
    assert not codex_config.exists()


def test_cli_cli_hooks_exits_nonzero_when_gemini_refuses(tmp_path, monkeypatch):
    import fno.paths as paths
    from fno.setup_cli import app

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)

    gemini_settings = tmp_path / "gemini" / "settings.json"
    gemini_settings.parent.mkdir(parents=True)
    gemini_settings.write_text("{malformed")
    result = CliRunner().invoke(
        app,
        [
            "cli-hooks",
            "--gemini-settings",
            str(gemini_settings),
            "--codex-config",
            str(tmp_path / "codex" / "config.toml"),
        ],
    )

    assert result.exit_code == 1
    assert "gemini: error:" in result.output
    assert "left unchanged" in result.output
    assert "UNTRUSTED" in result.output


def test_cli_cli_hooks_no_gemini_writes_only_codex(tmp_path, monkeypatch):
    import fno.paths as paths

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)

    from fno.setup_cli import app

    gset = tmp_path / "g" / "settings.json"
    cconf = tmp_path / "c" / "config.toml"
    res = CliRunner().invoke(
        app,
        [
            "cli-hooks",
            "--no-gemini",
            "--gemini-settings",
            str(gset),
            "--codex-config",
            str(cconf),
        ],
    )
    assert res.exit_code == 0, res.output
    assert cconf.exists()
    assert not gset.exists()
    assert "codex: wired SessionStart" in res.output
    assert "gemini:" not in res.output


def test_cli_cli_hooks_codex_alias_writes_only_codex(tmp_path, monkeypatch):
    import tomllib

    import fno.paths as paths

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)

    from fno.setup_cli import app

    cconf = tmp_path / "c" / "config.toml"
    res = CliRunner().invoke(
        app,
        ["cli-hooks-codex", "--codex-config", str(cconf)],
    )
    assert res.exit_code == 0, res.output
    parsed = tomllib.loads(cconf.read_text())
    cmds = [
        h["command"]
        for g in parsed["hooks"]["SessionStart"]
        for h in g["hooks"]
    ]
    assert cmds == [f"env FNO_PLATFORM=codex {fake_entry}"]
    assert "gemini:" not in res.output


def test_cli_cli_hooks_codex_wires_legacy_path_and_migration_flag(tmp_path, monkeypatch):
    import fno.paths as paths

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)

    from fno.setup_cli import app

    cconf = tmp_path / "codex" / "config.toml"
    legacy = tmp_path / "legacy" / "hooks.json"
    legacy.parent.mkdir()
    _write_codex_json(legacy, f"env FNO_PLATFORM=codex {fake_entry}")
    res = CliRunner().invoke(
        app,
        [
            "cli-hooks-codex",
            "--codex-config",
            str(cconf),
            "--codex-hooks-json",
            str(legacy),
            "--migrate-legacy-hooks-json",
        ],
    )

    assert res.exit_code == 0, res.output
    assert cconf.exists() and not legacy.exists()
    assert legacy.with_name("hooks.json.fno-bak").exists()
    assert "migrated footnote-owned" in res.output


def test_cli_cli_hooks_codex_defaults_hooks_json_next_to_codex_config(
    tmp_path, monkeypatch
):
    import fno.paths as paths

    fake_entry = tmp_path / "plugin" / "hooks" / "session-start.sh"
    fake_entry.parent.mkdir(parents=True)
    fake_entry.write_text("#!/usr/bin/env bash\n")
    monkeypatch.setattr(paths, "resolve_plugin_script", lambda rel: fake_entry)

    from fno.setup_cli import app

    codex_home = tmp_path / "codex-home"
    codex_home.mkdir()
    foreign = "bash '/Users/bb16/.codex/herdr-agent-state.sh' session"
    _write_codex_json(codex_home / "hooks.json", foreign)
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    res = CliRunner().invoke(app, ["cli-hooks-codex"])

    assert res.exit_code == 0, res.output
    assert str(codex_home / "hooks.json") in res.output
    assert str(codex_home / "config.toml") in res.output
    assert "manual consolidation" in res.output
    assert foreign in (codex_home / "hooks.json").read_text()
