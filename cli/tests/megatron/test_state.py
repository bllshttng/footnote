"""Megatron Phase 2 Task 2.2: mission state file + filelock tests."""
from __future__ import annotations

import json
import threading
from pathlib import Path

import pytest


def _make_state_file(path: Path, status: str = "running") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"---\n"
        f"mission_id: ab-deadbeef\n"
        f"status: {status}\n"
        f"created_at: 2026-05-06T13:00:00Z\n"
        f"sent_msg_ids:\n"
        f"  wave_1: [msg-a4f1b2]\n"
        f"received_completes: []\n"
        f"---\n\n"
        f"# Mission state\n",
        encoding="utf-8",
    )


# ---------------------------------------------------------------------------
# AC1-HP: read/write round-trip via filelock
# ---------------------------------------------------------------------------

def test_read_write_round_trip(tmp_path):
    from fno.megatron import read_state, write_state

    state_path = tmp_path / "state.md"
    fleet_root = state_path.parent.parent
    _make_state_file(state_path)

    state = read_state(state_path, fleet_root=fleet_root)
    assert state.mission_id == "ab-deadbeef"
    assert state.status == "running"
    assert state.sent_msg_ids == {"wave_1": ["msg-a4f1b2"]}
    assert state.received_completes == []  # no completion files yet

    # Write a completion JSON file to the fleet dir so read_state rebuilds it
    completions_dir = state_path.parent / "completions" / "wave-1"
    completions_dir.mkdir(parents=True, exist_ok=True)
    payload = {
        "project": "backend",
        "wave": 1,
        "mission_id": "ab-deadbeef",
        "pr_url": None,
        "pr_status": None,
        "commit_sha": None,
        "completed_at": "2026-05-13T10:00:00Z",
        "reply_to_msg_id": None,
        "msg_id": "msg-x1",
        "from": "backend",
    }
    (completions_dir / "backend.json").write_text(json.dumps(payload), encoding="utf-8")

    write_state(state_path, state)

    re_read = read_state(state_path, fleet_root=fleet_root)
    assert len(re_read.received_completes) == 1
    assert re_read.received_completes[0]["project"] == "backend"


# ---------------------------------------------------------------------------
# AC2-ERR: corrupt frontmatter is renamed and raises
# ---------------------------------------------------------------------------

def test_corrupt_frontmatter_backed_up(tmp_path):
    from fno.megatron import read_state, MissionStateCorrupt

    state_path = tmp_path / "state.md"
    state_path.write_text(
        "---\nmission_id: ab-broken\n  bad_indent: [\n---\n",
        encoding="utf-8",
    )

    with pytest.raises(MissionStateCorrupt):
        read_state(state_path)

    assert (tmp_path / "state.md.bak").exists()
    # Original moved out of the way; tests assert the .bak captures the corrupt content
    assert "bad_indent" in (tmp_path / "state.md.bak").read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# AC4-EDGE: monotonicity check rejects backwards transitions
# ---------------------------------------------------------------------------

def test_monotonicity_rejects_running_to_pending(tmp_path):
    from fno.megatron import read_state, write_state, MissionStateRegression

    state_path = tmp_path / "state.md"
    _make_state_file(state_path, status="running")

    state = read_state(state_path)
    state.status = "pending"

    with pytest.raises(MissionStateRegression):
        write_state(state_path, state)


def test_monotonicity_allows_running_to_paused(tmp_path):
    from fno.megatron import read_state, write_state

    state_path = tmp_path / "state.md"
    _make_state_file(state_path, status="running")

    state = read_state(state_path)
    state.status = "paused"
    write_state(state_path, state)

    re_read = read_state(state_path)
    assert re_read.status == "paused"


def test_monotonicity_allows_paused_to_running(tmp_path):
    from fno.megatron import read_state, write_state

    state_path = tmp_path / "state.md"
    _make_state_file(state_path, status="paused")

    state = read_state(state_path)
    state.status = "running"
    write_state(state_path, state)

    assert read_state(state_path).status == "running"


def test_monotonicity_rejects_complete_to_running(tmp_path):
    from fno.megatron import read_state, write_state, MissionStateRegression

    state_path = tmp_path / "state.md"
    _make_state_file(state_path, status="complete")

    state = read_state(state_path)
    state.status = "running"

    with pytest.raises(MissionStateRegression):
        write_state(state_path, state)


