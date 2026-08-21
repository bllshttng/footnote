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


def _cli_argv() -> list:
    """How to invoke THIS checkout's CLI as a separate process.

    Prefer the venv console script, so the timer runs this worktree's source
    rather than whatever `fno` is on PATH; a deployed binary can be several
    merges behind. Fall back to running the app through this interpreter, which
    needs nothing but the importable package.

    Deliberately NO skipif. This file is the one proof that busy mode drains
    with no operator, and it used to skip when the console script was absent. A
    skip reads as a pass in a wall of green, which is the same defect as the
    `check-pr-body-length.sh` specimen this PR also fixes - and here it would
    have hidden the node's whole acceptance criterion rather than a line count.
    A missing interpreter is not a thing to tiptoe around, so this always runs.
    """
    script = Path(sys.executable).parent / "fno-py"
    if script.exists():
        return [str(script)]
    return [sys.executable, "-c", "from fno.cli import app; app()"]


_FNO_ARGV = _cli_argv()


def _run_release(env, *, poll_s=1):
    """Start the release timer as its own process and wait for it to finish."""
    return subprocess.run(
        _FNO_ARGV
        + ["mail", "hold-release", "--handle", HANDLE, "--poll-s", str(poll_s)],
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


def _arm_clock(hold_dir, *, seconds_out, window_s=2):
    """Write a hold clock and return the exact epoch deadline it encodes.

    Truncates to a whole second BEFORE adding the offset, so the deadline the
    caller asserts against is the one the timer reads. Deriving it the other
    way loses up to a second to `strftime`, which is a real difference at these
    durations.
    """
    deadline = float(int(time.time()) + seconds_out)
    (hold_dir / f"{HANDLE}.json").write_text(
        json.dumps(
            {
                "until": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(deadline)),
                "window_s": window_s,
            }
        ),
        encoding="utf-8",
    )
    return deadline


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
    deadline = _arm_clock(hold_dir, seconds_out=2)

    proc = _run_release(env)

    assert proc.returncode == 0, proc.stderr
    # Assert against the deadline the timer was actually given, not against an
    # elapsed budget. The clock is stored to whole seconds, so a wall time of
    # X.9 truncates to X and the real wait is up to a second shorter than the
    # number asked for. An elapsed-time assertion measures the truncation as
    # much as the timer, and that is what made this flake on CI at 1.41s.
    assert time.time() >= deadline - 0.2, "the timer returned before its deadline"

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

    first_deadline = _arm_clock(hold_dir, seconds_out=2)
    proc = subprocess.Popen(
        _FNO_ARGV
        + ["mail", "hold-release", "--handle", HANDLE, "--poll-s", "1"],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        time.sleep(1)
        # The operator typed: the idle window restarts, past the first deadline.
        second_deadline = _arm_clock(hold_dir, seconds_out=4)
        assert second_deadline > first_deadline, "the re-arm must move the deadline"
        proc.communicate(timeout=30)
        assert time.time() >= second_deadline - 0.2, (
            "the timer fired on its original deadline, ignoring the re-arm"
        )
    finally:
        if proc.poll() is None:
            proc.kill()

    assert not clock.exists()
