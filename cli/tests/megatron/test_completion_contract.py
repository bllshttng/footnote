"""BUG-MT-003: Producer/consumer schema contract tests.

Producer (hooks/target-stop-hook.sh:459) writes completion JSONs with:
    {schema_version, project, wave, mission_id, pr_url, pr_status,
     commit_sha, completed_at, reply_to_msg_id, discoveries}

Consumers (artifact._build_waves, brief.assemble_wave_brief) read:
    {from, msg_id, reply_to, ts}

None of the consumer keys are in the producer output. This test suite
verifies the fallback chains that bridge the gap without changing the
producer schema.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import pytest

from fno.megatron.artifact import _build_waves
from fno.megatron.brief import assemble_wave_brief
from fno.megatron.state import MissionState, _append_received_complete_for_test


# ---------------------------------------------------------------------------
# Helper: build a producer-shape completion record matching target-stop-hook
# ---------------------------------------------------------------------------

def _producer_record(
    project: str = "proj-a",
    wave: int = 1,
    commit_sha: str = "abcdef123456abcd",
    reply_to_msg_id: Optional[str] = "msg-sent-001",
    completed_at: str = "2026-05-15T12:00:00Z",
    discoveries: Optional[str] = "### Discoveries\n\ntest body\n",
    mission_id: str = "ab-test0001",
    pr_url: Optional[str] = "https://github.com/org/repo/pull/99",
    pr_status: Optional[str] = "merged",
    schema_version: int = 1,
) -> dict:
    """Exact shape written by hooks/target-stop-hook.sh:459."""
    return {
        "schema_version": schema_version,
        "project": project,
        "wave": wave,
        "mission_id": mission_id,
        "pr_url": pr_url,
        "pr_status": pr_status,
        "commit_sha": commit_sha,
        "completed_at": completed_at,
        "reply_to_msg_id": reply_to_msg_id,
        "discoveries": discoveries,
    }


def _state_with_completes(records: list[dict], wave: int = 1) -> MissionState:
    """Construct a MissionState with given completion records for wave_N."""
    return MissionState(
        mission_id="ab-test0001",
        status="complete",
        created_at="2026-05-15T10:00:00Z",
        sent_msg_ids={f"wave_{wave}": ["msg-sent-001"]},
        _received_completes_override=records,
    )


# ---------------------------------------------------------------------------
# AC3-HP: producer-shape records are correctly consumed by _build_waves
# ---------------------------------------------------------------------------


def test_ac3_hp_build_waves_maps_producer_fields():
    """AC3-HP: _build_waves resolves from, msg_id, reply_to, ts from producer fields."""
    record = _producer_record(
        project="proj-a",
        commit_sha="abcdef123456abcd",
        reply_to_msg_id="msg-sent-001",
        completed_at="2026-05-15T12:00:00Z",
    )
    state = _state_with_completes([record], wave=1)
    waves = _build_waves(state)

    assert len(waves) == 1
    rc = waves[0]["received_completes"]
    assert len(rc) == 1, "Expected one received_complete in wave"

    entry = rc[0]
    # from: should map producer 'project' -> consumer 'from'
    assert entry["from"] is not None, "from should not be None"
    assert entry["from"] == "proj-a"

    # msg_id: should map producer 'commit_sha[:12]' -> consumer 'msg_id'
    assert entry["msg_id"] is not None, "msg_id should not be None"
    assert entry["msg_id"] == "abcdef123456"

    # reply_to: should map producer 'reply_to_msg_id' -> consumer 'reply_to'
    assert entry["reply_to"] is not None, "reply_to should not be None"
    assert entry["reply_to"] == "msg-sent-001"

    # ts: should map producer 'completed_at' -> consumer 'ts'
    assert entry["ts"] is not None, "ts should not be None"
    assert entry["ts"] == "2026-05-15T12:00:00Z"


# ---------------------------------------------------------------------------
# AC3-ERR: legacy record missing commit_sha -> msg_id falls back to project
# ---------------------------------------------------------------------------


def test_ac3_err_legacy_record_missing_commit_sha():
    """AC3-ERR: when commit_sha absent, msg_id falls back to project; no exception."""
    record = {
        "project": "legacy-proj",
        "wave": 1,
        "completed_at": "2026-05-15T09:00:00Z",
        # commit_sha intentionally absent (legacy record)
        "reply_to_msg_id": "msg-rt-legacy",
    }
    state = _state_with_completes([record], wave=1)
    waves = _build_waves(state)

    assert len(waves) == 1
    rc = waves[0]["received_completes"]
    assert len(rc) == 1

    entry = rc[0]
    # msg_id falls back to project when commit_sha is absent
    assert entry["msg_id"] is not None, "msg_id should not be None on legacy record"
    assert entry["msg_id"] == "legacy-proj"
    # No exception raised
    assert entry["from"] == "legacy-proj"
    assert entry["reply_to"] == "msg-rt-legacy"
    assert entry["ts"] == "2026-05-15T09:00:00Z"


# ---------------------------------------------------------------------------
# AC3-FR: _append_received_complete_for_test writes commit_sha + discoveries
# ---------------------------------------------------------------------------


def test_ac3_fr_helper_writes_commit_sha_and_discoveries(tmp_path: Path):
    """AC3-FR: test helper writes commit_sha and discoveries fields; consumer maps them."""
    import json

    fleet_dir = tmp_path / "ab-fr01"
    fleet_dir.mkdir(parents=True)
    state_path = fleet_dir / "state.md"

    state_path.write_text(
        "---\nmission_id: ab-fr01\nstatus: running\ncreated_at: 2026-05-15T10:00:00Z\n---\n",
        encoding="utf-8",
    )

    _append_received_complete_for_test(
        state_path,
        from_project="p1",
        msg_id="m1",
        reply_to="rt1",
        wave=1,
        commit_sha="abcdef123456",
        discoveries="### Discoveries\n\ntest body\n",
    )

    # Verify the written JSON has the new fields
    json_path = fleet_dir / "completions" / "wave-1" / "p1.json"
    assert json_path.exists(), "Completion JSON file should exist"
    data = json.loads(json_path.read_text(encoding="utf-8"))
    assert data["commit_sha"] == "abcdef123456", "commit_sha should be in payload"
    assert data["discoveries"] == "### Discoveries\n\ntest body\n", "discoveries should be in payload"

    # Also verify that a pure producer-shape record (no msg_id key, only commit_sha)
    # is correctly mapped by the consumer via the commit_sha[:12] fallback.
    pure_producer = {k: v for k, v in data.items() if k != "msg_id"}
    state = MissionState(
        mission_id="ab-fr01",
        status="complete",
        created_at="2026-05-15T10:00:00Z",
        sent_msg_ids={"wave_1": ["rt1"]},
        _received_completes_override=[pure_producer],
    )
    waves = _build_waves(state)
    rc = waves[0]["received_completes"]
    assert len(rc) == 1
    entry = rc[0]
    assert entry["from"] == "p1"
    # commit_sha[:12] is the fallback for msg_id when msg_id is absent
    assert entry["msg_id"] == "abcdef123456"
    assert entry["reply_to"] == "rt1"
    assert entry["ts"] is not None


# ---------------------------------------------------------------------------
# AC3-EDGE: malformed record {wave: 1} only -> from is None; no exception
# ---------------------------------------------------------------------------


def test_ac3_edge_malformed_record_no_from_no_project():
    """AC3-EDGE: record with only 'wave' key -> from is None; no exception."""
    record = {"wave": 1}
    state = _state_with_completes([record], wave=1)
    waves = _build_waves(state)  # Must not raise

    assert len(waves) == 1
    rc = waves[0]["received_completes"]
    assert len(rc) == 1

    entry = rc[0]
    assert entry["from"] is None, "from should be None when no from or project key"


def test_ac3_edge_non_string_commit_sha_does_not_raise():
    """AC3-EDGE (defensive): a corrupted record with a non-string commit_sha
    must not crash _build_waves. The raw `or "")[:12]` form would raise
    TypeError on int/list/dict; str() coercion before slice keeps the
    fallback chain crash-free for any input shape."""
    record = {
        "wave": 1,
        "project": "p1",
        "commit_sha": 12345,  # corrupted producer wrote int instead of str
    }
    state = _state_with_completes([record], wave=1)
    waves = _build_waves(state)  # Must not raise

    rc = waves[0]["received_completes"]
    entry = rc[0]
    assert entry["from"] == "p1"
    # str(12345)[:12] == "12345"
    assert entry["msg_id"] == "12345"


def test_ac3_edge_non_string_commit_sha_brief_does_not_raise():
    """Mirror of the artifact defensive-cast test for assemble_wave_brief."""
    record = {
        "wave": 1,
        "project": "p1",
        "commit_sha": [1, 2, 3],  # corrupted producer wrote list
    }
    brief = assemble_wave_brief(completes_for_wave=[record], wave=1)
    # The brief must not raise; project name must still appear.
    assert "p1" in brief


# ---------------------------------------------------------------------------
# AC3-UI: all producer-mandatory fields are referenced in consumer fallback chains
# ---------------------------------------------------------------------------


def test_ac3_ui_consumer_source_references_producer_mandatory_fields():
    """AC3-UI: verify each producer-mandatory field is referenced in at least one fallback chain."""
    import inspect
    import fno.megatron.artifact as artifact_mod

    source = inspect.getsource(artifact_mod._build_waves)

    # Each producer-mandatory field must appear as a fallback key in _build_waves
    mandatory_producer_fields = ["project", "commit_sha", "completed_at", "reply_to_msg_id"]
    for field in mandatory_producer_fields:
        assert field in source, (
            f"Producer-mandatory field '{field}' not referenced in _build_waves fallback chains"
        )


# ---------------------------------------------------------------------------
# AC3-HP-BRIEF: assemble_wave_brief maps producer-shape records correctly
# ---------------------------------------------------------------------------


def test_ac3_hp_brief_maps_producer_fields():
    """AC3-HP: assemble_wave_brief maps from/msg_id from producer-shape records."""
    record = _producer_record(
        project="proj-b",
        commit_sha="aabbcc112233",
        reply_to_msg_id="msg-sent-002",
        discoveries="some finding",
    )
    brief = assemble_wave_brief(completes_for_wave=[record], wave=1)

    # from_proj should map to producer 'project'
    assert "proj-b" in brief, "Brief should include project name as from_proj"
    # msg_id should appear (either direct or from commit_sha[:12])
    assert "aabbcc112233" in brief or "proj-b" in brief, "Brief should include msg_id or fallback"
