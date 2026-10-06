"""Tests for the schema-aware Python validator at fno.events.

Covers:
  - validate(event) success and failure paths
  - typed builders (phase_transition, child_promise, mission_*)
  - size cap enforcement (64KB on data payload)
  - keyword-only builder rejects unknown kwargs at construction time
  - SchemaUnavailableError if manifest absent (smoke-tested via monkeypatch)
"""
from __future__ import annotations

import hashlib
import json

import pytest

from fno.events import (
    SchemaUnavailableError,
    ValidationError,
    child_promise,
    context_snapshot,
    mission_complete,
    mission_started,
    phase_transition,
    validate,
    wave_advanced,
    worktree_overlap_observed,
)


def test_agent_removed_and_merge_cleanup_events_validate() -> None:
    removed = {
        "ts": "2026-09-02T00:00:00Z",
        "type": "agent_removed",
        "source": "daemon",
        "data": {
            "name": "worker",
            "registry_changed": True,
            "harness": "claude",
            "harness_session_id": "session-1",
            "actor": "session:operator-1",
            "reason": "operator-requested",
            "request_id": "agent-rm-1",
            "worktree_touched": False,
            "reclaimed_bytes": 0,
        },
    }
    request = {
        "ts": "2026-09-02T00:00:00Z",
        "type": "merge_cleanup_requested",
        "source": "python",
        "data": {
            "request_id": "merge-cleanup-1",
            "repo": "/repo",
            "pr": 42,
            "branch": "feature/session",
            "worktree": "/repo/worktree",
            "node_ids": ["x-90ee"],
        },
    }
    validate(removed)
    validate(request)


# -- AC1-HP: happy path --





# -- AC2-ERR: required fields --









def test_validate_dispatch_selection_diverged() -> None:
    event = {
        "ts": "2026-08-25T09:30:42Z",
        "type": "dispatch_selection_diverged",
        "source": "backlog",
        "data": {
            "node_id": "x-known-undispatched",
            "selector_command": "fno backlog ready --json",
            "observer_command": "fno backlog undispatched --json",
            "scope": "project=fno",
            "selector_entries_scanned": 4,
            "observer_entries_scanned": 7,
        },
    }
    assert validate(event) is None


def test_validate_failover_swapped_happy_path() -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "failover_swapped",
        "source": "daemon",
        "data": {"short_id": "aaaa1111", "redispatched": True},
    }
    assert validate(event) is None


@pytest.mark.parametrize(
    "data",
    [
        {"redispatched": True},
        {"short_id": "aaaa1111"},
        {"short_id": "aaaa1111", "redispatched": "true"},
    ],
)
def test_validate_failover_swapped_rejects_invalid_payload(data: dict) -> None:
    event = {
        "ts": "2026-05-07T09:30:42Z",
        "type": "failover_swapped",
        "source": "daemon",
        "data": data,
    }
    with pytest.raises(ValidationError):
        validate(event)











# -- AC1-HP: typed builders --

def test_phase_transition_builder_happy() -> None:
    ev = phase_transition(
        gate="ledger_updated", phase="register", nonce="x", session_id="s", source="target"
    )
    assert ev["type"] == "phase_transition"
    assert ev["source"] == "target"
    assert ev["data"]["gate_bearing"] is True
    assert ev["data"]["gate"] == "ledger_updated"
    assert "ts" in ev


def test_phase_transition_builder_audit_only() -> None:
    ev = phase_transition(
        phase="review", nonce="x", session_id="s", source="fno-loop", gate_bearing=False
    )
    assert ev["data"]["gate_bearing"] is False
    assert "gate" not in ev["data"]


def test_phase_transition_rejects_unknown_kwarg() -> None:
    with pytest.raises(TypeError, match="unexpected keyword argument"):
        phase_transition(  # type: ignore[call-arg]
            gate="g", phase="p", nonce="n", session_id="s", source="target", whoops="extra"
        )


