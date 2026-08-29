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


def test_cli_shape_flag_values_do_not_read_as_pr_numbers(monkeypatch, capsys) -> None:
    """The parser once collected positionals with a leading comprehension,
    which cannot know the token after `--until` is a VALUE - so every flag
    value read as a second PR number and the CLI spelling refused on usage
    every run. The verb was unreachable from the CLI while these tests
    passed, because they called wait_status directly."""
    _fake_cached_status(
        monkeypatch,
        {"pr": "9", "verdict": "green", "settled": True, "green": True},
        0,
    )
    rc = _wait.main(["9", "--until", "settled", "--timeout", "1m", "--interval", "5"])
    assert rc == 0
    assert '"settled": true' in capsys.readouterr().out


def test_torn_last_json_line_is_not_answered_by_an_older_line(monkeypatch, capsys) -> None:
    # A partially-written newest line must not hand the wait an OLDER payload
    # (stale JSON paired with the current exit code); the tick reads as no
    # answer, the wait keeps polling, and the deadline decides.
    from fno.pr import _cache

    lines = [
        '{"pr": "9", "verdict": "green", "settled": true, "green": true}\n',
        '{"pr": "9", "verdict": "gr',
    ]

    def f(pr, cwd=None, refresh=False):
        for ln in lines:
            sys.stdout.write(ln)
        return 0

    monkeypatch.setattr(_cache, "cached_status", f)
    rc = _wait.wait_status(
        "9", until="settled", timeout=60, interval=5, sleeper=lambda s: None,
        clock=_Clock(0, 0, 70),
    )
    # Never settled on the torn read: the deadline fires with the last code.
    assert rc == 2
    assert "still not settled" in capsys.readouterr().err


def test_review_mode_wakes_when_count_grows(monkeypatch, capsys) -> None:
    """--until review exits 0 the moment the review count rises above the
    baseline captured at the first successful read."""
    from fno.pr import _wait

    reads = [3, 3, 5]
    monkeypatch.setattr(_wait, "_review_count", lambda pr, cwd=None: reads.pop(0))
    rc = _wait.wait_status(
        "9", until="review", timeout=30, interval=5, sleeper=lambda s: None,
        clock=_Clock(0, 10),
    )
    assert rc == 0
    err = capsys.readouterr().err
    assert "new review" in err and "3 -> 5" in err


def test_review_mode_timeout_names_last_count(monkeypatch, capsys) -> None:
    from fno.pr import _wait

    reads = [4, 4]
    monkeypatch.setattr(
        _wait, "_review_count", lambda pr, cwd=None: reads.pop(0) if reads else 4
    )
    rc = _wait.wait_status(
        "9", until="review", timeout=30, interval=5, sleeper=lambda s: None,
        clock=_Clock(0, 28),
    )
    assert rc == 2
    err = capsys.readouterr().err
    assert "still no new review" in err and "last count 4" in err


def test_review_mode_never_exits_zero_on_an_unreadable_read(monkeypatch, capsys) -> None:
    """A failed read is no-answer, not zero-reviews: the wait rides it out and
    the deadline decides. Exiting 0 on an error would wake the session for a
    review nobody saw."""
    from fno.pr import _wait

    reads = [None, None]
    monkeypatch.setattr(_wait, "_review_count", lambda pr, cwd=None: reads.pop(0))
    rc = _wait.wait_status(
        "9", until="review", timeout=30, interval=5, sleeper=lambda s: None,
        clock=_Clock(0, 28),
    )
    assert rc == 2
    assert "no read succeeded" in capsys.readouterr().err


def test_review_mode_baseline_lands_on_first_successful_read(monkeypatch, capsys) -> None:
    """The first tick fails, the second succeeds and IS the baseline - a later
    equal count is not growth."""
    from fno.pr import _wait

    reads = [None, 2, 2]
    monkeypatch.setattr(_wait, "_review_count", lambda pr, cwd=None: reads.pop(0))
    rc = _wait.wait_status(
        "9", until="review", timeout=30, interval=5, sleeper=lambda s: None,
        clock=_Clock(0, 10, 28),
    )
    assert rc == 2
    assert "last count 2" in capsys.readouterr().err


def test_cli_shape_reaches_review_mode(monkeypatch, capsys) -> None:
    """The CLI spelling must reach the review path - the x-4eac lesson: unit
    tests calling wait_status directly while the verb was unreachable."""
    from fno.pr import _wait

    seen = {}

    def fake_wait(pr, *, until, timeout, interval, cwd=None, sleeper=None, clock=None):
        seen.update(until=until, pr=pr)
        return 0

    monkeypatch.setattr(_wait, "wait_status", fake_wait)
    rc = _wait.main(["9", "--until", "review", "--timeout", "1m", "--interval", "5"])
    capsys.readouterr()
    assert rc == 0
    assert seen == {"until": "review", "pr": "9"}


def test_repo_slug_strips_both_remote_spellings() -> None:
    from fno.pr import _wait

    assert _wait._slug_from_url("https://github.com/bll/footnote.git") == "bll/footnote"
    assert _wait._slug_from_url("git@github.com:bll/footnote.git") == "bll/footnote"
    assert _wait._slug_from_url("ssh://git@github.com/bll/footnote") == "bll/footnote"
    assert _wait._slug_from_url("not-a-url") == ""
