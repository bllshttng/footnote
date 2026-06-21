"""Tests for fno.setup.cli_hooks (Codex/Gemini SessionStart hook install).

Covers: fresh install, idempotent re-run, never-clobber of existing
hooks/settings, backup-before-write, Codex needs_trust, and the CLI surface.
"""
from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.setup.cli_hooks import install_codex_hook, install_gemini_hook

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