def test_child_promise_builder() -> None:
    ev = child_promise(session_id="s", nonce="n")
    assert ev["type"] == "child_promise"
    assert ev["source"] == "target"
    assert ev["data"] == {"session_id": "s", "nonce": "n"}


def test_context_snapshot_builder_is_session_bound_and_canonical() -> None:
    source_hash = "b" * 64
    ev = context_snapshot(
        session_id="harness-session",
        harness="codex",
        entry_state="startup",
        context_bytes=123,
        estimated_tokens=31,
        context_hash=hashlib.sha256(source_hash.encode()).hexdigest(),
        source_hashes=[source_hash],
        source_manifest=[
            {
                "source_id": "using-fno",
                "status": "observed",
                "bytes": 123,
                "content_hash": source_hash,
            }
        ],
        measurement_complete=True,
    )

    assert ev["type"] == "context_snapshot"
    assert ev["source"] == "hook"
    assert ev["data"]["session_id"] == "harness-session"
    assert ev["data"]["measurement_complete"] is True




def test_mission_started_builder() -> None:
    ev = mission_started(mission_id="mt-001")
    assert ev["type"] == "mission_started"
    assert ev["source"] == "megatron"
    assert ev["data"]["mission_id"] == "mt-001"


def test_wave_advanced_builder() -> None:
    ev = wave_advanced(mission_id="mt-001", wave=2, child_session_ids=["s1", "s2"])
    assert ev["data"]["wave"] == 2
    assert ev["data"]["child_session_ids"] == ["s1", "s2"]


def test_mission_complete_builder() -> None:
    ev = mission_complete(mission_id="mt-001", status="done")
    assert ev["data"]["status"] == "done"


def test_mission_complete_rejects_bad_status() -> None:
    with pytest.raises(ValidationError, match=r"unknown status"):
        mission_complete(mission_id="mt-001", status="sideways")


def test_integrity_warning_builder_happy() -> None:
    from fno.events import integrity_warning

    ev = integrity_warning(
        kind="missing_nonce_legacy_accepted",
        phase="register",
        session_id="sess-77",
        artifact_path="/tmp/x.md",
    )
    assert ev["type"] == "integrity_warning"
    assert ev["source"] == "hook"
    assert ev["data"]["kind"] == "missing_nonce_legacy_accepted"
    assert ev["data"]["artifact_path"] == "/tmp/x.md"


def test_integrity_warning_rejects_unknown_kind() -> None:
    from fno.events import integrity_warning

    with pytest.raises(ValidationError, match=r"unknown integrity_warning kind"):
        integrity_warning(
            kind="bogus_kind_value",
            phase="register",
            session_id="sess-77",
            artifact_path="/tmp/x.md",
        )


def test_done_race_collision_builder_happy() -> None:
    from fno.events import done_race_collision

    ev = done_race_collision(
        node_id="ab-deadbeef",
        first_completed_at="2026-05-15T10:00:00+00:00",
        second_attempt_at="2026-05-15T12:00:00+00:00",
    )
    assert ev["type"] == "done_race_collision"
    assert ev["source"] == "fno-loop"
    assert ev["data"]["node_id"] == "ab-deadbeef"


# -- AC4-EDGE: SchemaUnavailableError on bad path --

def test_schema_unavailable_raises(monkeypatch, tmp_path) -> None:
    """Resolving with no sibling schema.yaml must raise SchemaUnavailableError.

    The loader reads ``schema.yaml`` beside the package module. We point the
    module's ``__file__`` at an empty tmp dir (no sibling schema) so the lookup
    misses; the live package schema other tests rely on is untouched.
    """
    import fno.events as events_mod

    fake_module = tmp_path / "events" / "__init__.py"
    fake_module.parent.mkdir(parents=True)
    monkeypatch.setattr(events_mod, "__file__", str(fake_module))
    with pytest.raises(SchemaUnavailableError, match="events schema not found"):
        events_mod._resolve_manifest_path()


# -- BUG-MT-001: megatron manifest events must validate --


