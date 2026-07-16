"""Tests for dispatch_env and resolve_env_value.

Phase 03 of the provider rotation substrate (ab-256f6b6e).
Covers AC03.1-HP (dispatch arm), AC03.3-FR, AC03.4-FR, AC03.6-EDGE,
AC03.7-FR, and the ProviderNotFoundError / ProviderUnavailableError contract.

Tests never invoke the real macOS `security` command: keychain calls are
intercepted via monkeypatch on subprocess.run.
"""
from __future__ import annotations

import concurrent.futures
import subprocess
from pathlib import Path

import pytest
import yaml

from fno.adapters.providers.model import (
    ProviderNotFoundError,
    ProviderRecord,
    ProviderUnavailableError,
)
from fno.adapters.providers.staging import stage
from fno.adapters.providers.dispatch import dispatch_env, resolve_env_value


# ---------------------------------------------------------------------------
# Helpers / fixtures
# ---------------------------------------------------------------------------

def _write_settings(tmp_path: Path, records: list[dict]) -> Path:
    """Write a settings.yaml with config.providers.records and return the repo_root."""
    settings = {
        "config": {
            "providers": {
                "records": records,
            }
        }
    }
    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir(parents=True, exist_ok=True)
    settings_path = abilities_dir / "settings.yaml"
    settings_path.write_text(yaml.safe_dump(settings))
    return tmp_path


@pytest.fixture()
def creds_source(tmp_path: Path) -> Path:
    source = tmp_path / "canonical-creds"
    source.mkdir()
    (source / ".credentials.json").write_text('{"token": "dummy"}')
    return source


@pytest.fixture()
def staging_root(tmp_path: Path) -> Path:
    root = tmp_path / "providers"
    root.mkdir()
    return root


# ---------------------------------------------------------------------------
# AC03.1-HP: dispatch round-trip for oauth_dir / claude
# ---------------------------------------------------------------------------

def test_dispatch_env_claude_oauth(
    tmp_path: Path, creds_source: Path, staging_root: Path
) -> None:
    """dispatch_env for a claude oauth_dir record returns CLAUDE_CONFIG_DIR."""
    record = ProviderRecord(
        id="claude-max-primary",
        name="Claude Max Primary",
        cli="claude",
        auth="oauth_dir",
        credentials_source=creds_source,
    )
    repo_root = _write_settings(tmp_path, [record.model_dump(mode="json", exclude_none=True)])
    stage(record, root=staging_root)

    env = dispatch_env(record.id, repo_root=repo_root, root=staging_root)

    expected_link = staging_root / record.id / ".claude"
    assert env == {"CLAUDE_CONFIG_DIR": str(expected_link)}


def test_dispatch_env_gemini_oauth(
    tmp_path: Path, creds_source: Path, staging_root: Path
) -> None:
    """dispatch_env for a gemini oauth_dir record returns HOME pointing at staged home."""
    record = ProviderRecord(
        id="gemini-pro-a",
        name="Gemini Pro A",
        cli="gemini",
        auth="oauth_dir",
        credentials_source=creds_source,
    )
    repo_root = _write_settings(tmp_path, [record.model_dump(mode="json", exclude_none=True)])
    stage(record, root=staging_root)

    env = dispatch_env(record.id, repo_root=repo_root, root=staging_root)

    expected_home = staging_root / record.id / "home"
    assert env == {"HOME": str(expected_home)}


# ---------------------------------------------------------------------------
# AC03.6-EDGE: api_key env resolution via mocked keychain
# ---------------------------------------------------------------------------

def test_dispatch_env_api_key_keychain(
    tmp_path: Path, staging_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """dispatch_env for api_key record resolves KEYCHAIN refs via security CLI."""
    fake_key = "sk-ant-fake-key-abc123"
    captured_cmd: list[list[str]] = []

    def fake_subprocess_run(cmd, **kwargs):
        captured_cmd.append(list(cmd))
        result = subprocess.CompletedProcess(cmd, returncode=0)
        result.stdout = fake_key + "\n"
        return result

    import fno.adapters.providers.dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod.subprocess, "run", fake_subprocess_run)

    record = ProviderRecord(
        id="anthropic-api-via-openclaw",
        name="Anthropic API via OpenClaw",
        cli="openclaw",
        auth="api_key",
        env={"ANTHROPIC_API_KEY": "${KEYCHAIN:anthropic-api-key-default}"},
    )
    repo_root = _write_settings(tmp_path, [record.model_dump(mode="json", exclude_none=True)])
    stage(record, root=staging_root)

    env = dispatch_env(record.id, repo_root=repo_root, root=staging_root)

    assert env == {"ANTHROPIC_API_KEY": fake_key}
    # Verify the security command was called with correct args.
    assert len(captured_cmd) == 1
    assert captured_cmd[0] == [
        "security", "find-generic-password", "-w", "-s", "anthropic-api-key-default"
    ]


