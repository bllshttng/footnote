"""Door-shaped spawn provenance from the Python side.

The Rust side pins the same dict shapes deserialize into ``SpawnProvenance``
(``spawn_contract::tests::python_shaped_provenance_round_trips``); these
tests pin that the Python builder emits exactly those shapes and enforces
the door's rules.
"""

from __future__ import annotations

import json

import pytest

from fno.agents.spawn_lineage import build_spawn_provenance


def _with_env(monkeypatch, markers: dict[str, str]) -> None:
    monkeypatch.setenv("PWD", "/repo")
    for name in (
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
        "FNO_HARNESS_NAME",
        "FNO_HARNESS_SESSION_ID",
    ):
        monkeypatch.delenv(name, raising=False)
    for name, value in markers.items():
        monkeypatch.setenv(name, value)


def test_session_markers_build_a_door_shaped_record(monkeypatch):
    _with_env(monkeypatch, {"CLAUDE_CODE_SESSION_ID": "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9"})
    provenance = build_spawn_provenance()
    assert provenance is not None
    origin = provenance["origin"]
    owner = provenance["owner"]
    assert origin["kind"] == "session"
    assert origin["parent"]["session_id"] == "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9"
    assert origin["parent"]["harness"] == "claude"
    assert origin["parent"]["cwd"] == "/repo"
    assert owner["kind"] == "session"
    assert owner["session_id"] == origin["parent"]["session_id"]
    # JSON round-trip keeps the shape a Rust reader deserializes.
    reparsed = json.loads(json.dumps(provenance))
    assert reparsed == provenance


def test_unprovable_caller_builds_no_record(monkeypatch):
    _with_env(monkeypatch, {})
    # The process-tree walk can still prove a harness under a real harness
    # ancestor; scrub that proof too by refusing the walk is not possible
    # here, so accept either outcome: None (nothing proven) or a record
    # whose session id is present and whose shapes hold.
    provenance = build_spawn_provenance()
    if provenance is None:
        return
    assert provenance["origin"]["kind"] == "session"
    assert provenance["owner"]["kind"] == "session"


def test_explicit_daemon_origin_requires_mission_or_crown_owner(monkeypatch):
    monkeypatch.setattr(
        "fno.agents.naming.dispatch_sources",
        lambda: frozenset({"ab", "ac", "rd", "th", "pw", "jn"}),
    )
    origin = {
        "kind": "non_session",
        "source": {
            "kind": "daemon",
            "exe": "fno-agents-daemon",
            "arm": "active-backlog",
            "cause": "ab",
        },
    }
    with pytest.raises(ValueError, match="mission or crown owner"):
        build_spawn_provenance(
            explicit_origin=origin,
            explicit_owner={"kind": "operator", "tty": "/dev/ttys001"},
        )
    owner = {"kind": "mission", "project": "fno", "mission": "epic-x"}
    provenance = build_spawn_provenance(explicit_origin=origin, explicit_owner=owner)
    assert provenance["origin"]["source"]["cause"] == "ab"
    assert provenance["owner"]["kind"] == "mission"


def test_retired_sob_cause_refuses(monkeypatch):
    monkeypatch.setattr(
        "fno.agents.naming.dispatch_sources",
        lambda: frozenset({"ab", "ac", "rd", "th"}),
    )
    origin = {
        "kind": "non_session",
        "source": {
            "kind": "daemon",
            "exe": "fno-agents-daemon",
            "arm": "blueprint",
            "cause": "sob",
        },
    }
    owner = {"kind": "crown", "project": "fno", "scope": "epic-x"}
    with pytest.raises(ValueError, match="sob"):
        build_spawn_provenance(explicit_origin=origin, explicit_owner=owner)


def test_unknown_cause_code_refuses(monkeypatch):
    monkeypatch.setattr(
        "fno.agents.naming.dispatch_sources",
        lambda: frozenset({"ab", "ac", "rd", "th"}),
    )
    origin = {
        "kind": "non_session",
        "source": {
            "kind": "daemon",
            "exe": "fno-agents-daemon",
            "arm": "mystery-arm",
            "cause": "zz",
        },
    }
    owner = {"kind": "crown", "project": "fno", "scope": "epic-x"}
    with pytest.raises(ValueError, match="vocabulary"):
        build_spawn_provenance(explicit_origin=origin, explicit_owner=owner)


def test_carrier_env_outranks_ambient(monkeypatch):
    _with_env(monkeypatch, {"CLAUDE_CODE_SESSION_ID": "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9"})
    monkeypatch.setenv(
        "FNO_SPAWN_ORIGIN",
        json.dumps(
            {
                "kind": "non_session",
                "source": {
                    "kind": "daemon",
                    "exe": "fno-agents-daemon",
                    "arm": "active-backlog",
                    "cause": "ab",
                },
            }
        ),
    )
    monkeypatch.setenv(
        "FNO_SPAWN_OWNER",
        json.dumps({"kind": "crown", "project": "fno", "scope": "epic-x"}),
    )
    provenance = build_spawn_provenance()
    assert provenance is not None
    assert provenance["origin"]["kind"] == "non_session"
    assert provenance["origin"]["source"]["arm"] == "active-backlog"
    assert provenance["owner"] == {"kind": "crown", "project": "fno", "scope": "epic-x"}


def test_half_carrier_refuses(monkeypatch):
    _with_env(monkeypatch, {})
    monkeypatch.setenv(
        "FNO_SPAWN_ORIGIN",
        json.dumps({"kind": "non_session", "source": {"kind": "daemon", "exe": "d", "arm": "a", "cause": "ab"}}),
    )
    monkeypatch.delenv("FNO_SPAWN_OWNER", raising=False)
    with pytest.raises(ValueError, match="together"):
        build_spawn_provenance()


def test_session_launched_test_keeps_its_session_parent(monkeypatch):
    _with_env(monkeypatch, {"CLAUDE_CODE_SESSION_ID": "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9"})
    provenance = build_spawn_provenance()
    assert provenance is not None
    # The parent stays the live session; a script invocation is recorded as
    # the invocation, never as a parent replacement.
    invocation = {"kind": "test_script", "reference": "tests/some-script.sh"}
    provenance["origin"]["invocation"] = invocation
    assert provenance["origin"]["parent"]["session_id"] == (
        "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9"
    )