def test_validate_accepts_manifest_baselined() -> None:
    """Regression: schema entry for manifest_baselined must exist so
    _emit_event in megatron/queue.py does not get swallowed by
    its outer except: pass via ValidationError."""
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_baselined",
        "source": "megatron",
        "data": {
            "mission_id": "ab-mission01",
            "sha_short": "abcdef012345",
        },
    }
    assert validate(event) is None


def test_validate_accepts_manifest_mutated() -> None:
    """Regression for BUG-MT-001 sibling event."""
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_mutated",
        "source": "megatron",
        "data": {
            "mission_id": "ab-mission01",
            "stored_sha_short": "111111111111",
            "fresh_sha_short": "222222222222",
        },
    }
    assert validate(event) is None


def test_validate_rejects_manifest_baselined_missing_mission_id() -> None:
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_baselined",
        "source": "megatron",
        "data": {"sha_short": "abcdef012345"},
    }
    with pytest.raises(ValidationError):
        validate(event)


def test_validate_rejects_manifest_mutated_missing_sha_fields() -> None:
    event = {
        "ts": "2026-05-15T07:00:00Z",
        "type": "manifest_mutated",
        "source": "megatron",
        "data": {"mission_id": "ab-mission01"},
    }
    with pytest.raises(ValidationError):
        validate(event)


def test_worktree_overlap_observed_builder_is_deterministic_and_valid() -> None:
    """The builder sorts peers, stamps a stable observation id, and validates."""
    a = worktree_overlap_observed(
        observer_session_id="obs-1",
        peer_session_ids=["peer-b", "peer-a"],
        repository_key="/repo/.git",
        worktree_key="/repo/.git/worktrees/wt1",
    )
    b = worktree_overlap_observed(
        observer_session_id="obs-1",
        peer_session_ids=["peer-a", "peer-b"],
        repository_key="/repo/.git",
        worktree_key="/repo/.git/worktrees/wt1",
    )
    # Envelope ts differs by call; the data payload must be byte-identical.
    assert a["data"] == b["data"], "peer order must not change the observation"
    assert a["data"]["peer_session_ids"] == ["peer-a", "peer-b"], "peers sorted"
    assert a["data"]["observation_id"] == (
        "33acb94fc5648cc8d5289413548c0166fe29e7f51a7daccd922484ed95ce9d26"
    ), "observation id pinned for parity"
    assert validate(a) is None


def test_worktree_overlap_observed_rejects_empty_peers_at_build_and_validate() -> None:
    with pytest.raises(ValidationError):
        worktree_overlap_observed(
            observer_session_id="obs-1",
            peer_session_ids=[],
            repository_key="/repo/.git",
            worktree_key="/repo/.git/worktrees/wt1",
        )
    # The chokepoint must also catch a peer-less payload that bypassed the builder.
    with pytest.raises(ValidationError):
        validate(
            {
                "ts": "2026-05-07T09:30:42Z",
                "type": "worktree_overlap_observed",
                "source": "hook",
                "data": {
                    "observation_id": "x",
                    "repository_key": "/repo/.git",
                    "worktree_key": "/repo/.git/worktrees/wt1",
                    "observer_session_id": "obs-1",
                    "peer_session_ids": [],
                    "live_window_seconds": 120,
                },
            }
        )


def test_worktree_overlap_observed_rejects_unsorted_or_duplicate_stored_peers() -> None:
    """validate requires the stored peer list to equal its sorted-unique form."""
    base = worktree_overlap_observed(
        observer_session_id="obs-1",
        peer_session_ids=["peer-a", "peer-b"],
        repository_key="/repo/.git",
        worktree_key="/repo/.git/wt1",
    )
    unsorted = json.loads(json.dumps(base))
    unsorted["data"]["peer_session_ids"] = ["peer-b", "peer-a"]
    with pytest.raises(ValidationError):
        validate(unsorted)
    dup = json.loads(json.dumps(base))
    dup["data"]["peer_session_ids"] = ["peer-a", "peer-a"]
    with pytest.raises(ValidationError):
        validate(dup)


