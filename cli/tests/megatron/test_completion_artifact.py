"""Tests for write_mission_complete - the filesystem-aggregating completion artifact.

Covers:
  AC4-HP  - 2-wave 2-project mission produces correct artifact (headings + PR rows)
  AC4-ERR - status flip still succeeds even if artifact write raises
  AC4-EDGE - missing completion JSON for one project renders gracefully
  AC4-FR (ledger absent) - cost renders as "unknown" when ledger.json absent
  AC4-FR (atomic) - no leftover .tmp file after success
  AC4-FR (idempotency) - second call with same inputs produces byte-identical output
"""
from __future__ import annotations

import json
import textwrap
from pathlib import Path
from typing import Optional

import pytest
import yaml


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _write_manifest(fleet_dir: Path, mission_id: str, waves_spec: list) -> Path:
    """Write a minimal 00-INDEX.md manifest with the given waves.

    waves_spec is a list of dicts like [{"wave": 1, "projects": ["a", "b"]}, ...]
    """
    lines = [
        "---",
        f"mission_id: {mission_id}",
        "mission_type: fleet",
        "waves:",
    ]
    for w in waves_spec:
        lines.append(f"  - wave: {w['wave']}")
        lines.append("    projects:")
        for p in w["projects"]:
            lines.append(f"      - name: {p}")
            lines.append(f"        body: ''")
    lines.append("---")
    lines.append("")
    content = "\n".join(lines)
    manifest_path = fleet_dir / "00-INDEX.md"
    manifest_path.write_text(content, encoding="utf-8")
    return manifest_path


def _write_state(fleet_dir: Path, mission_id: str, status: str = "complete",
                 created_at: str = "2026-05-13T10:00:00Z") -> Path:
    content = textwrap.dedent(f"""\
        ---
        mission_id: {mission_id}
        status: {status}
        created_at: {created_at}
        sent_msg_ids: {{}}
        received_completes: []
        ---
        """)
    state_path = fleet_dir / "state.md"
    state_path.write_text(content, encoding="utf-8")
    return state_path


def _write_completion(fleet_dir: Path, wave: int, project: str,
                      pr_url: str = "https://github.com/org/repo/pull/1",
                      commit_sha: str = "abc1234",
                      completed_at: str = "2026-05-13T11:00:00Z") -> Path:
    comp_dir = fleet_dir / "completions" / f"wave-{wave}"
    comp_dir.mkdir(parents=True, exist_ok=True)
    payload = {
        "project": project,
        "wave": wave,
        "mission_id": "test-mission",
        "pr_url": pr_url,
        "pr_status": "open",
        "commit_sha": commit_sha,
        "completed_at": completed_at,
        "reply_to_msg_id": None,
    }
    path = comp_dir / f"{project}.json"
    path.write_text(json.dumps(payload), encoding="utf-8")
    return path


def _write_ledger(path: Path, entries: list) -> None:
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")


# ---------------------------------------------------------------------------
# AC4-HP: 2-wave 2-project mission produces correct artifact
# ---------------------------------------------------------------------------


def test_two_wave_two_project_produces_correct_artifact(tmp_path: Path):
    """Given 2 waves with 2 projects each, write_mission_complete produces
    an artifact with 2 wave headings and 4 PR rows."""
    from fno.megatron.artifact import write_mission_complete

    mission_id = "ab-test001"
    manifest_path = _write_manifest(
        tmp_path, mission_id,
        [
            {"wave": 1, "projects": ["alpha", "beta"]},
            {"wave": 2, "projects": ["gamma", "delta"]},
        ]
    )
    state_path = _write_state(tmp_path, mission_id)

    _write_completion(tmp_path, 1, "alpha", pr_url="https://github.com/org/repo/pull/10")
    _write_completion(tmp_path, 1, "beta",  pr_url="https://github.com/org/repo/pull/11")
    _write_completion(tmp_path, 2, "gamma", pr_url="https://github.com/org/repo/pull/20",
                      completed_at="2026-05-13T12:00:00Z")
    _write_completion(tmp_path, 2, "delta", pr_url="https://github.com/org/repo/pull/21",
                      completed_at="2026-05-13T12:30:00Z")

    artifact_path = write_mission_complete(
        manifest_path=manifest_path,
        state_path=state_path,
        fleet_dir=tmp_path,
    )

    assert artifact_path.exists()
    content = artifact_path.read_text(encoding="utf-8")

    # Frontmatter checks
    fm_text = content.split("---")[1]
    fm = yaml.safe_load(fm_text)
    assert fm["mission_id"] == mission_id
    assert fm["waves_completed"] == 2
    assert fm["project_count"] == 4
    assert fm["mission_status"] == "complete"

    # Body checks: 2 wave headings
    assert "## Wave 1" in content
    assert "## Wave 2" in content

    # 4 PR rows (one per project)
    assert "https://github.com/org/repo/pull/10" in content
    assert "https://github.com/org/repo/pull/11" in content
    assert "https://github.com/org/repo/pull/20" in content
    assert "https://github.com/org/repo/pull/21" in content


# ---------------------------------------------------------------------------
# AC4-ERR: status flip succeeds even if artifact write raises; event written
# ---------------------------------------------------------------------------


