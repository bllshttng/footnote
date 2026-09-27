"""The session_pid shim over the native `claim session-pid` verb.

The walk itself is Rust (`spawn_context::session_identity_from_table`), so
these tests pin the shim's contract only: one cached exec per from_pid (AC6),
and the degrade-to-uncapturable shape. Assertions sit on `_session_identity`
(the cache seam) because the module's public wrappers are neutralized by the
suite-wide `_neutral_host_harness` fixture.
"""

import json
import subprocess
from unittest import mock

import pytest

from fno.claims import session_pid


def _run_returning(payload):
    return subprocess.CompletedProcess(
        args=[], returncode=0, stdout=json.dumps(payload), stderr=""
    )


@pytest.fixture(autouse=True)
def _fresh_cache():
    session_pid._clear_session_identity_cache()
    yield
    session_pid._clear_session_identity_cache()


def test_both_halves_come_from_one_cached_read():
    run = mock.MagicMock(return_value=_run_returning({"session_pid": 4242, "harness": "claude"}))
    with mock.patch.object(subprocess, "run", run):
        first = session_pid._session_identity(100)
        second = session_pid._session_identity(100)
    assert first == (4242, "claude")
    assert second == (4242, "claude")
    # AC6: one exec total, whatever the call order.
    assert run.call_count == 1
    assert run.call_args.args[0] == [
        "fno",
        "agents",
        "claim",
        "session-pid",
        "--json",
        "--from-pid",
        "100",
    ]


def test_distinct_from_pid_execs_again():
    run = mock.MagicMock(return_value=_run_returning({"session_pid": 4242, "harness": "claude"}))
    with mock.patch.object(subprocess, "run", run):
        session_pid._session_identity(100)
        session_pid._session_identity(200)
    assert run.call_count == 2


def test_uncapturable_degrades_to_none_without_raising():
    run = mock.MagicMock(return_value=_run_returning({"session_pid": None, "harness": None}))
    with mock.patch.object(subprocess, "run", run):
        assert session_pid._session_identity(None) == (None, None)


def test_failed_read_degrades_to_none_without_raising():
    with mock.patch.object(subprocess, "run", side_effect=OSError("binary gone")):
        assert session_pid._session_identity(100) == (None, None)


def test_malformed_payload_degrades_to_none():
    run = mock.MagicMock(
        return_value=subprocess.CompletedProcess(args=[], returncode=0, stdout="not json", stderr="")
    )
    with mock.patch.object(subprocess, "run", run):
        assert session_pid._session_identity(100) == (None, None)


def test_thread_worker_shape_refuses_pid_keeps_harness():
    # The measured thread-worker answer: the refusing walk declines the pid,
    # the harness walk behind the spare still proves claude.
    run = mock.MagicMock(return_value=_run_returning({"session_pid": None, "harness": "claude"}))
    with mock.patch.object(subprocess, "run", run):
        assert session_pid._session_identity(None) == (None, "claude")


def test_pid_dies_with_session_still_a_deny_list():
    assert session_pid.pid_dies_with_session("claude") is True
    assert session_pid.pid_dies_with_session("codex") is False
    assert session_pid.pid_dies_with_session(None) is True
