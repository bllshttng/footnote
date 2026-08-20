"""The acceptance run for busy mode: held mail drains with no operator (x-481e).

Mail drains at exactly two moments in this codebase, SessionStart and
UserPromptSubmit, and both need the operator to type. This test exercises the
third trigger end to end: a REAL detached `fno mail hold-release` process wakes
on its own clock, clears the flag, dedupes, and emits its marker. No prompt, no
injected text, no user-shaped input anywhere in the run.

The assertion is the POSITIVE marker on the release. Nothing here asserts that
no injection happened during the hold, because a working hold and a dead bus
produce that same absence, and a check that cannot separate them proves nothing.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

HANDLE = "ac0eptrun"

# The venv console script, so the timer runs THIS worktree's source rather than
# whatever `fno` is on PATH (a deployed binary can be several merges behind).
_FNO = str(Path(sys.executable).parent / "fno-py")

pytestmark = pytest.mark.skipif(
    not Path(_FNO).exists(), reason="no fno console script in this environment"
)


def _run_release(env, *, poll_s=1):
    """Start the release timer as its own process and wait for it to finish."""
    return subprocess.run(
        [
            _FNO,
            "mail",
            "hold-release",
            "--handle",
            HANDLE,
            "--poll-s",
            str(poll_s),
        ],
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )


@pytest.fixture
def state(tmp_path, monkeypatch):
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    # Set the bus override on THIS process too, so mail written here and mail
    # read by the child timer resolve to one log.
    monkeypatch.setenv("FNO_BUS_DIR", str(home / ".fno" / "bus"))
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["FNO_AGENTS_HOME"] = str(home / ".fno" / "agents")
    env.pop("FNO_STATE_DIR", None)
    return env, home


def _send_to_held_session(body, *, sender="worker", ts):
    """Put one durable message on the bus addressed to the held session."""
    from fno.bus.log import Envelope, append, new_msg_id

    msg_id = new_msg_id()
    append(
        Envelope(
            id=msg_id,
            thread=msg_id,
            from_=sender,
            to=HANDLE,
            kind="send",
            body=body,
            ts=ts,
        )
    )
    return msg_id


def test_a_held_message_lands_after_expiry_with_no_operator_input(state, tmp_path):
    env, home = state
    hold_dir = home / ".fno" / "mail-hold"
    hold_dir.mkdir(parents=True, exist_ok=True)

    # Three messages arrive during the hold, two of them identical - the shape a
    # worker produces when it gets no answer and re-sends.
    _send_to_held_session("first report", ts="2026-08-20T10:00:00Z")
    _send_to_held_session("second report", sender="other", ts="2026-08-20T10:00:01Z")
    _send_to_held_session("first report", ts="2026-08-20T10:00:02Z")

    # Arm a two-second hold by writing the clock directly: this test is about
    # the RELEASE trigger, and `fno mail hold` needs an ambient harness
    # identity a subprocess does not have.
    until = time.gmtime(time.time() + 2)
    (hold_dir / f"{HANDLE}.json").write_text(
        json.dumps(
            {
                "until": time.strftime("%Y-%m-%dT%H:%M:%SZ", until),
                "window_s": 2,
            }
        ),
        encoding="utf-8",
    )

    started = time.monotonic()
    proc = _run_release(env)
    elapsed = time.monotonic() - started

    assert proc.returncode == 0, proc.stderr
    assert elapsed >= 1.5, "the timer returned before its own deadline"

    result = json.loads(proc.stdout.strip().splitlines()[-1])
    assert result["handle"] == HANDLE
    assert result["held_count"] == 3, "the release must see everything held"
    assert result["deduped_count"] == 1, "the two identical bodies collapse to one"
    # The transport is the only thing this environment cannot exercise: there is
    # no daemon socket to inject through, so the outcome is the honest miss. The
    # delivered branch and its cursor advance are asserted in
    # cli/tests/unit/test_mail_hold.py against the same release function.
    assert result["outcome"] in ("delivered", "inject-missed")

    # The positive marker, on the release, in the durable event log.
    events = (home / ".fno" / "events.jsonl").read_text(encoding="utf-8")
    released = [
        json.loads(line)
        for line in events.splitlines()
        if line.strip() and json.loads(line).get("kind") == "mail_hold_released"
    ]
    assert len(released) == 1, "the release must emit exactly one marker"
    assert released[0]["handle"] == HANDLE
    assert released[0]["held_count"] == 3

    # The clock is gone, so nothing re-releases and the DND column reads clear.
    assert not (hold_dir / f"{HANDLE}.json").exists()


def test_the_timer_exits_quietly_when_the_hold_was_lifted_by_hand(state):
    """`fno mail hold --off` already released, so the timer must not release twice."""
    env, home = state

    proc = _run_release(env)

    assert proc.returncode == 0, proc.stderr
    assert proc.stdout.strip() == ""
    events_path = home / ".fno" / "events.jsonl"
    assert not events_path.exists() or "mail_hold_released" not in events_path.read_text(
        encoding="utf-8"
    )


def test_an_idle_rearm_extends_the_hold_past_the_original_deadline(state):
    """The timer re-reads its clock on every wake, so a prompt pushes it out.

    Without the re-read the timer commits to the first deadline and fires while
    the operator is still mid-conversation, which is the interruption busy mode
    exists to stop.
    """
    env, home = state
    hold_dir = home / ".fno" / "mail-hold"
    hold_dir.mkdir(parents=True, exist_ok=True)
    clock = hold_dir / f"{HANDLE}.json"

    def _write_deadline(seconds_out):
        clock.write_text(
            json.dumps(
                {
                    "until": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + seconds_out)
                    ),
                    "window_s": 2,
                }
            ),
            encoding="utf-8",
        )

    _write_deadline(2)
    proc = subprocess.Popen(
        [
            _FNO, "mail", "hold-release", "--handle", HANDLE, "--poll-s", "1",
        ],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        time.sleep(1)
        _write_deadline(4)  # the operator typed: the idle window restarts
        started = time.monotonic()
        proc.communicate(timeout=30)
        assert time.monotonic() - started >= 2.5, (
            "the timer fired on its original deadline, ignoring the re-arm"
        )
    finally:
        if proc.poll() is None:
            proc.kill()

    assert not clock.exists()
