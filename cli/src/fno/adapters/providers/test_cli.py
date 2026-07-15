"""Tests for fno providers CLI surface (Phase 02).

Run: cd cli && uv run pytest src/fno/adapters/providers/test_cli.py -v
"""
from __future__ import annotations

import os
import stat
from pathlib import Path
from typing import Any

import pytest
import tomli_w
import tomllib
from typer.testing import CliRunner

from fno.adapters.providers.cli import cli as providers_app


runner = CliRunner()


# Wider help width for chip subcommands so options don't column-wrap
# (caught on PR #224: typer/rich line-wraps help in narrow CI envs and
# options vanish from captured stdout). Mirrors the project pattern.
_WIDE_HELP_ENV = {
    "COLUMNS": "240",
    "NO_COLOR": "1",
    "TERM": "dumb",
}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _flatten(d: dict) -> dict:
    cfg = d.get("config")
    if not isinstance(cfg, dict):
        return d
    out = {k: v for k, v in d.items() if k != "config"}
    out.update(cfg)
    return out


def _strip_none(x):
    if isinstance(x, dict):
        return {k: _strip_none(v) for k, v in x.items() if v is not None}
    if isinstance(x, list):
        return [_strip_none(v) for v in x]
    return x


def _write_settings(path: Path, content: dict) -> None:
    """Write a flat config.toml at path (lifts any legacy config: wrapper)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(tomli_w.dumps(_strip_none(_flatten(content))), encoding="utf-8")


def _invoke(args: list[str], cwd: Path, home: Path):
    """Invoke the CLI with environment isolation."""
    return runner.invoke(
        providers_app,
        args,
        env={
            "HOME": str(home),
            "PWD": str(cwd),
        },
        catch_exceptions=False,
    )


def _two_record_config(active: str = "claude-primary") -> dict:
    return {
        "config": {
            "providers": {
                "active": active,
                "records": [
                    {
                        "id": "claude-primary",
                        "name": "Claude Primary",
                        "cli": "claude",
                        "auth": "oauth_dir",
                        "credentials_source": str(Path.home() / ".claude"),
                        "priority": 10,
                    },
                    {
                        "id": "gemini-backup",
                        "name": "Gemini Backup",
                        "cli": "gemini",
                        "auth": "api_key",
                        "env": {"GEMINI_API_KEY": "test-key"},
                        "priority": 20,
                    },
                ],
            }
        }
    }


# ---------------------------------------------------------------------------
# AC02.3-CLI: Empty-state message is helpful
# ---------------------------------------------------------------------------

class TestListEmpty:
    def test_list_empty_state_message(self, tmp_path: Path):
        """AC02.3-CLI: fno providers list on empty config prints helpful message; exit 0."""
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "No providers configured" in result.output

    def test_list_empty_state_exit_zero(self, tmp_path: Path):
        """AC02.3-CLI: exit code is 0 on empty state."""
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0


# ---------------------------------------------------------------------------
# List with records
# ---------------------------------------------------------------------------

class TestListWithRecords:
    def test_list_shows_all_records(self, tmp_path: Path):
        """fno providers list shows both records."""
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config())
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "claude-primary" in result.output
        assert "gemini-backup" in result.output

    def test_list_marks_active_with_asterisk(self, tmp_path: Path):
        """fno providers list marks the active record with *."""
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config(active="claude-primary"))
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        # The active record should have a * marker
        output_lines = result.output.splitlines()
        active_lines = [l for l in output_lines if "claude-primary" in l]
        assert any("*" in line for line in active_lines), (
            f"Expected '*' next to active provider in: {active_lines}"
        )

    def test_list_inactive_has_no_asterisk(self, tmp_path: Path):
        """Non-active record is not marked with *."""
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config(active="claude-primary"))
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        output_lines = result.output.splitlines()
        backup_lines = [l for l in output_lines if "gemini-backup" in l]
        assert backup_lines, "gemini-backup should appear in output"
        assert not any("*" in line for line in backup_lines), (
            f"Expected no '*' next to inactive provider in: {backup_lines}"
        )


# ---------------------------------------------------------------------------
# Show
# ---------------------------------------------------------------------------

class TestShow:
    def test_show_existing_prints_fields(self, tmp_path: Path):
        """fno providers show <id> prints all fields for the record."""
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config())
        result = _invoke(["show", "claude-primary"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "claude-primary" in result.output
        assert "claude" in result.output   # cli value
        assert "oauth_dir" in result.output  # auth value

    def test_show_nonexistent_exits_nonzero(self, tmp_path: Path):
        """fno providers show nonexistent exits 1 with stderr message."""
        result = _invoke(["show", "nonexistent"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0


# ---------------------------------------------------------------------------
# AC02.1-HP + AC02.2-ERR: Add
# ---------------------------------------------------------------------------

class TestAdd:
    def test_add_valid_oauth_record(self, tmp_path: Path):
        """AC02.1-HP: add with valid oauth_dir args creates a record loadable via load_providers."""
        from fno.adapters.providers.loader import load_providers

        creds = tmp_path / ".claude"
        creds.mkdir()

        result = _invoke(
            [
                "add", "claude-secondary",
                "--cli", "claude",
                "--auth", "oauth_dir",
                "--credentials-source", str(creds),
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        assert result.exit_code == 0, f"stdout: {result.output}\nstderr: {result.stderr}"

        # Verify via load_providers targeting tmp_path
        config = load_providers(repo_root=tmp_path)
        assert any(r.id == "claude-secondary" for r in config.records)

    def test_add_round_trip_use_list(self, tmp_path: Path):
        """AC02.1-HP: add + use + list shows active correctly."""
        from fno.adapters.providers.loader import load_providers

        creds = tmp_path / ".claude-secondary"
        creds.mkdir()

        _invoke(
            [
                "add", "claude-max-secondary",
                "--cli", "claude",
                "--auth", "oauth_dir",
                "--credentials-source", str(creds),
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        _invoke(["use", "claude-max-secondary", "--scope", "global"], cwd=tmp_path, home=tmp_path)

        list_result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result_has_active(list_result.output, "claude-max-secondary")

        config = load_providers(repo_root=tmp_path)
        assert config.active == "claude-max-secondary"

    def test_add_missing_credentials_source_for_oauth(self, tmp_path: Path):
        """AC02.2-ERR: add with oauth_dir but no --credentials-source exits non-zero with auth_strategy_mismatch."""
        result = _invoke(
            ["add", "bad-provider", "--cli", "claude", "--auth", "oauth_dir"],
            cwd=tmp_path,
            home=tmp_path,
        )
        assert result.exit_code != 0
        err_text = result.stderr + result.output
        assert "auth_strategy_mismatch" in err_text
        assert "bad-provider" in err_text

    def test_add_duplicate_refuses_without_force(self, tmp_path: Path):
        """add with existing id refuses without --force."""
        creds = tmp_path / ".claude"
        creds.mkdir()
        args = [
            "add", "my-provider",
            "--cli", "claude",
            "--auth", "oauth_dir",
            "--credentials-source", str(creds),
            "--scope", "global",
        ]
        _invoke(args, cwd=tmp_path, home=tmp_path)
        result = _invoke(args, cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0

    def test_add_duplicate_with_force_succeeds(self, tmp_path: Path):
        """add with existing id and --force overwrites successfully."""
        creds = tmp_path / ".claude"
        creds.mkdir()
        args = [
            "add", "my-provider",
            "--cli", "claude",
            "--auth", "oauth_dir",
            "--credentials-source", str(creds),
            "--scope", "global",
        ]
        _invoke(args, cwd=tmp_path, home=tmp_path)
        result = _invoke(args + ["--force"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0

    def test_add_invalid_env_pair_exits_nonzero(self, tmp_path: Path):
        """add with malformed --env entry (no =) exits non-zero."""
        result = _invoke(
            [
                "add", "api-provider",
                "--cli", "claude",
                "--auth", "api_key",
                "--env", "BADKEY",  # missing =VALUE
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        assert result.exit_code != 0


# ---------------------------------------------------------------------------
# AC02.5-FR: Atomic write failure
# ---------------------------------------------------------------------------

class TestAtomicWrite:
    def test_add_readonly_settings_exits_nonzero(self, tmp_path: Path):
        """AC02.5-FR: add with read-only settings.yaml exits non-zero; file unchanged."""
        # Create a pre-existing settings.yaml and make the .fno dir read-only
        abilities_dir = tmp_path / ".fno"
        abilities_dir.mkdir(parents=True)
        settings = abilities_dir / "config.toml"
        settings.write_text("v2_enabled = false\n", encoding="utf-8")
        original_content = settings.read_text(encoding="utf-8")

        # Make the parent directory read-only so atomic_write can't create a temp file
        abilities_dir.chmod(stat.S_IRUSR | stat.S_IXUSR)  # r-x: can list, can't write
        try:
            creds = tmp_path / ".claude"
            creds.mkdir()
            result = _invoke(
                [
                    "add", "fail-provider",
                    "--cli", "claude",
                    "--auth", "oauth_dir",
                    "--credentials-source", str(creds),
                    "--scope", "global",
                ],
                cwd=tmp_path,
                home=tmp_path,
            )
            assert result.exit_code != 0
            # Content must be unchanged
            abilities_dir.chmod(stat.S_IRWXU)  # restore to read
            assert settings.read_text(encoding="utf-8") == original_content
        finally:
            # Always restore permissions so pytest can clean up tmp_path
            abilities_dir.chmod(stat.S_IRWXU)


# ---------------------------------------------------------------------------
# Test command
# ---------------------------------------------------------------------------

class TestTestCommand:
    def test_test_valid_record_returns_zero(self, tmp_path: Path):
        """fno providers test <id> returns 0 when binary on PATH + credentials_source exists."""
        import shutil
        # Only run if 'claude' binary is actually available, otherwise skip
        if not shutil.which("claude"):
            pytest.skip("claude binary not on PATH in this environment")

        creds = tmp_path / ".claude"
        creds.mkdir()
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, {
            "config": {
                "providers": {
                    "active": None,
                    "records": [
                        {
                            "id": "claude-test",
                            "name": "Claude Test",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            "credentials_source": str(creds),
                            "priority": 10,
                        }
                    ],
                }
            }
        })
        result = _invoke(["test", "claude-test"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0

    def test_test_nonexistent_cli_binary_exits_nonzero(self, tmp_path: Path):
        """fno providers test exits non-zero when CLI binary is not on PATH."""
        creds = tmp_path / ".claude"
        creds.mkdir()
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, {
            "config": {
                "providers": {
                    "active": None,
                    "records": [
                        {
                            "id": "hermes-test",
                            "name": "Hermes Test",
                            "cli": "hermes",
                            "auth": "oauth_dir",
                            "credentials_source": str(creds),
                            "priority": 10,
                        }
                    ],
                }
            }
        })
        result = _invoke(["test", "hermes-test"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0

    def test_test_nonexistent_id_exits_nonzero(self, tmp_path: Path):
        """fno providers test nonexistent exits non-zero."""
        result = _invoke(["test", "no-such-provider"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0

    def test_test_missing_credentials_source_exits_nonzero(self, tmp_path: Path):
        """fno providers test exits non-zero when credentials_source path doesn't exist."""
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, {
            "config": {
                "providers": {
                    "active": None,
                    "records": [
                        {
                            "id": "claude-missing-creds",
                            "name": "Claude Missing Creds",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            "credentials_source": str(tmp_path / "nonexistent-creds"),
                            "priority": 10,
                        }
                    ],
                }
            }
        })
        result = _invoke(["test", "claude-missing-creds"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0


# ---------------------------------------------------------------------------
# Use command
# ---------------------------------------------------------------------------

class TestUse:
    def test_use_updates_active(self, tmp_path: Path):
        """AC02.1-HP: fno providers use <id> updates config.providers.active."""
        from fno.adapters.providers.loader import load_providers

        creds = tmp_path / ".claude"
        creds.mkdir()
        _invoke(
            [
                "add", "my-provider",
                "--cli", "claude",
                "--auth", "oauth_dir",
                "--credentials-source", str(creds),
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        result = _invoke(["use", "my-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0

        config = load_providers(repo_root=tmp_path)
        assert config.active == "my-provider"

    def test_use_nonexistent_exits_nonzero(self, tmp_path: Path):
        """fno providers use nonexistent exits non-zero."""
        result = _invoke(["use", "nonexistent"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0


# ---------------------------------------------------------------------------
# Remove command
# ---------------------------------------------------------------------------

class TestRemove:
    def test_remove_nonactive_succeeds(self, tmp_path: Path):
        """fno providers remove <id> succeeds for non-active records."""
        from fno.adapters.providers.loader import load_providers

        creds = tmp_path / ".claude"
        creds.mkdir()
        # Add two records, set first as active
        for pid in ["provider-a", "provider-b"]:
            _invoke(
                [
                    "add", pid,
                    "--cli", "claude",
                    "--auth", "oauth_dir",
                    "--credentials-source", str(creds),
                    "--scope", "global",
                ],
                cwd=tmp_path,
                home=tmp_path,
            )
        _invoke(["use", "provider-a", "--scope", "global"], cwd=tmp_path, home=tmp_path)

        result = _invoke(["remove", "provider-b", "--scope", "global"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0

        config = load_providers(repo_root=tmp_path)
        assert not any(r.id == "provider-b" for r in config.records)

    def test_remove_active_without_force_exits_nonzero(self, tmp_path: Path):
        """AC02.4-EDGE: remove active record without --force exits non-zero."""
        creds = tmp_path / ".claude"
        creds.mkdir()
        _invoke(
            [
                "add", "active-provider",
                "--cli", "claude",
                "--auth", "oauth_dir",
                "--credentials-source", str(creds),
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        _invoke(["use", "active-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)

        result = _invoke(["remove", "active-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0

    def test_remove_active_stderr_mentions_force(self, tmp_path: Path):
        """AC02.4-EDGE: stderr explains --force requirement when removing active."""
        creds = tmp_path / ".claude"
        creds.mkdir()
        _invoke(
            [
                "add", "active-provider",
                "--cli", "claude",
                "--auth", "oauth_dir",
                "--credentials-source", str(creds),
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        _invoke(["use", "active-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)

        result = _invoke(["remove", "active-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)
        err_text = result.stderr + result.output
        assert "--force" in err_text

    def test_remove_active_with_force_succeeds(self, tmp_path: Path):
        """fno providers remove --force removes even the active record."""
        from fno.adapters.providers.loader import load_providers

        creds = tmp_path / ".claude"
        creds.mkdir()
        _invoke(
            [
                "add", "active-provider",
                "--cli", "claude",
                "--auth", "oauth_dir",
                "--credentials-source", str(creds),
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        _invoke(["use", "active-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)

        result = _invoke(
            ["remove", "active-provider", "--force", "--scope", "global"],
            cwd=tmp_path,
            home=tmp_path,
        )
        assert result.exit_code == 0

        config = load_providers(repo_root=tmp_path)
        assert not any(r.id == "active-provider" for r in config.records)

    def test_remove_record_remains_on_failure(self, tmp_path: Path):
        """AC02.4-EDGE: record remains in settings.yaml when remove fails."""
        from fno.adapters.providers.loader import load_providers

        creds = tmp_path / ".claude"
        creds.mkdir()
        _invoke(
            [
                "add", "active-provider",
                "--cli", "claude",
                "--auth", "oauth_dir",
                "--credentials-source", str(creds),
                "--scope", "global",
            ],
            cwd=tmp_path,
            home=tmp_path,
        )
        _invoke(["use", "active-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)
        _invoke(["remove", "active-provider", "--scope", "global"], cwd=tmp_path, home=tmp_path)  # no --force

        config = load_providers(repo_root=tmp_path)
        assert any(r.id == "active-provider" for r in config.records)


# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------

def result_has_active(output: str, provider_id: str) -> bool:
    """Return True if any output line contains both provider_id and '*'."""
    for line in output.splitlines():
        if provider_id in line and "*" in line:
            return True
    return False


# ---------------------------------------------------------------------------
# Smoke test passes dispatch_env vars (Gemini Code Assist MEDIUM PR #199)
# ---------------------------------------------------------------------------

class TestSmokeDispatchEnv:
    """fno providers test --smoke must pass dispatch_env() env vars to the subprocess."""

    def test_smoke_passes_dispatch_env_vars_for_api_key_provider(
        self, tmp_path: Path, monkeypatch
    ):
        """fno providers test --smoke must inject dispatch_env() vars (e.g. GEMINI_API_KEY)
        into the subprocess env, not just inherit the parent process env."""
        import subprocess as subprocess_module

        # Stage a fake api_key provider (api_key needs no filesystem staging).
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(
            settings_path,
            {
                "config": {
                    "providers": {
                        "active": None,
                        "records": [
                            {
                                "id": "gemini-smoke-test",
                                "name": "Gemini Smoke Test",
                                "cli": "gemini",
                                "auth": "api_key",
                                "env": {"GEMINI_API_KEY": "test-smoke-key"},
                            }
                        ],
                    }
                }
            },
        )

        # Ensure `gemini` binary resolves on PATH by pointing PATH at a fake bin dir.
        fake_bin = tmp_path / "bin"
        fake_bin.mkdir()
        fake_gemini = fake_bin / "gemini"
        fake_gemini.write_text("#!/bin/sh\nexit 0\n")
        fake_gemini.chmod(0o755)
        monkeypatch.setenv("PATH", str(fake_bin))

        # Capture (cmd, env) for each subprocess.run. The smoke arm makes
        # internal `git` calls (repo-root resolution) before the CLI invocation,
        # so key off the actual `gemini --help` run rather than the first call.
        captured: list[tuple[list, dict]] = []

        def capturing_run(cmd, **kwargs):
            captured.append((list(cmd), dict(kwargs.get("env") or {})))
            import subprocess
            return subprocess.CompletedProcess(cmd, returncode=0, stdout="", stderr="")

        # Patch subprocess.run at the module level; cli.py's smoke arm uses
        # the module-level subprocess import, so this intercepts the call.
        monkeypatch.setattr(subprocess_module, "run", capturing_run)

        result = _invoke(
            ["test", "gemini-smoke-test", "--smoke"],
            cwd=tmp_path,
            home=tmp_path,
        )

        assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
        smoke_envs = [env for cmd, env in captured if cmd and cmd[0] == "gemini"]
        assert smoke_envs, "the smoke `gemini` invocation was not run"
        env_used = smoke_envs[0]
        assert "GEMINI_API_KEY" in env_used, (
            f"dispatch_env GEMINI_API_KEY must be injected into smoke subprocess env. "
            f"Got keys: {sorted(env_used.keys())}"
        )
        assert env_used["GEMINI_API_KEY"] == "test-smoke-key"


# ---------------------------------------------------------------------------
# CG4: fno providers combos {add, list, remove, test, use}
# Plan B (Spec 4, ab-0e5a921e). AC4.1-4.5.
# ---------------------------------------------------------------------------

@pytest.fixture
def combos_cli_env(tmp_path: Path, monkeypatch):
    """Pre-seed two providers + isolate runtime_state so combos cli tests are independent."""
    settings = tmp_path / ".fno" / "config.toml"
    _write_settings(settings, _two_record_config())
    monkeypatch.setenv(
        "FNO_RUNTIME_STATE_PATH",
        str(tmp_path / "runtime-state.json"),
    )
    return tmp_path


class TestCombosAdd:
    def test_add_writes_combo_to_settings(self, combos_cli_env: Path):
        """AC4.1-HP: add writes a fresh combo block."""
        result = _invoke(
            [
                "combos", "add", "my-stack",
                "--strategy", "round_robin",
                "--sticky", "3",
                "--providers", "claude-primary,gemini-backup",
                "--scope", "project",
            ],
            cwd=combos_cli_env,
            home=combos_cli_env,
        )
        assert result.exit_code == 0, result.output
        data = tomllib.loads(
            (combos_cli_env / ".fno" / "config.toml").read_text()
        )
        combos = data["providers"]["combos"]
        assert "my-stack" in combos
        assert combos["my-stack"]["strategy"] == "round_robin"
        assert combos["my-stack"]["sticky_limit"] == 3
        assert combos["my-stack"]["providers"] == ["claude-primary", "gemini-backup"]

    def test_add_unknown_provider_id_fails_without_mutation(
        self, combos_cli_env: Path
    ):
        """AC4.2-ERR: unknown provider id rejected; settings.yaml unchanged."""
        before = (combos_cli_env / ".fno" / "config.toml").read_text()
        result = _invoke(
            [
                "combos", "add", "bad",
                "--providers", "claude-primary,does-not-exist,gemini-backup",
            ],
            cwd=combos_cli_env,
            home=combos_cli_env,
        )
        assert result.exit_code != 0
        assert "does-not-exist" in result.output
        after = (combos_cli_env / ".fno" / "config.toml").read_text()
        assert before == after

    def test_add_duplicate_name_fails(self, combos_cli_env: Path):
        """Duplicate combo name is rejected (must remove first)."""
        ok = _invoke(
            ["combos", "add", "dup", "--providers", "claude-primary"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert ok.exit_code == 0, ok.output
        dup = _invoke(
            ["combos", "add", "dup", "--providers", "gemini-backup"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert dup.exit_code != 0
        assert "already exists" in dup.output


class TestCombosList:
    def test_list_empty_when_no_combos(self, combos_cli_env: Path):
        result = _invoke(
            ["combos", "list"], cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0
        assert "No combos configured" in result.output

    def test_list_after_add_shows_combo(self, combos_cli_env: Path):
        """AC4.3-UI: list shows name, strategy, sticky, members."""
        _invoke(
            [
                "combos", "add", "my-stack",
                "--strategy", "round_robin",
                "--sticky", "2",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        result = _invoke(
            ["combos", "list"], cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0
        assert "my-stack" in result.output
        assert "round_robin" in result.output
        assert "claude-primary" in result.output

    def test_list_json_returns_structured_data(self, combos_cli_env: Path):
        """--json returns the same data as JSON."""
        import json as json_mod
        _invoke(
            [
                "combos", "add", "my-stack",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        result = _invoke(
            ["combos", "list", "--json"], cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0
        rows = json_mod.loads(result.output)
        assert len(rows) == 1
        assert rows[0]["name"] == "my-stack"
        assert rows[0]["members"] == ["claude-primary", "gemini-backup"]


class TestCombosRemove:
    def test_remove_clears_active_combo_and_warns(self, combos_cli_env: Path):
        """AC4.4-EDGE: remove clears active_combo when matched."""
        _invoke(
            ["combos", "add", "my-stack", "--providers", "claude-primary"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        _invoke(
            ["combos", "use", "my-stack"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        result = _invoke(
            ["combos", "remove", "my-stack"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0
        assert "active_combo cleared" in result.output
        data = tomllib.loads(
            (combos_cli_env / ".fno" / "config.toml").read_text()
        )
        assert data["providers"].get("active_combo") is None
        assert "my-stack" not in data["providers"].get("combos", {})

    def test_remove_unknown_combo_fails(self, combos_cli_env: Path):
        result = _invoke(
            ["combos", "remove", "ghost"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code != 0
        assert "ghost" in result.output


class TestCombosTest:
    def test_test_reports_per_member_health(self, combos_cli_env: Path):
        """AC4.5-FR: test surfaces a-in-cooldown-b-ok-c-ok shape + verdict."""
        from fno.adapters.providers.error_taxonomy import ErrorRule
        from fno.adapters.providers.runtime_state import update_provider_health

        _invoke(
            [
                "combos", "add", "my-stack",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        # Cooldown 'claude-primary' so the verdict becomes partial_cooldown.
        update_provider_health(
            "claude-primary", ErrorRule(status=401, cooldown_ms=60_000),
        )

        result = _invoke(
            ["combos", "test", "my-stack"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0
        assert "claude-primary" in result.output
        assert "in_cooldown" in result.output
        assert "gemini-backup" in result.output
        assert "verdict: partial_cooldown" in result.output


class TestCombosUse:
    def test_use_sets_active_combo(self, combos_cli_env: Path):
        _invoke(
            ["combos", "add", "my-stack", "--providers", "claude-primary"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        result = _invoke(
            ["combos", "use", "my-stack"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0
        data = tomllib.loads(
            (combos_cli_env / ".fno" / "config.toml").read_text()
        )
        assert data["providers"]["active_combo"] == "my-stack"

    def test_use_unknown_combo_fails(self, combos_cli_env: Path):
        result = _invoke(
            ["combos", "use", "ghost"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code != 0
        assert "ghost" in result.output


# ---------------------------------------------------------------------------
# x-84d7 task 1.1: providers list -J emitter (Connections UI read plane)
# ---------------------------------------------------------------------------

class TestListJson:
    def test_list_json_empty_is_array(self, tmp_path: Path):
        """AC1-EDGE groundwork: empty config emits [] on -J (parseable, no prose)."""
        import json as json_mod

        result = _invoke(["list", "-J"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert json_mod.loads(result.output) == []

    def test_list_json_rows_carry_ui_fields(self, tmp_path: Path):
        """AC1-HP: -J emits one row per record with id/cli/auth/priority/active/headroom."""
        import json as json_mod

        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config(active="claude-primary"))
        result = _invoke(["list", "-J"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        rows = json_mod.loads(result.output)
        by_id = {r["id"]: r for r in rows}
        assert set(by_id) == {"claude-primary", "gemini-backup"}
        cp = by_id["claude-primary"]
        assert cp["cli"] == "claude"
        assert cp["auth"] == "oauth_dir"
        assert cp["priority"] == 10
        assert cp["active"] is True
        assert "headroom" in cp        # 'unknown' allowed; the key must exist
        assert "snapshot" in cp        # None for non-managed; key present
        assert by_id["gemini-backup"]["active"] is False


class TestCombosListActiveField:
    def test_combos_list_json_marks_active(self, combos_cli_env: Path):
        """AC1-HP: combos list -J rows gain an 'active' bool from settings.active_combo."""
        import json as json_mod

        _invoke(
            ["combos", "add", "alpha", "--providers", "claude-primary"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        _invoke(
            ["combos", "add", "beta", "--providers", "gemini-backup"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        _invoke(
            ["combos", "use", "beta"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        result = _invoke(
            ["combos", "list", "-J"], cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0, result.output
        rows = json_mod.loads(result.output)
        active_map = {r["name"]: r["active"] for r in rows}
        assert active_map == {"alpha": False, "beta": True}
        # Existing fields unchanged.
        beta = next(r for r in rows if r["name"] == "beta")
        assert beta["members"] == ["gemini-backup"]
        assert "strategy" in beta and "sticky_limit" in beta


# ---------------------------------------------------------------------------
# x-84d7 task 1.1: atomic `combos update` verb (kills the remove+add hazard)
# ---------------------------------------------------------------------------

class TestCombosUpdate:
    def test_update_replaces_members_atomically(self, combos_cli_env: Path):
        """AC4-HP groundwork: exactly one update call commits the new order."""
        _invoke(
            [
                "combos", "add", "main",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        result = _invoke(
            [
                "combos", "update", "main",
                "--providers", "gemini-backup,claude-primary",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0, result.output
        data = tomllib.loads(
            (combos_cli_env / ".fno" / "config.toml").read_text()
        )
        assert data["providers"]["combos"]["main"]["providers"] == [
            "gemini-backup", "claude-primary",
        ]

    def test_update_unknown_member_rejected_without_mutation(
        self, combos_cli_env: Path
    ):
        """AC2-EDGE groundwork: unknown member id rejected; config unchanged."""
        _invoke(
            ["combos", "add", "main", "--providers", "claude-primary"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        before = (combos_cli_env / ".fno" / "config.toml").read_text()
        result = _invoke(
            ["combos", "update", "main", "--providers", "claude-primary,ghost"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code != 0
        assert "ghost" in result.output
        after = (combos_cli_env / ".fno" / "config.toml").read_text()
        assert before == after

    def test_update_unknown_combo_fails(self, combos_cli_env: Path):
        """Updating a combo that does not exist is a refusal, not a create."""
        result = _invoke(
            ["combos", "update", "ghost", "--providers", "claude-primary"],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code != 0
        assert "ghost" in result.output

    def test_update_preserves_strategy_when_omitted(self, combos_cli_env: Path):
        """A pure reorder must NOT silently rewrite round_robin -> fallback."""
        _invoke(
            [
                "combos", "add", "main",
                "--strategy", "round_robin",
                "--sticky", "3",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        # Reorder only: no --strategy/--sticky.
        result = _invoke(
            [
                "combos", "update", "main",
                "--providers", "gemini-backup,claude-primary",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0, result.output
        combo = tomllib.loads(
            (combos_cli_env / ".fno" / "config.toml").read_text()
        )["providers"]["combos"]["main"]
        assert combo["strategy"] == "round_robin"  # preserved
        assert combo["sticky_limit"] == 3           # preserved
        assert combo["providers"] == ["gemini-backup", "claude-primary"]

    def test_update_can_change_strategy(self, combos_cli_env: Path):
        """--strategy on update replaces the stored strategy."""
        _invoke(
            [
                "combos", "add", "main",
                "--strategy", "fallback",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        result = _invoke(
            [
                "combos", "update", "main",
                "--strategy", "round_robin",
                "--sticky", "2",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        assert result.exit_code == 0, result.output
        data = tomllib.loads(
            (combos_cli_env / ".fno" / "config.toml").read_text()
        )
        combo = data["providers"]["combos"]["main"]
        assert combo["strategy"] == "round_robin"
        assert combo["sticky_limit"] == 2

    def test_update_resets_round_robin_cursor(self, combos_cli_env: Path):
        """Reordering members invalidates the stored cursor (hash change)."""
        from fno.adapters.providers.rotation import compute_providers_hash
        from fno.adapters.providers.runtime_state import (
            advance_cursor,
            read_cursor,
        )

        _invoke(
            [
                "combos", "add", "main",
                "--strategy", "round_robin",
                "--providers", "claude-primary,gemini-backup",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        old_hash = compute_providers_hash(("claude-primary", "gemini-backup"))
        advance_cursor(
            "main", sticky_limit=1, providers_hash=old_hash, providers_count=2,
        )
        assert read_cursor("main", old_hash) is not None

        _invoke(
            [
                "combos", "update", "main",
                "--providers", "gemini-backup,claude-primary",
            ],
            cwd=combos_cli_env, home=combos_cli_env,
        )
        new_hash = compute_providers_hash(("gemini-backup", "claude-primary"))
        assert read_cursor("main", new_hash) is None
