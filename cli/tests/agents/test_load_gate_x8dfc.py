"""x-8dfc load-gate relaxation: the registry read tolerates any well-shaped
identity token (provider OR harness) so one alien harness never bricks the
shared read, while genuine corruption still raises. Capability (can THIS fno
dispatch the row?) moves to the spawn/ask seam.

The Rust half of the cross-language parity (AC1-FR) is
``client_verbs.rs::load_registry_gate_shape_check_x8dfc``: both readers accept
the same alien-harness fixture and refuse the same corrupt fixture.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _write_registry(tmp_path: Path, rows: list[dict], version: int = 9) -> Path:
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": version, "agents": rows}), encoding="utf-8"
    )
    return registry_path


def test_ac2_hp_alien_harness_row_loads(tmp_path: Path, monkeypatch) -> None:
    """AC2-HP: an alien harness row loads (no RegistryVersionError) and lists."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    reg = _write_registry(
        tmp_path,
        [
            {
                "name": "nh",
                "provider": "newharness",
                "harness": "newharness",
                "harness_session_id": "deadbeefcafef00d",
                "cwd": "/tmp",
                "log_path": "/tmp/nh.log",
                "status": "live",
            }
        ],
    )
    entries = load_registry(path=reg)
    assert len(entries) == 1
    assert entries[0].harness == "newharness"


def test_ac2_hp_alien_harness_absent_from_live_discovery(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP: an alien harness has no live transport, so it never resolves as a
    live discovery target (it stays durably mail-routable elsewhere)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.discover import _discover_from_registry

    reg = _write_registry(
        tmp_path,
        [
            {
                "name": "nh",
                "provider": "newharness",
                "harness": "newharness",
                "harness_session_id": "deadbeefcafef00d",
                "cwd": "/tmp",
                "log_path": "/tmp/nh.log",
                "status": "live",
            }
        ],
    )
    assert _discover_from_registry(registry_path=reg) == []


def test_ac1_err_empty_identity_bricks(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR: an empty provider AND no harness is corruption, still raises."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    reg = _write_registry(
        tmp_path,
        [{"name": "bad", "provider": "", "cwd": "/tmp", "log_path": "/l", "status": "live"}],
    )
    with pytest.raises(RegistryVersionError, match="no valid identity"):
        load_registry(path=reg)


def test_ac1_err_non_string_identity_bricks(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR: a non-string provider with no harness is corruption."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    reg = _write_registry(
        tmp_path,
        [{"name": "bad", "provider": 7, "cwd": "/tmp", "log_path": "/l", "status": "live"}],
    )
    with pytest.raises(RegistryVersionError, match="no valid identity"):
        load_registry(path=reg)


def test_ac1_err_whitespace_identity_bricks(tmp_path: Path, monkeypatch) -> None:
    """A whitespace-bearing token is corruption, not an alien harness."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    reg = _write_registry(
        tmp_path,
        [{"name": "ws", "provider": "a b", "cwd": "/tmp", "log_path": "/l", "status": "live"}],
    )
    with pytest.raises(RegistryVersionError, match="no valid identity"):
        load_registry(path=reg)


def test_ac2_err_divergence_warns_and_harness_wins(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC2-ERR: provider != harness loads with a loud warning; harness wins."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    reg = _write_registry(
        tmp_path,
        [
            {
                "name": "dv",
                "provider": "claude",
                "harness": "codex",
                "codex_session_id": "cx-123",
                "cwd": "/tmp",
                "log_path": "/l",
                "status": "live",
            }
        ],
    )
    entries = load_registry(path=reg)
    assert len(entries) == 1
    warn = capsys.readouterr().err
    assert "diverged" in warn and "'dv'" in warn
    # harness wins for identity: session_id resolves via the codex field.
    assert entries[0].session_id == "cx-123"


def test_ac1_edge_provider_less_row_loads(tmp_path: Path, monkeypatch) -> None:
    """AC1-EDGE: a harness-only row (post-v10 writer shape) loads with provider
    backfilled from harness, so AgentEntry's required provider is populated."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    reg = _write_registry(
        tmp_path,
        [
            {
                "name": "pv",
                "harness": "claude",
                "harness_session_id": "aaaabbbbccccdddd",
                "short_id": "aaaabbbb",
                "cwd": "/tmp",
                "log_path": "/l",
                "status": "live",
            }
        ],
    )
    entries = load_registry(path=reg)
    assert len(entries) == 1
    assert entries[0].provider == "claude"
    assert entries[0].harness == "claude"