def test_worktree_overlap_observed_rejects_observer_as_its_own_peer() -> None:
    """The observer cannot be its own peer (the predicate excludes SELF_ID)."""
    with pytest.raises(ValidationError):
        worktree_overlap_observed(
            observer_session_id="obs-1",
            peer_session_ids=["obs-1"],
            repository_key="/repo/.git",
            worktree_key="/repo/.git/wt1",
        )


# -- the review_attestation disposition obligation -----------------------------
#
# A pass disposes nothing by itself about EARLIER findings. Emitting one
# over a branch holding non-terminal blocking findings is the silent producer
# half of the impossible-merge deadlock. The chokepoint enforces it, so every
# writer (script, hook, manual emit) is covered with no new flags. Every chain
# here is constructed into the tmp repo the cap helper reads.


def _attestation_event(
    verdict: str, branch: str, *, dispositions=None, findings=None
) -> dict:
    data = {
        "reviewer": "code-review",
        "head_sha": "0" * 39 + "1",
        "verdict": verdict,
        "session_id": "s-ob",
        "branch": branch,
        "reviewed_base_sha": "a" * 40,
        "reviewed_head_sha": "0" * 39 + "1",
        "reviewed_line_count": 10,
        "reviewed_file_count": 2,
    }
    if dispositions is not None:
        data["dispositions"] = dispositions
    if findings is not None:
        data["findings"] = findings
    return {
        "ts": "2026-08-31T19:00:00Z",
        "type": "review_attestation",
        "source": "target",
        "data": data,
    }


def _obligation_chain_event(i: int, verdict: str, findings, dispositions=None) -> dict:
    data = {
        "reviewer": "code-review",
        "head_sha": f"{i:040x}",
        "verdict": verdict,
        "session_id": "s-ob",
        "branch": "feature/x-ob",
        "reviewed_base_sha": "a" * 40,
        "reviewed_head_sha": f"{i:040x}",
        "findings_blocking": len(findings),
        "findings": findings,
    }
    if dispositions:
        data["dispositions"] = dispositions
    return {"ts": f"2026-08-31T1{i:02d}:00:00Z", "type": "review_attestation",
            "source": "target", "data": data}


_OB_HARD = {
    "category": "correctness",
    "verdict": "CONFIRMED",
    "blocking": True,
    "has_required_fields": True,
    "finding_key": "cli/src/fake.py:779:correctness",
}

_OB_EFF = {
    "category": "efficiency",
    "verdict": "CONFIRMED",
    "blocking": True,
    "has_required_fields": True,
    "finding_key": "crates/fno-agents/src/cargo_build_dirs.rs:208:efficiency",
}

_OB_COVERAGE = {
    "category": "test-coverage",
    "verdict": "CONFIRMED",
    "blocking": False,
    "has_required_fields": True,
    "finding_key": "cli/tests/fake.py:10:test-coverage",
}


def _seed_obligation_chain(tmp_path, events) -> None:
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in events) + "\n", encoding="utf-8"
    )
























def _lead_checkin(data: dict) -> dict:
    return {
        "ts": "2026-09-10T12:00:00Z",
        "type": "lead_checkin",
        "source": "loop",
        "data": data,
    }












def test_worker_pattern_source_outranks_a_narrower_per_type_list() -> None:
    event = {
        "ts": "2026-09-16T12:00:00Z",
        "type": "agent_spawned",
        "source": "worker:wkA",
        "data": {},
    }
    assert validate(event) is None


def test_every_declared_source_is_a_legal_envelope_source() -> None:
    """A type's sources: list must draw only from values the envelope
    itself allows, or the per-type check in validate() could refuse
    every producer of that source unconditionally."""
    import fno.events as events_mod

    events_mod._require_schema()
    envelope_sources = set(events_mod.ALLOWED_SOURCES)
    assert events_mod.EVENT_TYPES is not None
    violations = [
        (name, source)
        for name, spec in events_mod.EVENT_TYPES.items()
        for source in (spec.get("sources") or [])
        if source not in envelope_sources
    ]
    assert violations == []
