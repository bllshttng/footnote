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
        [{"id": "readyrule", "name": "ReadyRule", "harness": "claude",
          "auth": "managed", "config_dir": str(cfg)}],
    )
    ov = resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)
    assert ov.lane == "config-dir"
    assert ov.env == {"CLAUDE_CONFIG_DIR": str(cfg)}


def test_config_dir_missing_dir_refused(tmp_path: Path, providers_root: Path) -> None:
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "ReadyRule", "harness": "claude",
          "auth": "managed", "config_dir": str(tmp_path / "nope")}],
    )
    with pytest.raises(AccountResolutionError, match="does not exist"):
        resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)


def test_config_dir_no_login_refused(tmp_path: Path, providers_root: Path) -> None:
    cfg = tmp_path / "empty-alt"
    cfg.mkdir()  # dir exists, no login material
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "ReadyRule", "harness": "claude",
          "auth": "managed", "config_dir": str(cfg)}],
    )
    # No darwin Keychain item for a throwaway tmp dir; on darwin _read_slot_blob
    # shells `security` which returns nonzero -> None. Assert refusal.
    with pytest.raises(AccountResolutionError, match="no claude login"):
        resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)


def test_config_dir_claude_json_only_refused(tmp_path: Path, providers_root: Path) -> None:
    """.claude.json is settings/metadata, present in a logged-OUT dir - it must
    NOT count as a login (else a credentialless dir spawns an auth-prompt zombie)."""
    cfg = tmp_path / "logged-out"
    cfg.mkdir()
    (cfg / ".claude.json").write_text("{}")  # metadata only, no .credentials.json
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "ReadyRule", "harness": "claude",
          "auth": "managed", "config_dir": str(cfg)}],
    )
    with pytest.raises(AccountResolutionError, match="no claude login"):
        resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)


def test_nonactive_matching_account_id_not_treated_active(
    tmp_path: Path, providers_root: Path
) -> None:
    """A non-active record whose account_id coincidentally equals the active
    slot id must NOT be treated as active (that would bill the active account)."""
    _stamp_active(providers_root, "makers")
    repo = _write_settings(
        tmp_path,
        [{"id": "readyrule", "name": "R", "harness": "claude", "auth": "managed",
          "account_id": "makers"}],  # metadata collides with the active slot id
    )
    with pytest.raises(AccountResolutionError, match="not the active"):
        resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)


# --- AC1-ERR: unknown / non-claude -----------------------------------------

def test_unknown_id_refused_lists_claude_accounts(tmp_path: Path, providers_root: Path) -> None:
    repo = _write_settings(
        tmp_path,
        [{"id": "makers", "name": "Makers", "harness": "claude", "auth": "managed"}],
    )
    with pytest.raises(AccountResolutionError, match="is not registered.*makers"):
        resolve_account_overlay("nope", repo_root=repo, providers_root=providers_root)


def test_non_claude_record_refused(tmp_path: Path, providers_root: Path) -> None:
    repo = _write_settings(
        tmp_path,
        [{"id": "codex-main", "name": "Codex", "harness": "codex", "auth": "managed"}],
    )
    with pytest.raises(AccountResolutionError, match="claude-only"):
        resolve_account_overlay("codex-main", repo_root=repo, providers_root=providers_root)