# ---------------------------------------------------------------------------
# AC4-EDGE: append_sent_msg_id is order-preserving and durable
# ---------------------------------------------------------------------------

def test_append_sent_msg_id_preserves_order(tmp_path):
    from fno.megatron import append_sent_msg_id, read_state

    state_path = tmp_path / "state.md"
    _make_state_file(state_path)

    append_sent_msg_id(state_path, wave=2, msg_id="msg-X")
    append_sent_msg_id(state_path, wave=2, msg_id="msg-Y")
    append_sent_msg_id(state_path, wave=2, msg_id="msg-Z")

    state = read_state(state_path)
    assert state.sent_msg_ids["wave_2"] == ["msg-X", "msg-Y", "msg-Z"]
    assert state.sent_msg_ids["wave_1"] == ["msg-a4f1b2"]


# ---------------------------------------------------------------------------
# AC4-EDGE: concurrent writes serialize via filelock
# ---------------------------------------------------------------------------

def test_concurrent_appends_serialize(tmp_path):
    from fno.megatron import append_sent_msg_id, read_state

    state_path = tmp_path / "state.md"
    _make_state_file(state_path)

    barrier = threading.Barrier(8)
    errors: list[Exception] = []

    def worker(idx: int):
        barrier.wait()
        try:
            append_sent_msg_id(state_path, wave=3, msg_id=f"msg-w{idx:02d}")
        except Exception as exc:  # pragma: no cover - reported via errors list
            errors.append(exc)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert not errors, f"unexpected errors: {errors}"

    state = read_state(state_path)
    msgs = sorted(state.sent_msg_ids["wave_3"])
    assert msgs == [f"msg-w{i:02d}" for i in range(8)]


# ---------------------------------------------------------------------------
# Manifest immutability fields: update_state_field + back-compat parsing
# ---------------------------------------------------------------------------

def test_update_state_field_stamps_sha(tmp_path):
    """AC2-HP component: update_state_field writes the field under lock."""
    from fno.megatron import read_state, write_state
    from fno.megatron.state import MissionState, update_state_field

    state_path = tmp_path / "state.md"
    write_state(state_path, MissionState(mission_id="ab-test1", status="pending"))
    update_state_field(state_path, "manifest_sha256", "deadbeef" * 8)
    s = read_state(state_path)
    assert s.manifest_sha256 == "deadbeef" * 8


def test_update_state_field_rejects_unknown_field(tmp_path):
    """update_state_field allowlist prevents arbitrary field writes."""
    from fno.megatron import write_state
    from fno.megatron.state import (
        MissionState,
        MissionStateError,
        update_state_field,
    )

    state_path = tmp_path / "state.md"
    write_state(state_path, MissionState(mission_id="ab-test2", status="pending"))
    with pytest.raises(MissionStateError):
        update_state_field(state_path, "status", "running")


def test_state_md_back_compat_without_sha_fields(tmp_path):
    """Pre-rev state.md (no manifest_sha256 fields) parses with both as None."""
    state_path = tmp_path / "state.md"
    state_path.write_text(
        "---\nmission_id: ab-old\nstatus: running\n---\n", encoding="utf-8"
    )
    from fno.megatron import read_state

    s = read_state(state_path)
    assert s.manifest_sha256 is None
    assert s.manifest_sha256_first_set_at is None


def test_stamp_manifest_sha_writes_both_fields_atomically(tmp_path):
    """stamp_manifest_sha writes sha + first_set_at under one filelock.

    Verifies the joint invariant: a successful call leaves BOTH fields
    non-None, never one-set-one-None (the partial-write window the helper
    is designed to eliminate).
    """
    from fno.megatron import read_state, write_state
    from fno.megatron.state import MissionState, stamp_manifest_sha

    state_path = tmp_path / "state.md"
    write_state(state_path, MissionState(mission_id="ab-stamp1", status="pending"))

    stamp_manifest_sha(state_path, "abc123" * 8 + "deadbe", "2026-05-13T10:00:00Z")
    s = read_state(state_path)
    assert s.manifest_sha256 == "abc123" * 8 + "deadbe"
    assert s.manifest_sha256_first_set_at == "2026-05-13T10:00:00Z"