def test_artifact_write_failure_does_not_block_status_flip(tmp_path: Path, monkeypatch):
    """If write_mission_complete raises, update_status must still succeed
    and emit a completion_artifact_write_failed event."""
    from fno.megatron import state as state_mod

    mission_id = "ab-errtest"
    state_path = tmp_path / "state.md"
    _write_state(tmp_path, mission_id, status="running")

    # Make write_mission_complete raise
    monkeypatch.setattr(
        "fno.megatron.artifact.write_mission_complete",
        lambda **kw: (_ for _ in ()).throw(OSError("simulated disk full")),
    )

    # Also make .fno dir so the event emit is attempted
    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir()
    events_file = abilities_dir / "events.jsonl"
    events_file.write_text("", encoding="utf-8")

    original_cwd = Path.cwd()
    import os
    try:
        os.chdir(tmp_path)
        from fno.megatron.state import update_status
        update_status(state_path, "complete")
    finally:
        os.chdir(original_cwd)

    # State file must reflect complete
    from fno.megatron.state import read_state
    final_state = read_state(state_path)
    assert final_state.status == "complete"

    # events.jsonl must contain completion_artifact_write_failed
    events_text = events_file.read_text(encoding="utf-8")
    events = [json.loads(line) for line in events_text.strip().splitlines() if line.strip()]
    artifact_fail_events = [e for e in events if e.get("type") == "completion_artifact_write_failed"]
    assert len(artifact_fail_events) >= 1
    event_data = artifact_fail_events[0].get("data", {})
    assert "disk full" in event_data.get("reason", "")


# ---------------------------------------------------------------------------
# AC4-EDGE: missing completion JSON for one project renders gracefully
# ---------------------------------------------------------------------------


def test_missing_completion_json_renders_gracefully(tmp_path: Path):
    """If a completion JSON file is absent for one project, that project row
    reads '(completion file not found)' rather than crashing."""
    from fno.megatron.artifact import write_mission_complete

    mission_id = "ab-edgetest"
    manifest_path = _write_manifest(
        tmp_path, mission_id,
        [{"wave": 1, "projects": ["present", "absent"]}]
    )
    state_path = _write_state(tmp_path, mission_id)

    # Only write completion for "present", not "absent"
    _write_completion(tmp_path, 1, "present")

    artifact_path = write_mission_complete(
        manifest_path=manifest_path,
        state_path=state_path,
        fleet_dir=tmp_path,
    )

    content = artifact_path.read_text(encoding="utf-8")
    assert "(completion file not found)" in content
    # "present" still appears normally
    assert "present" in content


# ---------------------------------------------------------------------------
# AC4-FR (ledger absent): cost renders as "unknown" when ledger absent
# ---------------------------------------------------------------------------


def test_cost_unknown_when_ledger_absent(tmp_path: Path):
    """When ledger_path is None and no default ledger exists, cost renders as 'unknown'."""
    from fno.megatron.artifact import write_mission_complete

    mission_id = "ab-nocost"
    manifest_path = _write_manifest(
        tmp_path, mission_id,
        [{"wave": 1, "projects": ["alpha"]}]
    )
    state_path = _write_state(tmp_path, mission_id)
    _write_completion(tmp_path, 1, "alpha")

    artifact_path = write_mission_complete(
        manifest_path=manifest_path,
        state_path=state_path,
        fleet_dir=tmp_path,
        ledger_path=Path(tmp_path / "nonexistent-ledger.json"),
    )

    content = artifact_path.read_text(encoding="utf-8")
    assert "unknown" in content


# ---------------------------------------------------------------------------
# AC4-FR (atomic): no leftover .tmp after success
# ---------------------------------------------------------------------------


def test_atomic_write_leaves_no_tempfile(tmp_path: Path):
    """Successful write must not leave any .tmp sibling on disk."""
    from fno.megatron.artifact import write_mission_complete

    mission_id = "ab-atomic"
    manifest_path = _write_manifest(
        tmp_path, mission_id,
        [{"wave": 1, "projects": ["x"]}]
    )
    state_path = _write_state(tmp_path, mission_id)
    _write_completion(tmp_path, 1, "x")

    write_mission_complete(
        manifest_path=manifest_path,
        state_path=state_path,
        fleet_dir=tmp_path,
    )

    leftovers = list(tmp_path.glob("*.tmp"))
    assert leftovers == []


# ---------------------------------------------------------------------------
# AC4-FR (idempotency): second call produces byte-identical artifact
# ---------------------------------------------------------------------------


def test_idempotent_second_call(tmp_path: Path):
    """Calling write_mission_complete twice with the same inputs produces
    byte-identical output (atomic overwrite, no duplication)."""
    from fno.megatron.artifact import write_mission_complete

    mission_id = "ab-idem"
    manifest_path = _write_manifest(
        tmp_path, mission_id,
        [{"wave": 1, "projects": ["a", "b"]}]
    )
    state_path = _write_state(tmp_path, mission_id)
    _write_completion(tmp_path, 1, "a", completed_at="2026-05-13T11:00:00Z")
    _write_completion(tmp_path, 1, "b", completed_at="2026-05-13T11:05:00Z")

    path1 = write_mission_complete(
        manifest_path=manifest_path,
        state_path=state_path,
        fleet_dir=tmp_path,
    )
    content1 = path1.read_text(encoding="utf-8")

    path2 = write_mission_complete(
        manifest_path=manifest_path,
        state_path=state_path,
        fleet_dir=tmp_path,
    )
    content2 = path2.read_text(encoding="utf-8")

    assert content1 == content2
