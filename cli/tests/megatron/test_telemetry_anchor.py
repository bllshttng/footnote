"""Tests for megatron telemetry anchoring (BUG-MT-008).

Before this fix, the megatron emit helpers checked
``Path(".fno").is_dir()`` relative to the process cwd. When the
operator ran ``fno megatron run`` from outside the repo (or any
directory without a ``.fno/`` sibling), every emit silently
no-op'd: mission_started, mission_complete, wave_advanced,
manifest_baselined, manifest_mutated, completion_file_corrupt,
completion_artifact_write_failed.

This test pins the contract that events are written to the repo-root's
``.fno/events.jsonl`` regardless of cwd, using
``resolve_repo_root`` (anchored via ``FNO_REPO_ROOT`` env var in tests).
"""
from __future__ import annotations

from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _reset_resolve_repo_root_cache():
    """resolve_repo_root is @cache-decorated; clear before+after each test."""
    from fno.paths import resolve_repo_root
    resolve_repo_root.cache_clear()
    yield
    resolve_repo_root.cache_clear()


def test_resolve_events_path_uses_repo_root_not_cwd(tmp_path, monkeypatch):
    """resolve_events_path returns <repo_root>/.fno/events.jsonl.

    Set FNO_REPO_ROOT to one directory, chdir to a sibling without an
    .fno/ - the helper still returns the repo_root path.
    """
    repo_root = tmp_path / "repo"
    (repo_root / ".fno").mkdir(parents=True)
    other_cwd = tmp_path / "elsewhere"
    other_cwd.mkdir()

    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))
    monkeypatch.chdir(other_cwd)

    from fno.megatron._telemetry import resolve_events_path
    result = resolve_events_path()
    assert result is not None
    assert result == repo_root / ".fno" / "events.jsonl"


def test_resolve_events_path_none_when_no_abilities_dir(tmp_path, monkeypatch):
    """When no .fno/ exists under repo_root, returns None (graceful no-op)."""
    repo_root = tmp_path / "bare-repo"
    repo_root.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))
    monkeypatch.chdir(tmp_path)

    from fno.megatron._telemetry import resolve_events_path
    assert resolve_events_path() is None


def test_emit_status_event_writes_to_repo_root_from_alien_cwd(tmp_path, monkeypatch):
    """End-to-end: update_status from outside the repo still writes the
    mission_started event to <repo_root>/.fno/events.jsonl.

    This is the load-bearing assertion for BUG-MT-008: cwd is intentionally
    pointed at a directory with NO .fno/ sibling. Pre-fix, the emit
    helper saw `Path(".fno").is_dir()` is False and silently returned.
    """
    repo_root = tmp_path / "repo"
    (repo_root / ".fno").mkdir(parents=True)
    alien_cwd = tmp_path / "alien"
    alien_cwd.mkdir()

    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))
    monkeypatch.chdir(alien_cwd)

    # Build a fleet state file under <alien>/fleet/<slug>/state.md so we
    # exercise the production code path. The mission_id must be present
    # so the mission_started event has data.
    fleet_root = alien_cwd / "fleet"
    slug_dir = fleet_root / "boot-slug"
    slug_dir.mkdir(parents=True)
    state_path = slug_dir / "state.md"
    state_path.write_text(
        "---\n"
        "mission_id: ab-anchor1\n"
        "status: pending\n"
        "created_at: 2026-05-15T07:00:00Z\n"
        "sent_msg_ids: {}\n"
        "received_completes: []\n"
        "---\n",
        encoding="utf-8",
    )

    from fno.megatron.state import update_status
    update_status(state_path, "running", fleet_root=fleet_root)

    events_path = repo_root / ".fno" / "events.jsonl"
    assert events_path.exists(), (
        "mission_started event must land under repo_root, not alien cwd"
    )

    # Sanity: cwd was alien and had no .fno/, so a cwd-relative
    # path would NOT have an events.jsonl there.
    assert not (alien_cwd / ".fno").exists()

    # Confirm the event payload type for the strictest possible regression.
    import json
    lines = events_path.read_text(encoding="utf-8").strip().splitlines()
    assert lines, "events.jsonl must contain at least one line"
    parsed = [json.loads(line) for line in lines]
    types = [e.get("type") for e in parsed]
    assert "mission_started" in types