# ---------------------------------------------------------------------------
# ProviderNotFoundError
# ---------------------------------------------------------------------------

def test_dispatch_env_unknown_provider_raises_not_found(
    tmp_path: Path, staging_root: Path
) -> None:
    """dispatch_env raises ProviderNotFoundError for unknown provider_id."""
    repo_root = _write_settings(tmp_path, [])
    with pytest.raises(ProviderNotFoundError):
        dispatch_env("totally-unknown-id", repo_root=repo_root, root=staging_root)


# ---------------------------------------------------------------------------
# AC03.4-FR: Unstaged provider raises ProviderUnavailableError
# ---------------------------------------------------------------------------

def test_dispatch_env_unstaged_oauth_raises_unavailable(
    tmp_path: Path, creds_source: Path, staging_root: Path
) -> None:
    """dispatch_env raises ProviderUnavailableError when oauth_dir not staged."""
    record = ProviderRecord(
        id="claude-unstaged",
        name="Claude Unstaged",
        cli="claude",
        auth="oauth_dir",
        credentials_source=creds_source,
    )
    repo_root = _write_settings(tmp_path, [record.model_dump(mode="json", exclude_none=True)])
    # Deliberately do NOT call stage().
    with pytest.raises(ProviderUnavailableError, match="not staged"):
        dispatch_env(record.id, repo_root=repo_root, root=staging_root)


# ---------------------------------------------------------------------------
# AC03.7-FR: Unresolvable env ref raises ProviderUnavailableError
# ---------------------------------------------------------------------------

