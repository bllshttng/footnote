"""Tests for per-spawn account overlay resolution (x-d012, US2).

Hermetic: never reads the real provider store or Keychain. Settings are
written to a tmp repo_root; the active-slot stamp is a file under a tmp
providers_root; config-dir logins are on-disk .credentials.json so no
`security` call.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import yaml

from fno.agents.account_env import (
    AccountResolutionError,
    resolve_account_overlay,
)


def _write_settings(tmp_path: Path, records: list[dict]) -> Path:
    settings = {"config": {"providers": {"records": records}}}
    d = tmp_path / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    (d / "settings.yaml").write_text(yaml.safe_dump(settings))
    return tmp_path


@pytest.fixture()
def providers_root(tmp_path: Path) -> Path:
    root = tmp_path / "providers"
    root.mkdir()
    return root


def _stamp_active(providers_root: Path, record_id: str) -> None:
    (providers_root / ".active-claude").write_text(record_id)


# --- AC1-HP: config_dir (lane 2) -------------------------------------------

def test_config_dir_lane(tmp_path: Path, providers_root: Path) -> None:
    cfg = tmp_path / "claude-alt"
    cfg.mkdir()
    (cfg / ".credentials.json").write_text("{}")  # login present on disk
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "ReadyRule", "cli": "claude",
          "auth": "managed", "config_dir": str(cfg)}],
    )
    ov = resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)
    assert ov.lane == "config-dir"
    assert ov.env == {"CLAUDE_CONFIG_DIR": str(cfg)}


def test_config_dir_missing_dir_refused(tmp_path: Path, providers_root: Path) -> None:
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "ReadyRule", "cli": "claude",
          "auth": "managed", "config_dir": str(tmp_path / "nope")}],
    )
    with pytest.raises(AccountResolutionError, match="does not exist"):
        resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)


def test_config_dir_no_login_refused(tmp_path: Path, providers_root: Path) -> None:
    cfg = tmp_path / "empty-alt"
    cfg.mkdir()  # dir exists, no login material
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "ReadyRule", "cli": "claude",
          "auth": "managed", "config_dir": str(cfg)}],
    )
    # No darwin Keychain item for a throwaway tmp dir; on darwin _read_slot_blob
    # shells `security` which returns nonzero -> None. Assert refusal.
    with pytest.raises(AccountResolutionError, match="no claude login"):
        resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)


# --- AC1-ERR: unknown / non-claude -----------------------------------------

def test_unknown_id_refused_lists_claude_accounts(tmp_path: Path, providers_root: Path) -> None:
    repo = _write_settings(
        tmp_path,
        [{"id": "makers", "name": "Makers", "cli": "claude", "auth": "managed"}],
    )
    with pytest.raises(AccountResolutionError, match="not a registered provider.*makers"):
        resolve_account_overlay("nope", repo_root=repo, providers_root=providers_root)


def test_non_claude_record_refused(tmp_path: Path, providers_root: Path) -> None:
    repo = _write_settings(
        tmp_path,
        [{"id": "codex-main", "name": "Codex", "cli": "codex", "auth": "managed"}],
    )
    with pytest.raises(AccountResolutionError, match="claude-only"):
        resolve_account_overlay("codex-main", repo_root=repo, providers_root=providers_root)


# --- lane 3: managed active rides the shared slot ---------------------------

def test_managed_active_pins_shared_slot(tmp_path: Path, providers_root: Path) -> None:
    """Lane 3 pins CLAUDE_CONFIG_DIR to ~/.claude (not {}), so a stale parent
    CLAUDE_CONFIG_DIR export can't leak and bill the wrong account."""
    _stamp_active(providers_root, "makers")
    repo = _write_settings(
        tmp_path,
        [{"id": "makers", "name": "Makers", "cli": "claude", "auth": "managed"}],
    )
    ov = resolve_account_overlay("makers", repo_root=repo, providers_root=providers_root)
    assert ov.lane == "managed-active"
    assert ov.env == {"CLAUDE_CONFIG_DIR": str(Path.home() / ".claude")}


# --- managed non-active: refuse, point at config-dir (the correct mechanism) --

def test_managed_nonactive_refused_points_at_config_dir(
    tmp_path: Path, providers_root: Path
) -> None:
    """A managed non-active account has no correct overlay (a setup-token bills
    the wrong account); refuse with a config-dir pointer, never inject a token."""
    _stamp_active(providers_root, "makers")
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "ReadyRule", "cli": "claude", "auth": "managed"}],
    )
    with pytest.raises(AccountResolutionError) as exc:
        resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)
    msg = str(exc.value)
    assert "config dir" in msg and "bills the wrong account" in msg
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in msg  # never suggests the broken lane


def test_config_dir_wins_over_managed_nonactive(
    tmp_path: Path, providers_root: Path
) -> None:
    """A managed account WITH a config_dir uses its own dir (lane 2), even when
    it is not the active slot occupant - the config-dir mechanism is primary."""
    _stamp_active(providers_root, "makers")
    cfg = tmp_path / "ryr-dir"
    cfg.mkdir()
    (cfg / ".credentials.json").write_text("{}")
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "R", "cli": "claude", "auth": "managed",
          "config_dir": str(cfg)}],
    )
    ov = resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)
    assert ov.lane == "config-dir"
    assert ov.env == {"CLAUDE_CONFIG_DIR": str(cfg)}
