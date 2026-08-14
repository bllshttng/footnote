"""Tests for fno providers CLI surface (Phase 02).

Run: cd cli && uv run pytest src/fno/adapters/providers/test_cli.py -v
"""
from __future__ import annotations

import stat
from pathlib import Path

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
                        "harness": "claude",
                        "auth": "oauth_dir",
                        "credentials_source": str(Path.home() / ".claude"),
                        "priority": 10,
                    },
                    {
                        "id": "gemini-backup",
                        "name": "Gemini Backup",
                        "harness": "gemini",
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
        """AC02.3-CLI: accounts list on empty config prints helpful message; exit 0."""
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "No accounts configured" in result.output
        # The remedy it names must be a verb that still exists (AC5 removed
        # `fno providers`), or the empty state teaches a command that exits 2.
        assert "fno config accounts add" in result.output

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
        active_lines = [ln for ln in output_lines if "claude-primary" in ln]
        assert any("*" in line for line in active_lines), (
            f"Expected '*' next to active provider in: {active_lines}"
        )

    def test_list_inactive_has_no_asterisk(self, tmp_path: Path):
        """Non-active record is not marked with *."""
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config(active="claude-primary"))
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        output_lines = result.output.splitlines()
        backup_lines = [ln for ln in output_lines if "gemini-backup" in ln]
        assert backup_lines, "gemini-backup should appear in output"
        assert not any("*" in line for line in backup_lines), (
            f"Expected no '*' next to inactive provider in: {backup_lines}"
        )


# ---------------------------------------------------------------------------
# x-8183: the usage=<age> column (distinct from snapshot=<age>, the
# credential blob age) and the DISARMED footer.
# ---------------------------------------------------------------------------

