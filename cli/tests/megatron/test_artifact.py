"""Unit tests for the mission-completion artifact builder + path constructor.

Pure-function tests: no disk writes, no manifest loads beyond the test
fixtures we hand-roll. Disk + integration coverage lives in
``test_artifact_integration.py``.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import yaml

from fno.megatron.artifact import (
    build_mission_artifact,
    mission_artifact_path,
)
from fno.megatron.manifest import Budget, Manifest, Project, Wave
from fno.megatron.state import MissionState


# ---------------------------------------------------------------------------
# AC4-EDGE: path constructor is deterministic and stable
# ---------------------------------------------------------------------------


def test_path_is_deterministic(tmp_path: Path):
    target = mission_artifact_path(tmp_path, "ab-mm9001")
    assert target == tmp_path / "mission-complete-ab-mm9001.md"
    # Stable across calls
    assert mission_artifact_path(tmp_path, "ab-mm9001") == target


# ---------------------------------------------------------------------------
# AC1-HP: build a complete artifact for a 2-wave mission
# ---------------------------------------------------------------------------


def _two_wave_state(mission_id: str = "ab-mm0010") -> MissionState:
    return MissionState(
        mission_id=mission_id,
        status="complete",
        created_at="2026-05-07T12:00:00Z",
        sent_msg_ids={
            "wave_1": ["msg-w1a", "msg-w1b"],
            "wave_2": ["msg-w2a"],
        },
        _received_completes_override=[
            {"wave": 1, "from": "backend", "msg_id": "msg-cb1a", "reply_to": "msg-w1a"},
            {"wave": 1, "from": "frontend", "msg_id": "msg-cb1b", "reply_to": "msg-w1b"},
            {"wave": 2, "from": "docs", "msg_id": "msg-cb2a", "reply_to": "msg-w2a"},
        ],
        slug="fleet-2026-05-07-cool-mission",
    )


def _two_wave_manifest(mission_id: str = "ab-mm0010") -> Manifest:
    return Manifest(
        mission_id=mission_id,
        mission_type="fleet",
        waves=[
            Wave(
                wave=1,
                projects=[
                    Project(name="backend", body="ship the region feature"),
                    Project(name="frontend", body="render new view"),
                ],
            ),
            Wave(
                wave=2,
                projects=[Project(name="docs", body="document the change")],
            ),
        ],
        budget=Budget(),
        extra={"title": "Build the cool mission"},
    )


def test_build_complete_with_two_waves():
    state = _two_wave_state()
    manifest = _two_wave_manifest()
    completed_at = "2026-05-07T13:00:00Z"

    rendered = build_mission_artifact(state, manifest, completed_at=completed_at)
    assert rendered.startswith("---\n")
    fm_text = rendered.split("---\n", 2)[1]
    fm = yaml.safe_load(fm_text)

    assert fm["type"] == "mission-complete"
    assert fm["mission_id"] == "ab-mm0010"
    assert fm["status"] == "complete"
    assert fm["created_at"] == "2026-05-07T12:00:00Z"
    assert fm["completed_at"] == completed_at
    assert fm["total_waves_planned"] == 2
    assert fm["total_waves_advanced"] == 2
    assert fm["projects"] == ["backend", "docs", "frontend"]
    assert fm["total_dispatched"] == 3
    assert fm["total_received"] == 3
    assert fm["paused_reason"] is None
    assert fm["slug"] == "fleet-2026-05-07-cool-mission"

    waves = fm["waves"]
    assert len(waves) == 2
    assert waves[0]["wave"] == 1
    assert waves[0]["sent_msg_ids"] == ["msg-w1a", "msg-w1b"]
    assert {c["from"] for c in waves[0]["received_completes"]} == {"backend", "frontend"}
    assert waves[1]["wave"] == 2
    assert waves[1]["sent_msg_ids"] == ["msg-w2a"]
    assert waves[1]["received_completes"][0]["from"] == "docs"


# ---------------------------------------------------------------------------
# AC4-EDGE: cancelled mission with zero waves advanced
# ---------------------------------------------------------------------------


def test_build_cancelled_zero_waves():
    state = MissionState(
        mission_id="ab-empty",
        status="cancelled",
        created_at="2026-05-07T10:00:00Z",
        sent_msg_ids={},
        _received_completes_override=[],
    )
    rendered = build_mission_artifact(
        state, manifest=None, completed_at="2026-05-07T10:30:00Z"
    )
    fm = yaml.safe_load(rendered.split("---\n", 2)[1])
    assert fm["status"] == "cancelled"
    assert fm["waves"] == []
    assert fm["total_dispatched"] == 0
    assert fm["total_received"] == 0
    assert fm["total_waves_advanced"] == 0
    # Manifest is None -> manifest-derived fields are None
    assert fm["projects"] is None
    assert fm["total_waves_planned"] is None


# ---------------------------------------------------------------------------
# AC4-EDGE: failed mission carries paused_reason through
# ---------------------------------------------------------------------------


def test_build_failed_with_paused_reason():
    state = MissionState(
        mission_id="ab-broken",
        status="failed",
        created_at="2026-05-07T10:00:00Z",
        paused_reason="dispatch-error: backend unreachable",
        sent_msg_ids={"wave_1": ["msg-x"]},
        _received_completes_override=[],
    )
    rendered = build_mission_artifact(state, manifest=None)
    fm = yaml.safe_load(rendered.split("---\n", 2)[1])
    assert fm["status"] == "failed"
    assert fm["paused_reason"] == "dispatch-error: backend unreachable"


# ---------------------------------------------------------------------------
# AC4-EDGE: missing manifest elides manifest-derived fields without breaking
# ---------------------------------------------------------------------------


def test_build_manifest_none_elides_fields():
    state = _two_wave_state()
    rendered = build_mission_artifact(state, manifest=None)
    fm = yaml.safe_load(rendered.split("---\n", 2)[1])
    # State-derived fields still populated
    assert fm["mission_id"] == "ab-mm0010"
    assert fm["total_dispatched"] == 3
    # Manifest-derived fields elided
    assert fm["projects"] is None
    assert fm["total_waves_planned"] is None


# ---------------------------------------------------------------------------
# AC4-EDGE: builder is deterministic for the same inputs
# ---------------------------------------------------------------------------


def test_build_idempotent():
    state = _two_wave_state()
    manifest = _two_wave_manifest()
    completed_at = "2026-05-07T13:00:00Z"
    a = build_mission_artifact(state, manifest, completed_at=completed_at)
    b = build_mission_artifact(state, manifest, completed_at=completed_at)
    assert a == b


# ---------------------------------------------------------------------------
# AC2-ERR: YAML round-trips safely; no python-object tags or eval surface
# ---------------------------------------------------------------------------


def test_build_yaml_serializes_safely():
    state = _two_wave_state()
    manifest = _two_wave_manifest()
    rendered = build_mission_artifact(state, manifest)
    fm_text = rendered.split("---\n", 2)[1]
    # safe_load round-trips without raising on python-object tags
    parsed = yaml.safe_load(fm_text)
    assert isinstance(parsed, dict)
    # No leak of python types into the frontmatter (no Path, no datetime objects).
    for value in parsed.values():
        assert not repr(type(value)).startswith("<class 'pathlib.")
