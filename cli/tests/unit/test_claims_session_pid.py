"""Unit tests for fno.claims.session_pid: the shim over the native verb.

The ancestor walk itself lives in Rust (`spawn_context.rs`, served by
`fno agents claim session-pid`); its semantics - nearest harness wins, the
claude exe-only substring rule, segment-vs-substring matching, the node-shim
stem rule, pool-machinery refusal, and the FNO_SESSION_PID stamp pair - are
pinned by spawn_context's own tests (session_identity_*, ambient_stamp_pair,
harness_walk_still_proves_claude_behind_a_spare). What stays testable here is
the Python contract: exec the verb once per from_pid, cache the answer,
answer both halves from that one read, and degrade to (None, None) on any
failure mode instead of raising into a caller that holds a claim lock.
"""
from __future__ import annotations

import json
import subprocess
from unittest.mock import patch

import pytest

from fno.claims import session_pid
from fno.claims.session_pid import (
    pid_dies_with_session,
    resolve_session_harness,
    resolve_session_pid,
)


@pytest.fixture(autouse=True)
def _fresh_identity_cache():
    """Each test starts from a cold memo and leaves one behind it."""
    session_pid._clear_session_identity_cache()
    yield
    session_pid._clear_session_identity_cache()


def _reply(payload=None, rc=0, stdout=None):
    """A CompletedProcess shaped like the verb's answer."""
    if stdout is None:
        stdout = "" if payload is None else json.dumps(payload)
    return subprocess.CompletedProcess([], rc, stdout=stdout, stderr="")


def test_verb_answer_fills_both_halves():
    """One JSON read answers pid and harness together (AC6)."""
    with patch.object(
        session_pid.subprocess,
        "run",
        return_value=_reply({"session_pid": 20, "harness": "claude"}),
    ) as run:
        assert resolve_session_pid(from_pid=10) == 20
        assert resolve_session_harness(from_pid=10) == "claude"
    (call,) = run.call_args_list
    assert call.args[0] == [
        "fno",
        "agents",
        "claim",
        "session-pid",
        "--json",
        "--from-pid",
        "10",
    ]


def test_one_exec_per_from_pid_cached():
    """Repeat reads for one from_pid pay one exec; a new from_pid execs again."""
    with patch.object(
        session_pid.subprocess,
        "run",
        return_value=_reply({"session_pid": 20, "harness": "claude"}),
    ) as run:
        assert resolve_session_pid(from_pid=10) == 20
        assert resolve_session_pid(from_pid=10) == 20
        assert resolve_session_harness(from_pid=10) == "claude"
        assert run.call_count == 1
        assert resolve_session_pid(from_pid=11) == 20
        assert run.call_count == 2


def test_cache_clear_seam_reexecs():
    """The documented test seam drops the memo so a re-exec is observable."""
    with patch.object(
        session_pid.subprocess,
        "run",
        return_value=_reply({"session_pid": 20, "harness": None}),
    ) as run:
        assert resolve_session_pid(from_pid=10) == 20
        session_pid._clear_session_identity_cache()
        assert resolve_session_pid(from_pid=10) == 20
        assert run.call_count == 2


def test_degrades_when_the_binary_is_missing():
    with patch.object(
        session_pid.subprocess, "run", side_effect=FileNotFoundError("fno")
    ):
        assert resolve_session_pid(from_pid=10) is None
        assert resolve_session_harness(from_pid=10) is None


def test_degrades_on_a_nonzero_exit():
    with patch.object(
        session_pid.subprocess, "run", return_value=_reply(rc=1, stdout="boom")
    ):
        assert resolve_session_pid(from_pid=10) is None


def test_degrades_on_malformed_json():
    with patch.object(
        session_pid.subprocess, "run", return_value=_reply(stdout="not json")
    ):
        assert resolve_session_pid(from_pid=10) is None
        assert resolve_session_harness(from_pid=10) is None


def test_degrades_on_a_non_dict_payload():
    with patch.object(
        session_pid.subprocess, "run", return_value=_reply(stdout="[20]")
    ):
        assert resolve_session_pid(from_pid=10) is None


def test_degrades_on_a_timeout():
    with patch.object(
        session_pid.subprocess,
        "run",
        side_effect=subprocess.TimeoutExpired(cmd="fno", timeout=30),
    ):
        assert resolve_session_pid(from_pid=10) is None


@pytest.mark.parametrize("bad", ["20", 0, -3, 2.5])
def test_rejects_pid_halves_that_are_not_positive_ints(bad):
    with patch.object(
        session_pid.subprocess,
        "run",
        return_value=_reply({"session_pid": bad, "harness": "claude"}),
    ):
        assert resolve_session_pid(from_pid=10) is None


def test_rejects_a_non_string_harness_but_keeps_the_pid():
    with patch.object(
        session_pid.subprocess,
        "run",
        return_value=_reply({"session_pid": 20, "harness": 42}),
    ):
        assert resolve_session_pid(from_pid=10) == 20
        assert resolve_session_harness(from_pid=10) is None


def test_pid_dies_with_session_is_a_measured_deny_list():
    """codex is the one measured shared host; everything else dies with the
    session, including an unknown or absent harness."""
    assert pid_dies_with_session("codex") is False
    assert pid_dies_with_session("Codex") is False
    for harness in ("claude", "gemini", "opencode", "agy", "cursor-agent", "", None):
        assert pid_dies_with_session(harness) is True, harness