def test_route_backed_account_uses_explicit_route_overlay(
    tmp_path: Path, providers_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: account overlay and route resolver share one complete env."""
    repo = _write_settings(
        tmp_path,
        [{
            "id": "zai",
            "name": "Z.AI",
            "harness": "claude",
            "auth": "api_key",
            "route": "zai/glm-5.3[1m]",
        }],
    )
    expected = {
        "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "live-token",
        "ANTHROPIC_MODEL": "glm-5.3[1m]",
    }
    seen: list[tuple[str, str]] = []

    def resolve(provider: str, model: str, **_kwargs: object) -> dict[str, str]:
        seen.append((provider, model))
        return expected

    monkeypatch.setattr("fno.adapters.providers.dispatch.resolve_explicit_route", resolve)

    overlay = resolve_account_overlay("zai", repo_root=repo, providers_root=providers_root)

    assert overlay.lane == "api-key"
    assert overlay.env == expected
    assert seen == [("zai", "glm-5.3[1m]")]


def test_route_backed_account_refuses_unresolvable_route(
    tmp_path: Path, providers_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-ERR: unknown/keyless routes fail before launch without ambient auth."""
    repo = _write_settings(
        tmp_path,
        [{
            "id": "zai",
            "name": "Z.AI",
            "harness": "claude",
            "auth": "api_key",
            "route": "zai/glm-5.3",
        }],
    )
    monkeypatch.setattr(
        "fno.adapters.providers.dispatch.resolve_explicit_route",
        lambda *_a, **_k: None,
    )

    with pytest.raises(AccountResolutionError, match="route.*unavailable"):
        resolve_account_overlay("zai", repo_root=repo, providers_root=providers_root)


# --- lane 3: managed active rides the shared slot ---------------------------

def test_managed_active_pins_shared_slot(tmp_path: Path, providers_root: Path) -> None:
    """Lane 3 pins CLAUDE_CONFIG_DIR to ~/.claude (not {}), so a stale parent
    CLAUDE_CONFIG_DIR export can't leak and bill the wrong account."""
    _stamp_active(providers_root, "makers")
    repo = _write_settings(
        tmp_path,
        [{"id": "makers", "name": "Makers", "harness": "claude", "auth": "managed"}],
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
        [{"id": "readyrule", "name": "ReadyRule", "harness": "claude", "auth": "managed"}],
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
        [{"id": "readyrule", "name": "R", "harness": "claude", "auth": "managed",
          "config_dir": str(cfg)}],
    )
    ov = resolve_account_overlay("readyrule", repo_root=repo, providers_root=providers_root)
    assert ov.lane == "config-dir"
    assert ov.env == {"CLAUDE_CONFIG_DIR": str(cfg)}


def test_mesh_env_wrapper_scrubs_and_pins(tmp_path: Path) -> None:
    """The pane seam adds `env -u` for each scrubbed auth var and sets the
    account's CLAUDE_CONFIG_DIR (x-d012 P1: no ambient-token override)."""
    from fno.agents.mux_spawn import _mesh_env_wrapper
    from fno.agents.account_env import SCRUB_AUTH_VARS

    argv = _mesh_env_wrapper(
        "w1", "claude", None, ["claude", "hi"],
        account_env={"CLAUDE_CONFIG_DIR": "/x/.claude-alt"},
    )
    joined = " ".join(argv)
    for var in SCRUB_AUTH_VARS:
        assert f"-u {var}" in joined
    assert "CLAUDE_CONFIG_DIR=/x/.claude-alt" in argv


def test_mesh_env_wrapper_sets_block_cap(monkeypatch) -> None:
    """The pane seam raises the claude Stop-hook block cap, honoring override (x-1680)."""
    from fno.agents.mux_spawn import _mesh_env_wrapper

    monkeypatch.delenv("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", raising=False)
    argv = _mesh_env_wrapper("w1", "claude", None, ["claude", "hi"])
    assert "CLAUDE_CODE_STOP_HOOK_BLOCK_CAP=50" in argv

    # An explicit operator value wins over the fno default.
    monkeypatch.setenv("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP", "99")
    argv = _mesh_env_wrapper("w2", "claude", None, ["claude", "hi"])
    assert "CLAUDE_CODE_STOP_HOOK_BLOCK_CAP=99" in argv

    # Non-claude providers are not touched.
    argv = _mesh_env_wrapper("w3", "codex", None, ["codex", "hi"])
    assert not any(str(a).startswith("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP=") for a in argv)
