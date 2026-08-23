"""`fno do pr wait` - the sanctioned watcher (x-4eac).

One verb replaces every hand-rolled `while/sleep/grep` poll loop: each tick
rides the coalescing cache, the exit code is the status verb's own alphabet,
and a timeout exits with the last observed code plus an unsettled note.
These tests drive `wait_status` with a fake `cached_status` and a fake clock:
no test here may shell gh.
"""
from __future__ import annotations

import json
import sys

from fno.pr import _wait


def _fake_cached_status(monkeypatch, payload: dict, rc: int):
    def f(pr, cwd=None, refresh=False):
        sys.stdout.write(json.dumps(payload) + "\n")
        return rc

    from fno.pr import _cache

    monkeypatch.setattr(_cache, "cached_status", f)


class _Clock:
    """Returns ticks[0] on the first call (the deadline read), then advances."""

    def __init__(self, *ticks):
        self.last = list(ticks or [0.0])[0]
        self._next = list(ticks[1:])

    def __call__(self):
        now = self.last
        if self._next:
            self.last = self._next.pop(0)
        return now


def test_parse_duration_units_and_garbage() -> None:
    assert _wait.parse_duration("30m") == 1800.0
    assert _wait.parse_duration("90s") == 90.0
    assert _wait.parse_duration("1.5h") == 5400.0
    assert _wait.parse_duration("45") == 45.0
    assert _wait.parse_duration("junk") == 0.0


def test_settled_green_exits_the_status_code(monkeypatch, capsys) -> None:
    _fake_cached_status(
        monkeypatch,
        {"pr": "9", "verdict": "green", "settled": True, "green": True},
        0,
    )
    rc = _wait.wait_status(
        "9", until="settled", timeout=60, interval=5, sleeper=lambda s: None,
        clock=_Clock(0),
    )
    assert rc == 0
    out = capsys.readouterr()
    assert '"settled": true' in out.out  # the final payload is re-emitted
    # The spend line is the promise that separates the verb from the loops it
    # replaces: the spender sees its spend at exit.
    assert "gh call(s) this invocation" in out.err


def test_settled_red_exits_one_not_two(monkeypatch, capsys) -> None:
    """A settled-red PR is an ANSWER, not a wait failure: the watcher wakes
    the session with the red verdict, exactly like the hand-rolled loop's
    settled-grep did."""
    _fake_cached_status(
        monkeypatch,
        {"pr": "9", "verdict": "red", "settled": True, "green": False},
        1,
    )
    rc = _wait.wait_status(
        "9", until="settled", timeout=60, interval=5, sleeper=lambda s: None,
        clock=_Clock(0),
    )
    assert rc == 1


def test_timeout_exits_last_code_with_an_unsettled_note(monkeypatch, capsys) -> None:
    _fake_cached_status(
        monkeypatch,
        {"pr": "9", "verdict": "pending", "settled": False, "green": False},
        2,
    )
    rc = _wait.wait_status(
        "9", until="settled", timeout=120, interval=60, sleeper=lambda s: None,
        clock=_Clock(0, 100),
    )
    assert rc == 2
    err = capsys.readouterr().err
    assert "still not settled" in err


def test_until_green_keeps_waiting_through_red(monkeypatch, capsys) -> None:
    """--until green means merge-ready: a settled red is not done, and the
    wait must not exit early claiming success."""
    _fake_cached_status(
        monkeypatch,
        {"pr": "9", "verdict": "red", "settled": True, "green": False},
        1,
    )
    rc = _wait.wait_status(
        "9", until="green", timeout=120, interval=60, sleeper=lambda s: None,
        clock=_Clock(0, 100),
    )
    assert rc == 1
    assert "still not green" in capsys.readouterr().err


def test_unknown_condition_and_bad_args_refuse(monkeypatch, capsys) -> None:
    assert _wait.wait_status("9", until="purple", timeout=10) == 2
    capsys.readouterr()
    assert _wait.main(["9", "--until", "settled", "--timeout", "nope"]) == 2
    capsys.readouterr()
    assert _wait.main(["not-a-pr"]) == 2
    capsys.readouterr()


def test_an_unknown_flag_is_refused_never_dropped(capsys) -> None:
    """A typo'd flag must not fall back to a bound the caller never chose -
    the same refuse-don't-drop discipline `fno do pr status` enforces."""
    assert _wait.main(["9", "--timeou", "30m"]) == 2
    err = capsys.readouterr().err
    assert "unrecognized flag" in err and "--timeou" in err
