"""Megatron - cross-project fleet orchestration.

Lifts the existing megawalk wave model up one altitude so projects (not
tasks) become wave units. Since group 3 of the control-plane step-5
collapse (ab-9fd662c6), the commander is the unified Rust loop
(``fno-agents loop run --driver megatron``); this package provides the
mission-queue verbs it shells (``queue.mission_next`` /
``queue.mission_complete``), the manifest/state substrate, and the
dispatch (plan file + ``fno backlog intake``) that creates each
project's mission node.

Public API:
    Manifest          - parsed mission manifest (dataclass)
    Wave              - one wave block from the manifest
    Project           - one project within a wave
    Budget            - per-mission cost cap config
    ManifestError     - parse-time error
    load_manifest     - parse a manifest path into a Manifest
    validate_manifest - return aggregated ValidationError list
    ValidationError   - validator output (code/wave_index/message)
    MissionState      - parsed mission state file (dataclass)
    MissionStateCorrupt
    MissionStateRegression
    CommanderAlreadyRunning
    read_state        - load mission state with corruption guard
    write_state       - filelock-protected write with monotonicity check
    append_sent_msg_id

Phases 3+ (commander loop, CLI, budget, research) import these symbols.
"""
from __future__ import annotations

from fno.megatron.artifact import (
    build_mission_artifact,
    mission_artifact_path,
    write_mission_artifact,
)
from fno.megatron.manifest import (
    Budget,
    Manifest,
    ManifestError,
    Project,
    Wave,
    load_manifest,
)
from fno.megatron.state import (
    CommanderAlreadyRunning,
    MissionState,
    MissionStateCorrupt,
    MissionStateError,
    MissionStateRegression,
    _append_received_complete_for_test,
    append_received_complete,
    append_sent_msg_id,
    read_state,
    resolve_mission_directory,
    update_status,
    write_state,
)
from fno.megatron.validator import (
    ValidationError,
    validate_manifest,
)
from fno.megatron import queue  # re-export module for `from fno.megatron import queue`

__all__ = [
    "Manifest",
    "Wave",
    "Project",
    "Budget",
    "ManifestError",
    "load_manifest",
    "MissionState",
    "MissionStateCorrupt",
    "MissionStateError",
    "MissionStateRegression",
    "CommanderAlreadyRunning",
    "read_state",
    "write_state",
    "append_sent_msg_id",
    # append_received_complete removed from public API; use filesystem completion
    # files (Wave 3's stop hook) in production. For tests, use
    # _append_received_complete_for_test or the append_received_complete alias.
    "update_status",
    "resolve_mission_directory",
    "ValidationError",
    "validate_manifest",
    "build_mission_artifact",
    "mission_artifact_path",
    "write_mission_artifact",
]