def test_dispatch_env_unresolvable_keychain_raises(
    tmp_path: Path, staging_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC03.7: dispatch_env raises ProviderUnavailableError when keychain lookup fails.

    The error must name BOTH the env var (ANTHROPIC_API_KEY) AND the
    unresolvable keychain reference (does-not-exist), per AC03.7 wording:
    'the error names the unresolvable reference'.
    """
    def fail_subprocess_run(cmd, **kwargs):
        raise subprocess.CalledProcessError(44, cmd)

    import fno.adapters.providers.dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod.subprocess, "run", fail_subprocess_run)

    record = ProviderRecord(
        id="anthropic-api-via-openclaw",
        name="Anthropic API via OpenClaw",
        cli="openclaw",
        auth="api_key",
        env={"ANTHROPIC_API_KEY": "${KEYCHAIN:does-not-exist}"},
    )
    repo_root = _write_settings(tmp_path, [record.model_dump(mode="json", exclude_none=True)])
    stage(record, root=staging_root)

    with pytest.raises(ProviderUnavailableError) as exc_info:
        dispatch_env(record.id, repo_root=repo_root, root=staging_root)

    msg = str(exc_info.value)
    assert "ANTHROPIC_API_KEY" in msg, f"error should name the env var; got: {msg!r}"
    assert "does-not-exist" in msg, f"error should name the unresolvable keychain reference; got: {msg!r}"


# ---------------------------------------------------------------------------
# AC03.3-FR: Concurrency safety
# ---------------------------------------------------------------------------

def test_dispatch_env_concurrency_safe(
    tmp_path: Path, creds_source: Path, staging_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """dispatch_env is safe to call concurrently from many threads."""
    record_a = ProviderRecord(
        id="claude-a",
        name="Claude A",
        cli="claude",
        auth="oauth_dir",
        credentials_source=creds_source,
    )
    record_b = ProviderRecord(
        id="gemini-b",
        name="Gemini B",
        cli="gemini",
        auth="oauth_dir",
        credentials_source=creds_source,
    )

    raw_a = record_a.model_dump(mode="json", exclude_none=True)
    raw_b = record_b.model_dump(mode="json", exclude_none=True)
    repo_root = _write_settings(tmp_path, [raw_a, raw_b])

    stage(record_a, root=staging_root)
    stage(record_b, root=staging_root)

    expected_a = dispatch_env(record_a.id, repo_root=repo_root, root=staging_root)
    expected_b = dispatch_env(record_b.id, repo_root=repo_root, root=staging_root)

    def call(pid: str) -> dict:
        return dispatch_env(pid, repo_root=repo_root, root=staging_root)

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        results_a = list(pool.map(call, [record_a.id] * 100))
        results_b = list(pool.map(call, [record_b.id] * 100))

    assert all(r == expected_a for r in results_a), "Result for A leaked"
    assert all(r == expected_b for r in results_b), "Result for B leaked"
    # Verify no cross-contamination: A results must never equal B.
    assert expected_a != expected_b
    assert all(r != expected_b for r in results_a), "A result matched B"


# ---------------------------------------------------------------------------
# resolve_env_value unit tests
# ---------------------------------------------------------------------------

def test_resolve_env_value_plain_string() -> None:
    """Plain strings (no $ prefix) are returned as-is."""
    assert resolve_env_value("hello") == "hello"
    assert resolve_env_value("") == ""


def test_resolve_env_value_env_var(monkeypatch: pytest.MonkeyPatch) -> None:
    """${ENV:VAR} resolves from os.environ."""
    monkeypatch.setenv("MY_TEST_VAR_XYZ", "resolved-value")
    assert resolve_env_value("${ENV:MY_TEST_VAR_XYZ}") == "resolved-value"


def test_resolve_env_value_env_var_missing(monkeypatch: pytest.MonkeyPatch) -> None:
    """${ENV:MISSING_VAR} raises ProviderUnavailableError."""
    monkeypatch.delenv("MISSING_ENV_VAR_ABC", raising=False)
    with pytest.raises(ProviderUnavailableError):
        resolve_env_value("${ENV:MISSING_ENV_VAR_ABC}")


def test_resolve_env_value_keychain_success(monkeypatch: pytest.MonkeyPatch) -> None:
    """${KEYCHAIN:item} calls security and returns stripped stdout."""
    def fake_run(cmd, **kwargs):
        result = subprocess.CompletedProcess(cmd, returncode=0)
        result.stdout = "my-secret-value\n"
        return result

    import fno.adapters.providers.dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod.subprocess, "run", fake_run)
    assert resolve_env_value("${KEYCHAIN:my-item}") == "my-secret-value"


def test_resolve_env_value_keychain_missing(monkeypatch: pytest.MonkeyPatch) -> None:
    """${KEYCHAIN:missing} raises ProviderUnavailableError when security fails."""
    def fail_run(cmd, **kwargs):
        raise subprocess.CalledProcessError(44, cmd)

    import fno.adapters.providers.dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod.subprocess, "run", fail_run)
    with pytest.raises(ProviderUnavailableError, match="my-missing"):
        resolve_env_value("${KEYCHAIN:my-missing}")


def test_resolve_env_value_file_success(tmp_path: Path) -> None:
    """${FILE:/path} reads the first line of the file."""
    key_file = tmp_path / "api.key"
    key_file.write_text("sk-file-key\nsecond line\n")
    assert resolve_env_value(f"${{FILE:{key_file}}}") == "sk-file-key"


def test_resolve_env_value_file_missing(tmp_path: Path) -> None:
    """${FILE:/missing} raises ProviderUnavailableError."""
    missing = tmp_path / "no-such-file.key"
    with pytest.raises(ProviderUnavailableError, match=str(missing)):
        resolve_env_value(f"${{FILE:{missing}}}")


def test_resolve_env_value_literal_escape() -> None:
    """${literal_value} (no recognized prefix) returns the inner contents verbatim."""
    assert resolve_env_value("${just-a-value}") == "just-a-value"


# ---------------------------------------------------------------------------
# Task 2b.1: spawn_with_provider_snapshot - subprocess env snapshot at spawn.
# Phase 02b of provider rotation failover (ab-9728b70b).
# ---------------------------------------------------------------------------

class TestSpawnSnapshot:
    def _seed_settings(self, tmp_path: Path, *, active: str) -> Path:
        import yaml as _yaml
        settings_path = tmp_path / "settings.yaml"
        settings_path.write_text(_yaml.safe_dump({
            "config": {
                "providers": {
                    "active": active,
                    "records": [
                        {
                            "id": "claude-anthropic",
                            "name": "Claude Direct",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            "credentials_source": "~/.claude",
                            "base_url": "https://api.anthropic.com",
                            "pricing": {
                                "input_per_million_usd": 15.0,
                                "output_per_million_usd": 75.0,
                            },
                        },
                        {
                            "id": "claude-openrouter",
                            "name": "OpenRouter Claude",
                            "cli": "claude",
                            "auth": "api_key",
                            "env": {"ANTHROPIC_API_KEY": "sk-or-test"},
                            "base_url": "https://openrouter.ai/api/v1",
                        },
                    ],
                }
            }
        }))
        return settings_path

    def test_hp1_subprocess_inherits_provider_id(self, tmp_path: Path):
        from fno.adapters.providers.dispatch import (
            spawn_with_provider_snapshot,
        )

        settings_path = self._seed_settings(tmp_path, active="claude-anthropic")
        proc = spawn_with_provider_snapshot(
            ["env"],
            settings_path=settings_path,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        out, _ = proc.communicate(timeout=5)
        text = out.decode()
        assert "FNO_PROVIDER_ID=claude-anthropic" in text
        assert "FNO_PROVIDER_AUTH=oauth_dir" in text
        assert "FNO_PROVIDER_BASE_URL=https://api.anthropic.com" in text

    def test_pricing_serialized_to_env(self, tmp_path: Path):
        import json as _json
        from fno.adapters.providers.dispatch import (
            spawn_with_provider_snapshot,
        )

        settings_path = self._seed_settings(tmp_path, active="claude-anthropic")
        proc = spawn_with_provider_snapshot(
            ["env"],
            settings_path=settings_path,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        out, _ = proc.communicate(timeout=5)
        text = out.decode()

        line = next((l for l in text.splitlines()
                     if l.startswith("FNO_PROVIDER_PRICING=")), "")
        assert line, f"FNO_PROVIDER_PRICING missing in env\n{text}"
        json_part = line.split("=", 1)[1]
        pricing = _json.loads(json_part)
        assert pricing["input_per_million_usd"] == 15.0
        assert pricing["output_per_million_usd"] == 75.0

    def test_hp2_snapshot_taken_at_spawn_not_lookup(self, tmp_path: Path):
        """A swap of the active provider AFTER spawn returns must not affect
        the running subprocess's env."""
        import time
        import threading
        from fno.adapters.providers.dispatch import (
            spawn_with_provider_snapshot,
        )
        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = self._seed_settings(tmp_path, active="claude-anthropic")

        # Spawn a subprocess that prints env then sleeps briefly so we can
        # mutate settings during its lifetime.
        proc = spawn_with_provider_snapshot(
            ["sh", "-c", "echo PROV=$FNO_PROVIDER_ID; sleep 0.3; echo PROV2=$FNO_PROVIDER_ID"],
            settings_path=settings_path,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )

        # Mutate active to openrouter while subprocess is running.
        def swap():
            time.sleep(0.05)
            atomic_mutate_settings(
                lambda d: ({**d, "config": {**d["config"], "providers": {**d["config"]["providers"], "active": "claude-openrouter"}}}),
                settings_path=settings_path,
            )
        t = threading.Thread(target=swap)
        t.start()

        out, _ = proc.communicate(timeout=5)
        t.join()
        text = out.decode()

        assert "PROV=claude-anthropic" in text
        # Even after the parent's settings change, the subprocess's env still
        # carries the snapshotted id.
        assert "PROV2=claude-anthropic" in text

    def test_err1_no_active_raises_missing_active_provider(self, tmp_path: Path):
        from fno.adapters.providers.dispatch import (
            spawn_with_provider_snapshot,
        )
        from fno.adapters.providers.loader import MissingActiveProvider
        import yaml as _yaml

        settings_path = tmp_path / "settings.yaml"
        settings_path.write_text(_yaml.safe_dump({
            "config": {"providers": {"active": None, "records": []}},
        }))

        with pytest.raises(MissingActiveProvider):
            spawn_with_provider_snapshot(
                ["env"], settings_path=settings_path,
            )

    def test_edge2_grandchild_inherits_env(self, tmp_path: Path):
        """Cites what-if finding #5: 'In-flight subprocess inherits stale
        provider env.' POSIX inheritance: a grandchild process still sees
        the snapshot env. The mechanism here is the explicit env= kwarg,
        not parent inheritance, so this test verifies the env actually
        propagates to children spawned by the spawned process."""
        from fno.adapters.providers.dispatch import (
            spawn_with_provider_snapshot,
        )

        settings_path = self._seed_settings(tmp_path, active="claude-anthropic")

        # The shell child spawns a grandchild that prints the env var.
        proc = spawn_with_provider_snapshot(
            ["sh", "-c", "sh -c 'echo GRANDCHILD=$FNO_PROVIDER_ID'"],
            settings_path=settings_path,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        out, _ = proc.communicate(timeout=5)
        assert b"GRANDCHILD=claude-anthropic" in out

    def test_edge3_recursion_guard_env_preserved(self, tmp_path: Path, monkeypatch):
        """Cites EDGE3: spawn helper layering must not clobber unrelated
        env vars set by the caller (e.g., TARGET_INSIDE_DISTILL=1 from the
        recursion guard work)."""
        from fno.adapters.providers.dispatch import (
            spawn_with_provider_snapshot,
        )

        settings_path = self._seed_settings(tmp_path, active="claude-anthropic")
        monkeypatch.setenv("TARGET_INSIDE_DISTILL", "1")

        proc = spawn_with_provider_snapshot(
            ["env"],
            settings_path=settings_path,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        out, _ = proc.communicate(timeout=5)
        text = out.decode()
        # Both env contracts must coexist
        assert "TARGET_INSIDE_DISTILL=1" in text
        assert "FNO_PROVIDER_ID=claude-anthropic" in text

    def test_credential_ref_omitted_when_none(self, tmp_path: Path):
        """The cred_ref env var is only set when the snapshot has it."""
        from fno.adapters.providers.dispatch import (
            spawn_with_provider_snapshot,
        )

        # Use the oauth_dir record which has no credential_ref; verify the
        # env var is absent rather than set to "None" or empty.
        settings_path = self._seed_settings(tmp_path, active="claude-anthropic")
        proc = spawn_with_provider_snapshot(
            ["env"],
            settings_path=settings_path,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        out, _ = proc.communicate(timeout=5)
        text = out.decode()
        # FNO_PROVIDER_CRED_REF should NOT appear in env.
        for line in text.splitlines():
            assert not line.startswith("FNO_PROVIDER_CRED_REF="), line


# ---------------------------------------------------------------------------
# Fix 4: _DEFAULT_ROOT must NOT be frozen at import time
# ---------------------------------------------------------------------------


def test_dispatch_module_default_root_not_frozen_at_import(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 4: _DEFAULT_ROOT binding at import time freezes the path before monkeypatching.

    The fix removes the module-level _DEFAULT_ROOT = ... binding; the default
    must be evaluated lazily at call time so FNO_CONFIG overrides work.
    """
    # Import AFTER monkeypatching HOME so we can detect if it was frozen
    monkeypatch.setenv("HOME", str(tmp_path / "fake-home"))

    # Reload the module to simulate a fresh import with the new HOME
    import importlib
    import fno.adapters.providers.dispatch as dispatch_mod
    importlib.reload(dispatch_mod)

    # The module-level _DEFAULT_ROOT (if it still exists) should NOT contain
    # the developer's real HOME dir; it should reference the monkeypatched HOME.
    if hasattr(dispatch_mod, "_DEFAULT_ROOT"):
        default_root = dispatch_mod._DEFAULT_ROOT
        # It should NOT be a resolved absolute path starting with the real home
        # The test is: it must NOT be frozen to a path that ignores monkeypatching.
        # If it is lazy (property/function call), _DEFAULT_ROOT won't exist.
        # If it is eager, it will contain the resolved path at import time.
        # After our fix, _DEFAULT_ROOT must not exist at module level.
        pytest.fail(
            f"dispatch.py still has a module-level _DEFAULT_ROOT = {default_root!r}. "
            "This freezes the path at import time and prevents test monkeypatching. "
            "Remove the module-level binding and call _default_providers_root() lazily."
        )


def test_dispatch_env_honors_config_dir(tmp_path: Path) -> None:
    """x-d012: a record with config_dir returns CLAUDE_CONFIG_DIR regardless of
    auth, so combo/failover never dispatches it on the ambient default account."""
    record = ProviderRecord(
        id="readyrule", name="ReadyRule", cli="claude", auth="managed",
        config_dir=Path("/x/.claude-alt"),
    )
    repo_root = _write_settings(tmp_path, [record.model_dump(mode="json", exclude_none=True)])
    env = dispatch_env(record.id, repo_root=repo_root)
    assert env == {"CLAUDE_CONFIG_DIR": "/x/.claude-alt"}