class TestListUsageColumn:
    def test_never_probed_shows_usage_never(self, tmp_path: Path):
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config())
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        for line in result.output.splitlines():
            if "claude-primary" in line:
                assert "usage=never" in line

    def test_stale_snapshot_reports_its_age_with_ttl(self, tmp_path: Path):
        import os as _os
        import time

        from fno.adapters.providers.runtime_state import write_usage_snapshot
        from fno.adapters.providers.usage import UsageSnapshot

        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config())
        state_path = tmp_path / "provider-runtime-state.json"
        now = time.time()

        # write_usage_snapshot resolves its path the same way `list`'s read
        # does - via FNO_RUNTIME_STATE_PATH - so seed through that env var.
        _os.environ["FNO_RUNTIME_STATE_PATH"] = str(state_path)
        try:
            write_usage_snapshot(
                UsageSnapshot(
                    provider_id="claude-primary", windows=(), probed_at=now - 900,
                    source="oauth-endpoint",
                ),
                now=now,
            )
        finally:
            _os.environ.pop("FNO_RUNTIME_STATE_PATH", None)

        result = runner.invoke(
            providers_app, ["list"],
            env={"HOME": str(tmp_path), "PWD": str(tmp_path),
                 "FNO_RUNTIME_STATE_PATH": str(state_path)},
            catch_exceptions=False,
        )
        assert result.exit_code == 0
        line = next(ln for ln in result.output.splitlines() if "claude-primary" in ln)
        assert "usage=15m (STALE, ttl=5m)" in line

    def test_configured_ttl_renders_in_stale_label(self, tmp_path: Path):
        import os as _os
        import time

        from fno.adapters.providers.runtime_state import write_usage_snapshot
        from fno.adapters.providers.usage import UsageSnapshot

        settings_path = tmp_path / ".fno" / "config.toml"
        config = _two_record_config()
        config["config"]["providers"]["quota"] = {"probe_ttl_seconds": 900}
        _write_settings(settings_path, config)
        state_path = tmp_path / "provider-runtime-state.json"
        now = time.time()

        _os.environ["FNO_RUNTIME_STATE_PATH"] = str(state_path)
        try:
            write_usage_snapshot(
                UsageSnapshot(
                    provider_id="claude-primary", windows=(), probed_at=now - 3600,
                    source="oauth-endpoint",
                ),
                now=now,
            )
        finally:
            _os.environ.pop("FNO_RUNTIME_STATE_PATH", None)

        result = runner.invoke(
            providers_app, ["list"],
            env={"HOME": str(tmp_path), "PWD": str(tmp_path),
                 "FNO_RUNTIME_STATE_PATH": str(state_path)},
            catch_exceptions=False,
        )
        assert result.exit_code == 0
        line = next(ln for ln in result.output.splitlines() if "claude-primary" in ln)
        assert "ttl=15m" in line

    def test_json_rows_carry_usage_fields(self, tmp_path: Path):
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config())
        result = _invoke(["list", "--json"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        import json as _json

        rows = _json.loads(result.output)
        row = next(r for r in rows if r["id"] == "claude-primary")
        assert row["usage_age_s"] is None
        assert row["usage_stale"] is False
        assert row["usage_ttl_seconds"] == 300


class TestListDisarmedFooter:
    def test_footer_prints_when_defer_dispatch_false(self, tmp_path: Path):
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config())
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "rotation is DISARMED" in result.output
        assert "accounts.quota.defer_dispatch" in result.output

    def test_footer_absent_when_defer_dispatch_true(self, tmp_path: Path):
        settings_path = tmp_path / ".fno" / "config.toml"
        config = _two_record_config()
        config["config"]["providers"]["quota"] = {"defer_dispatch": True}
        _write_settings(settings_path, config)
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "rotation is DISARMED" not in result.output

    def test_footer_absent_on_empty_config(self, tmp_path: Path):
        """The disarmed footer is a `list` addendum, not a fresh-install nag;
        the empty-state branch returns before the footer check."""
        result = _invoke(["list"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "rotation is DISARMED" not in result.output

    def test_footer_absent_from_json_output(self, tmp_path: Path):
        settings_path = tmp_path / ".fno" / "config.toml"
        _write_settings(settings_path, _two_record_config())
        result = _invoke(["list", "--json"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0
        assert "DISARMED" not in result.output


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
                "--harness", "claude",
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
                "--harness", "claude",
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
            ["add", "bad-provider", "--harness", "claude", "--auth", "oauth_dir"],
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
            "--harness", "claude",
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
            "--harness", "claude",
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
                "--harness", "claude",
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
        fno_dir = tmp_path / ".fno"
        fno_dir.mkdir(parents=True)
        settings = fno_dir / "config.toml"
        settings.write_text("v2_enabled = false\n", encoding="utf-8")
        original_content = settings.read_text(encoding="utf-8")

        # Make the parent directory read-only so atomic_write can't create a temp file
        fno_dir.chmod(stat.S_IRUSR | stat.S_IXUSR)  # r-x: can list, can't write
        try:
            creds = tmp_path / ".claude"
            creds.mkdir()
            result = _invoke(
                [
                    "add", "fail-provider",
                    "--harness", "claude",
                    "--auth", "oauth_dir",
                    "--credentials-source", str(creds),
                    "--scope", "global",
                ],
                cwd=tmp_path,
                home=tmp_path,
            )
            assert result.exit_code != 0
            # Content must be unchanged
            fno_dir.chmod(stat.S_IRWXU)  # restore to read
            assert settings.read_text(encoding="utf-8") == original_content
        finally:
            # Always restore permissions so pytest can clean up tmp_path
            fno_dir.chmod(stat.S_IRWXU)


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
                            "harness": "claude",
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
                            "harness": "hermes",
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
                            "harness": "claude",
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
                "--harness", "claude",
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
                    "--harness", "claude",
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
                "--harness", "claude",
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
                "--harness", "claude",
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
                "--harness", "claude",
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
                "--harness", "claude",
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
                                "harness": "gemini",
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
        combos = data["accounts"]["combos"]
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
        assert data["accounts"].get("active_combo") is None
        assert "my-stack" not in data["accounts"].get("combos", {})

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
        assert data["accounts"]["active_combo"] == "my-stack"

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
        assert cp["harness"] == "claude"
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
        assert data["accounts"]["combos"]["main"]["providers"] == [
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
        )["accounts"]["combos"]["main"]
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
        combo = data["accounts"]["combos"]["main"]
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


# ---------------------------------------------------------------------------
# AC5-CON: routing-active and slot-active resolve through ONE function
# ---------------------------------------------------------------------------


def _managed_pair_config(active: str) -> dict:
    return {
        "config": {
            "providers": {
                "active": active,
                "records": [
                    {"id": "readyrule", "name": "readyrule", "harness": "claude",
                     "auth": "managed", "priority": 10},
                    {"id": "makers", "name": "makers", "harness": "claude",
                     "auth": "managed", "priority": 20},
                ],
            }
        }
    }


class TestEffectiveActive:
    """`config.providers.active` and the slot occupant can disagree.

    Live on the machine that motivated this: config said `readyrule` while the
    shared slot held `makers`, so the display path marked one account and the
    dispatch path evaluated the other's headroom.
    """

    @pytest.fixture()
    def diverged(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
        _write_settings(tmp_path / ".fno" / "config.toml", _managed_pair_config("readyrule"))
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        from fno.adapters.providers import managed

        managed.stamp_active_slot("claude", "makers")
        return tmp_path

    def test_resolver_returns_the_slot_occupant(self, diverged: Path) -> None:
        from fno.adapters.providers.loader import effective_active

        assert effective_active(repo_root=diverged) == "makers"

    def test_dispatch_path_agrees_with_the_display_path(self, diverged: Path) -> None:
        # The two paths must not disagree: one resolver, called from both.
        from fno.adapters.providers.loader import (
            effective_active,
            is_effective_active,
            load_providers,
        )
        from fno.dispatch import _resolve_provider_id

        config = load_providers(repo_root=diverged)
        marked = [r.id for r in config.records if is_effective_active(r, config)]
        assert marked == ["makers"]
        assert _resolve_provider_id() == effective_active(repo_root=diverged) == "makers"

    def test_list_marks_the_slot_occupant_not_the_config_pointer(
        self, diverged: Path
    ) -> None:
        result = _invoke(["list"], cwd=diverged, home=diverged)
        assert result.exit_code == 0, result.output
        starred = [ln for ln in result.output.splitlines() if ln.lstrip().startswith("*")]
        assert len(starred) == 1
        assert "makers" in starred[0]

    def test_non_managed_active_still_reads_routing_active(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A record whose credential does NOT come from the shared slot keeps
        # routing-active as its authority.
        _write_settings(tmp_path / ".fno" / "config.toml", _two_record_config())
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        from fno.adapters.providers.loader import effective_active

        assert effective_active(repo_root=tmp_path) == "claude-primary"

    def test_managed_active_with_no_stamp_is_nobody(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # No stamp means nothing is known to occupy the slot. Guessing
        # routing-active here is how a dispatch ends up billing an account whose
        # credential is not actually loaded.
        _write_settings(tmp_path / ".fno" / "config.toml", _managed_pair_config("readyrule"))
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        from fno.adapters.providers.loader import effective_active

        assert effective_active(repo_root=tmp_path) is None


# ---------------------------------------------------------------------------
# AC4-HP: fno providers doctor reports the store's real condition
# ---------------------------------------------------------------------------


class TestDoctor:
    """Reconstructs the live store defect: one credential under two ids, both
    blobs expired, slot tainted. Each of those otherwise surfaces only as an
    `unknown` somewhere downstream."""

    @pytest.fixture()
    def sick_store(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
        import json as _json

        _write_settings(tmp_path / ".fno" / "config.toml", _managed_pair_config("readyrule"))
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        from fno.adapters.providers import managed

        # expiresAt in MILLISECONDS (the shape Claude Code writes), already past.
        blob = _json.dumps({
            "claudeAiOauth": {
                "accessToken": "one-shared-token",
                "expiresAt": 1783352000000,
            }
        })
        root = tmp_path / ".fno" / "providers"
        for rid in ("readyrule", "makers"):
            (root / rid).mkdir(parents=True)
            (root / rid / "blob").write_text(blob)
        managed._set_slot_taint("claude", root, True)
        return tmp_path

    def test_names_duplicate_expiry_and_taint_and_exits_nonzero(
        self, sick_store: Path
    ) -> None:
        result = _invoke(["doctor"], cwd=sick_store, home=sick_store)
        assert result.exit_code != 0, result.output
        assert "duplicate-credential" in result.output
        # The pair is named in both directions, so either row points at the other.
        assert "readyrule" in result.output and "makers" in result.output
        assert result.output.count("expired-credential") == 2
        assert "tainted-slot" in result.output
        assert "one-shared-token" not in result.output

    def test_healthy_store_is_quiet_and_exits_zero(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import json as _json

        _write_settings(tmp_path / ".fno" / "config.toml", _managed_pair_config("readyrule"))
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        root = tmp_path / ".fno" / "providers"
        from fno.adapters.providers import managed

        for rid, tok in (("readyrule", "tok-a"), ("makers", "tok-b")):
            (root / rid).mkdir(parents=True)
            (root / rid / "blob").write_text(
                _json.dumps({
                    "claudeAiOauth": {"accessToken": tok, "expiresAt": 4102444800000}
                })
            )
            # A shared-slot account with no proven identity is no longer
            # healthy: its usage cannot be attributed, so doctor says so.
            managed.write_record_principal(
                rid,
                {"account_uuid": f"acct-{rid}", "organization_uuid": "org-1"},
                root,
            )
        result = _invoke(["doctor"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        assert "no problems found" in result.output

    def test_config_dir_without_a_login_is_reported(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A config_dir record whose dir holds no credential would spawn an
        # auth-prompt zombie; doctor is where that becomes visible before then.
        empty = tmp_path / "claude-alt"
        empty.mkdir()
        _write_settings(tmp_path / ".fno" / "config.toml", {
            "config": {"providers": {"active": "alt", "records": [
                {"id": "alt", "name": "alt", "harness": "claude", "auth": "managed",
                 "config_dir": str(empty)},
            ]}}
        })
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        monkeypatch.setattr("fno.agents.account_env._login_present", lambda p: False)
        result = _invoke(["doctor"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code != 0, result.output
        assert "no-login-in-config-dir" in result.output


# ---------------------------------------------------------------------------
# fno providers pick (launch-time headroom picking)
# ---------------------------------------------------------------------------


def _config_dir_pair(tmp_path: Path, active: str = "readyrule") -> dict:
    for name in ("claude-alt", "claude-main"):
        d = tmp_path / name
        d.mkdir(exist_ok=True)
        (d / ".credentials.json").write_text("{}")
    return {
        "config": {
            "providers": {
                "active": active,
                "records": [
                    {"id": "readyrule", "name": "readyrule", "harness": "claude",
                     "auth": "managed", "config_dir": str(tmp_path / "claude-alt")},
                    {"id": "makers", "name": "makers", "harness": "claude",
                     "auth": "managed", "config_dir": str(tmp_path / "claude-main")},
                ],
            }
        }
    }


def _pick_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, cfg: dict) -> Path:
    _write_settings(tmp_path / ".fno" / "config.toml", cfg)
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime-state.json"))
    return tmp_path


class TestPick:
    def test_no_launchable_candidate_exits_4_with_the_setup_instruction(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC9-EDGE: an invisible degradation becomes an actionable instruction.

        Both accounts managed with no config_dir: the non-active one has no
        correct overlay at all, and the active one only rides the shared slot,
        so neither can be pinned to a worker.
        """
        _pick_env(tmp_path, monkeypatch, _managed_pair_config("readyrule"))
        from fno.adapters.providers import managed

        managed.stamp_active_slot("claude", "readyrule")

        result = _invoke(["pick"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 4, result.output
        assert "readyrule" in result.output and "makers" in result.output
        assert "--config-dir" in result.output

    def test_picks_the_first_candidate_with_headroom(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.adapters.providers.runtime_state import write_usage_snapshot
        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        import time as _time

        _pick_env(tmp_path, monkeypatch, _config_dir_pair(tmp_path))
        now = _time.time()  # headroom reads against the wall clock, not a fixture epoch
        write_usage_snapshot(
            UsageSnapshot("readyrule", (UsageWindow("5h", 100.0, now + 3600),), now, "t"),
            now=now,
        )
        write_usage_snapshot(
            UsageSnapshot("makers", (UsageWindow("5h", 4.0, now + 3600),), now, "t"),
            now=now,
        )
        verdict = _pick_verdict(tmp_path)
        assert verdict.account == "makers"
        assert verdict.exit_code == 0

    def test_every_launchable_candidate_exhausted_exits_3(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.adapters.providers.runtime_state import write_usage_snapshot
        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        import time as _time

        _pick_env(tmp_path, monkeypatch, _config_dir_pair(tmp_path))
        now = _time.time()
        for rid in ("readyrule", "makers"):
            write_usage_snapshot(
                UsageSnapshot(rid, (UsageWindow("5h", 100.0, now + 3600),), now, "t"),
                now=now,
            )
        result = _invoke(["pick"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 3, result.output
        assert "exhausted" in result.output

    def test_unprobeable_set_keeps_combo_order(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC14-EDGE: with no snapshots at all, ordering is unchanged.

        UNKNOWN never reads as EXHAUSTED and never blocks a launch, so the
        answer is the first launchable candidate in the operator's configured
        order - byte-identical to what the pre-existing walk returns.
        """
        _pick_env(tmp_path, monkeypatch, _config_dir_pair(tmp_path))
        from fno.adapters.providers.rotation import Combo, next_healthy_provider

        verdict = _pick_verdict(tmp_path)
        expected = next_healthy_provider(
            Combo(name="pick", providers=("readyrule", "makers"))
        )
        assert verdict.account == expected == "readyrule"
        assert dict(verdict.candidates)["readyrule"] == "unknown"

    def test_exclude_skips_a_known_dead_account(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _pick_env(tmp_path, monkeypatch, _config_dir_pair(tmp_path))
        verdict = _pick_verdict(tmp_path, exclude=("readyrule",))
        assert verdict.account == "makers"

    def test_print_env_emits_the_picked_config_dir(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _pick_env(tmp_path, monkeypatch, _config_dir_pair(tmp_path))
        result = _invoke(["pick", "--print-env"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        assert f"CLAUDE_CONFIG_DIR={tmp_path / 'claude-alt'}" in result.stdout

    def test_stderr_carries_a_reason_even_on_success(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Fail-open must not become fail-silent: every verdict is explained.
        _pick_env(tmp_path, monkeypatch, _config_dir_pair(tmp_path))
        verdict = _pick_verdict(tmp_path)
        assert verdict.reason
        assert len(verdict.candidates) == 2


def _pick_verdict(cwd: Path, exclude: tuple[str, ...] = ()):
    from fno.adapters.providers.cli import pick_account

    return pick_account(exclude=exclude)


class TestPickOptInAndOverlay:
    """The two halves a non-Python caller cannot get right on its own."""

    def test_if_armed_declines_when_the_knob_is_off(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Without this the Rust loop would pick on a default-off install and
        # change which account gets billed without being asked.
        cfg = _config_dir_pair(tmp_path)
        cfg["config"]["providers"]["quota"] = {"pick_on_launch": False}
        _pick_env(tmp_path, monkeypatch, cfg)
        result = _invoke(["pick", "--if-armed"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 5, result.output
        assert "not armed" in result.output

    def test_if_armed_picks_normally_once_the_knob_is_on(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        cfg = _config_dir_pair(tmp_path)
        cfg["config"]["providers"]["quota"] = {"pick_on_launch": True}
        _pick_env(tmp_path, monkeypatch, cfg)
        result = _invoke(["pick", "--if-armed"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        assert "readyrule" in result.stdout

    def test_without_if_armed_the_knob_is_not_consulted(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # An operator running `fno providers pick` by hand asked for an answer.
        cfg = _config_dir_pair(tmp_path)
        cfg["config"]["providers"]["quota"] = {"pick_on_launch": False}
        _pick_env(tmp_path, monkeypatch, cfg)
        result = _invoke(["pick"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output

    def test_print_env_emits_the_auth_vars_to_clear(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Pinning CLAUDE_CONFIG_DIR alone is half an overlay.

        An inherited ANTHROPIC_API_KEY / CLAUDE_CODE_OAUTH_TOKEN / routed
        ANTHROPIC_BASE_URL outranks it, so the worker would bill through a
        different route while the receipt named the picked account.
        """
        from fno.agents.account_env import SCRUB_AUTH_VARS

        _pick_env(tmp_path, monkeypatch, _config_dir_pair(tmp_path))
        result = _invoke(["pick", "--print-env"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        emitted = dict(
            line.split("=", 1) for line in result.stdout.splitlines() if "=" in line
        )
        assert emitted["CLAUDE_CONFIG_DIR"] == str(tmp_path / "claude-alt")
        for var in SCRUB_AUTH_VARS:
            assert var in emitted, f"{var} not carried for scrubbing"
            assert emitted[var] == "", f"{var} should be cleared, not set"


# ---------------------------------------------------------------------------
# x-4b8d: fno config accounts reconcile-slot (the taint's missing clearer)
# ---------------------------------------------------------------------------


class TestReconcileSlot:
    """`doctor` could report a tainted slot but never repair one, and no verb
    could. The repair is deliberately not a blind `clear-taint`: it acts only on
    proven identity, so its refusals are as load-bearing as its successes."""

    @pytest.fixture()
    def store(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
        _write_settings(tmp_path / ".fno" / "config.toml", _managed_pair_config("readyrule"))
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        root = tmp_path / ".fno" / "providers"
        root.mkdir(parents=True, exist_ok=True)
        return tmp_path

    def test_success_names_the_matched_record_and_no_credential(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC1-HP: a non-secret receipt naming the record reconciliation proved."""
        from fno.adapters.providers import managed

        monkeypatch.setattr(
            managed, "reconcile_slot",
            lambda cli, **kw: managed.ReconcileResult(
                "matched", record_id="readyrule",
                detail="the live claude slot belongs to 'readyrule'; taint cleared",
            ),
        )
        result = _invoke(["reconcile-slot", "claude"], cwd=store, home=store)
        assert result.exit_code == 0, result.output
        assert "readyrule" in result.output
        assert "token" not in result.output.lower()

    @pytest.mark.parametrize(
        "outcome",
        ["profile-unavailable", "malformed-profile", "zero-match",
         "ambiguous-match", "lock-timeout", "no-slot-credential"],
    )
    def test_every_refusal_exits_nonzero_and_names_its_type(
        self, store: Path, monkeypatch: pytest.MonkeyPatch, outcome: str
    ) -> None:
        """AC3-ERR: US3 - an outage must be loud, and say which way it failed."""
        from fno.adapters.providers import managed

        monkeypatch.setattr(
            managed, "reconcile_slot",
            lambda cli, **kw: managed.ReconcileResult(outcome, detail="why it refused"),
        )
        result = _invoke(["reconcile-slot", "claude"], cwd=store, home=store)
        assert result.exit_code != 0, result.output
        assert outcome in result.output
        assert "why it refused" in result.output

    def test_unknown_harness_refuses_without_touching_the_store(
        self, store: Path
    ) -> None:
        result = _invoke(["reconcile-slot", "gemini"], cwd=store, home=store)
        assert result.exit_code != 0
        assert "claude" in result.output

    def test_doctor_names_the_repair_command_for_a_tainted_slot(
        self, store: Path
    ) -> None:
        """A finding an operator cannot act on is only half a diagnosis."""
        from fno.adapters.providers import managed

        managed._set_slot_taint("claude", store / ".fno" / "providers", True)
        result = _invoke(["doctor"], cwd=store, home=store)
        assert result.exit_code != 0, result.output
        assert "tainted-slot" in result.output
        assert "fno config accounts reconcile-slot claude" in result.output

    def test_register_binds_the_principal_it_just_captured(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Registration is where footnote KNOWS whose credential it holds, so it
        is the only place a principal can be bound without proving anything."""
        import json as _json

        from fno.adapters.providers import managed

        blob = _json.dumps({"claudeAiOauth": {"accessToken": "live-token"}})
        monkeypatch.setattr(managed, "_read_slot_blob", lambda cli, config_dir=None: blob)
        monkeypatch.setattr(managed, "canonical_slot_blobs", lambda cli: [blob])
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda b: (
                {"account_uuid": "acct-1", "organization_uuid": "org-1",
                 "email": "jn@example.com"},
                None,
            ),
        )
        result = _invoke(["register", "readyrule"], cwd=store, home=store)
        assert result.exit_code == 0, result.output
        root = store / ".fno" / "providers"
        bound = managed.record_principal("readyrule", root)
        assert bound is not None and bound["account_uuid"] == "acct-1"
        assert "acct-1" not in result.output

    def test_register_refuses_while_the_slot_holds_two_accounts(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Binding whichever credential the snapshot happened to pick would file
        a stale account's identity under the new id."""
        import json as _json

        from fno.adapters.providers import managed

        a = _json.dumps({"claudeAiOauth": {"accessToken": "scoped"}})
        b = _json.dumps({"claudeAiOauth": {"accessToken": "unscoped"}})
        monkeypatch.setattr(managed, "canonical_slot_blobs", lambda cli: [a, b])
        seen = {a: "acct-a", b: "acct-b"}
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (
                {"account_uuid": seen[blob], "organization_uuid": "org-1"}, None
            ),
        )

        result = _invoke(["register", "readyrule"], cwd=store, home=store)

        assert result.exit_code != 0
        assert "two different accounts" in result.output
        assert managed.record_principal(
            "readyrule", store / ".fno" / "providers"
        ) is None

    def test_doctor_reports_an_ambiguous_slot(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Two accounts in one slot must not read as healthy."""
        from fno.adapters.providers import managed

        root = store / ".fno" / "providers"
        managed.stamp_active_slot("claude", "readyrule", root)
        managed.write_record_principal(
            "readyrule", {"account_uuid": "acct-a", "organization_uuid": "org-1"}, root
        )
        monkeypatch.setattr(managed, "canonical_slot_blobs", lambda cli: ["a", "b"])
        keys = {"a": "acct-a", "b": "acct-b"}
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (
                {"account_uuid": keys[blob], "organization_uuid": "org-1"}, None
            ),
        )

        result = _invoke(["doctor"], cwd=store, home=store)

        assert result.exit_code != 0, result.output
        assert "ambiguous-slot" in result.output
        assert "fno config accounts reconcile-slot claude" in result.output

    def test_doctor_names_an_out_of_band_login_as_drift(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The stamp is untainted and wrong: nothing downstream hesitates, so
        doctor is the only place this becomes visible before the bill does."""
        from fno.adapters.providers import managed

        root = store / ".fno" / "providers"
        managed.stamp_active_slot("claude", "readyrule", root)
        managed.write_record_principal(
            "readyrule", {"account_uuid": "acct-a", "organization_uuid": "org-1"}, root
        )
        monkeypatch.setattr(managed, "canonical_slot_blobs", lambda cli: ["{}"])
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (
                {"account_uuid": "acct-b", "organization_uuid": "org-1",
                 "email": "other@example.com"},
                None,
            ),
        )

        result = _invoke(["doctor"], cwd=store, home=store)

        assert result.exit_code != 0, result.output
        assert "slot-identity-drift" in result.output
        assert "readyrule" in result.output and "other@example.com" in result.output
        assert "fno config accounts reconcile-slot claude" in result.output

    def test_doctor_pays_no_profile_call_without_a_bound_principal(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Free until it can answer: with nothing to compare there is no question."""
        from fno.adapters.providers import managed

        managed.stamp_active_slot("claude", "readyrule", store / ".fno" / "providers")
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: pytest.fail("an unbound store must not reach the endpoint"),
        )
        result = _invoke(["doctor"], cwd=store, home=store)
        assert "slot-identity-drift" not in result.output

    def test_a_matching_principal_is_not_drift(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.adapters.providers import managed

        root = store / ".fno" / "providers"
        managed.stamp_active_slot("claude", "readyrule", root)
        managed.write_record_principal(
            "readyrule", {"account_uuid": "acct-a", "organization_uuid": "org-1"}, root
        )
        monkeypatch.setattr(managed, "canonical_slot_blobs", lambda cli: ["{}"])
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda blob: (
                {"account_uuid": "acct-a", "organization_uuid": "org-1"}, None
            ),
        )
        result = _invoke(["doctor"], cwd=store, home=store)
        assert "slot-identity-drift" not in result.output

    def test_doctor_names_an_unbound_principal_as_the_reason_for_unknown(
        self, store: Path
    ) -> None:
        """Fail-closed attribution must not be silent - the silence is the
        original disease. doctor is where 'unknown' gets a reason and a fix."""
        result = _invoke(["doctor"], cwd=store, home=store)

        assert result.exit_code != 0, result.output
        assert "unbound-principal" in result.output
        assert "fno config accounts register readyrule" in result.output

    def test_a_bound_record_is_not_reported_unbound(
        self, store: Path
    ) -> None:
        from fno.adapters.providers import managed

        root = store / ".fno" / "providers"
        for rid in ("readyrule", "makers"):
            managed.write_record_principal(
                rid,
                {"account_uuid": f"acct-{rid}", "organization_uuid": "org-1"},
                root,
            )
        result = _invoke(["doctor"], cwd=store, home=store)
        assert "unbound-principal" not in result.output

    @pytest.mark.parametrize(
        "failure,needle",
        [
            ("slot-changed", "changed while"),
            ("some-future-failure", "some-future-failure"),
        ],
    )
    def test_a_capture_that_wrote_nothing_never_reports_success(
        self, store: Path, monkeypatch: pytest.MonkeyPatch, failure: str, needle: str
    ) -> None:
        """A typed failure with no handler wrote nothing, so printing
        'Registered' would be a lie - including for a failure added later."""
        from fno.adapters.providers import managed

        # A None account_dir is how the capture says it wrote nothing.
        monkeypatch.setattr(
            managed, "register_slot_snapshot",
            lambda record, *a, **kw: (None, None, failure),
        )

        result = _invoke(["register", "readyrule"], cwd=store, home=store)

        assert result.exit_code != 0, result.output
        assert needle in result.output
        assert "Registered managed account" not in result.output

    def test_an_unprovable_identity_still_registers(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Registration must work offline; the record is simply unbound."""
        from pathlib import Path as _P

        from fno.adapters.providers import managed

        monkeypatch.setattr(
            managed, "register_slot_snapshot",
            lambda record, *a, **kw: (_P("/x"), None, "profile-unavailable"),
        )

        result = _invoke(["register", "readyrule"], cwd=store, home=store)

        assert result.exit_code == 0, result.output
        assert "Registered managed account" in result.output

    def test_a_late_slot_move_registers_with_a_warning(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The account IS registered; only the stamp's trustworthiness is in
        question, so refusing would be as wrong as staying silent."""
        from pathlib import Path as _P

        from fno.adapters.providers import managed

        monkeypatch.setattr(
            managed, "register_slot_snapshot",
            lambda record, *a, **kw: (_P("/x"), None, "slot-moved-after-write"),
        )

        result = _invoke(["register", "readyrule"], cwd=store, home=store)

        assert result.exit_code == 0, result.output
        assert "Registered managed account" in result.output
        assert "marked tainted" in result.output
        assert "reconcile-slot claude" in result.output

    def test_concurrent_registrations_do_not_drop_each_other(
        self, store: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """`_persist` runs under the slot lock and must re-read there: merging
        into the record set loaded before the lock would let the later save
        silently drop the earlier account."""
        import json as _json

        from fno.adapters.providers import managed
        from fno.adapters.providers.loader import load_providers

        blob = _json.dumps({"claudeAiOauth": {"accessToken": "live"}})
        monkeypatch.setattr(managed, "canonical_slot_blobs", lambda cli: [blob])
        monkeypatch.setattr(
            managed, "slot_principal",
            lambda b: (
                {"account_uuid": "acct-new", "organization_uuid": "org-1"}, None
            ),
        )
        # A rival registration lands between the pre-lock load and _persist.
        real = managed.register_slot_snapshot

        def _racing(record, *a, **kw):
            persist = kw.get("persist")

            def _wrapped() -> None:
                from fno.adapters.providers.loader import save_providers
                from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

                existing = load_providers(repo_root=store)
                save_providers(
                    ProvidersConfig(
                        records=[*existing.records,
                                 ProviderRecord(id="rival", name="rival",
                                                harness="claude", auth="managed")],
                        active=existing.active,
                    ),
                    scope="global",
                )
                if persist is not None:
                    persist()

            kw["persist"] = _wrapped
            return real(record, *a, **kw)

        monkeypatch.setattr(managed, "register_slot_snapshot", _racing)

        result = _invoke(["register", "fresh"], cwd=store, home=store)

        assert result.exit_code == 0, result.output
        ids = {r.id for r in load_providers(repo_root=store).records}
        assert {"rival", "fresh"} <= ids


# ---------------------------------------------------------------------------
# usage --refresh: the probe result IS the displayed result
#
# The defect these pin: `probe_usage` returned real 5h/weekly windows while
# `fno config accounts usage --refresh` printed `unknown` in the same revision.
# Two reads of one observation can disagree; one read cannot.
# ---------------------------------------------------------------------------


def _usage_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    _write_settings(tmp_path / ".fno" / "config.toml", _config_dir_pair(tmp_path))
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime-state.json"))
    return tmp_path


def _stub_probe(monkeypatch: pytest.MonkeyPatch, snapshots: dict) -> None:
    """Make the single probe path return a known snapshot per record id."""
    monkeypatch.setattr(
        "fno.adapters.providers.usage.probe_usage_detail",
        lambda record, now=None: (
            (snapshots[record.id], None)
            if record.id in snapshots
            else (None, "probe-failed")
        ),
    )


class TestUsageRefreshContract:
    def test_the_probed_snapshot_is_the_json_output(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC1-HP: same invocation, same observation, field for field."""
        import json as _json

        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        _usage_env(tmp_path, monkeypatch)
        probed = UsageSnapshot(
            provider_id="readyrule",
            windows=(
                UsageWindow("5h", 9.0, 1_800_000_000.0),
                UsageWindow("weekly", 95.0, 1_800_600_000.0),
            ),
            probed_at=1_700_000_000.0,
            source="oauth-endpoint",
        )
        _stub_probe(monkeypatch, {"readyrule": probed})

        result = _invoke(["usage", "--refresh", "--json"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        entry = _json.loads(result.output.strip())["readyrule"]

        assert entry["source"] == probed.source
        assert entry["probed_at"] == probed.probed_at
        assert entry["windows"] == [
            {"label": w.label, "used_pct": w.used_pct, "resets_at": w.resets_at}
            for w in probed.windows
        ]

    def test_a_failed_cache_write_still_displays_the_snapshot(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC2-FR: persistence degradation is reported, not substituted."""
        import json as _json

        from fno.adapters.providers import runtime_state as rs
        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        _usage_env(tmp_path, monkeypatch)
        probed = UsageSnapshot(
            "readyrule", (UsageWindow("5h", 12.0, 1_800_000_000.0),), 1_700_000_000.0, "oauth-endpoint"
        )
        _stub_probe(monkeypatch, {"readyrule": probed})
        monkeypatch.setattr(rs, "write_usage_snapshot", lambda s, now=None, **k: False)

        result = _invoke(["usage", "--refresh", "--json"], cwd=tmp_path, home=tmp_path)
        entry = _json.loads(result.output.strip())["readyrule"]
        assert entry["windows"][0]["used_pct"] == 12.0
        assert entry["persisted"] is False

    def test_unknown_names_the_boundary_that_failed(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC3-ERR: JSON carries a machine-readable reason; no credentials."""
        import json as _json

        _usage_env(tmp_path, monkeypatch)
        _stub_probe(monkeypatch, {})  # every record probes to unknown

        result = _invoke(["usage", "--refresh", "--json"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        payload = _json.loads(result.output.strip())
        assert payload["readyrule"] == {"state": "unknown", "reason": "probe-failed"}
        assert payload["makers"] == {"state": "unknown", "reason": "probe-failed"}

    def test_an_unattributable_record_never_borrows_other_windows(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Failure Modes / Boundaries: unknown stays unknown, never inherited."""
        import json as _json

        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        _usage_env(tmp_path, monkeypatch)
        probed = UsageSnapshot(
            "readyrule", (UsageWindow("5h", 9.0, 1_800_000_000.0),), 1_700_000_000.0, "oauth-endpoint"
        )
        monkeypatch.setattr(
            "fno.adapters.providers.usage.probe_usage_detail",
            lambda record, now=None: (
                (probed, None) if record.id == "readyrule" else (None, "unattributed")
            ),
        )

        result = _invoke(["usage", "--refresh", "--json"], cwd=tmp_path, home=tmp_path)
        payload = _json.loads(result.output.strip())
        assert payload["readyrule"]["windows"][0]["used_pct"] == 9.0
        assert payload["makers"] == {"state": "unknown", "reason": "unattributed"}

    def test_human_output_stays_one_compact_line_per_state(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

        _usage_env(tmp_path, monkeypatch)
        import time as _time

        probed = UsageSnapshot(
            "readyrule", (UsageWindow("5h", 9.0, _time.time() + 3600),), _time.time(), "oauth-endpoint"
        )
        _stub_probe(monkeypatch, {"readyrule": probed})

        result = _invoke(["usage", "--refresh"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        assert "readyrule  [claude]  5h" in result.output
        assert "makers  [claude]  unknown (probe-failed)" in result.output

    def test_without_refresh_an_unprobed_record_says_so(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import json as _json

        _usage_env(tmp_path, monkeypatch)
        result = _invoke(["usage", "--json"], cwd=tmp_path, home=tmp_path)
        assert result.exit_code == 0, result.output
        payload = _json.loads(result.output.strip())
        assert payload["readyrule"] == {"state": "unknown", "reason": "not-probed"}
