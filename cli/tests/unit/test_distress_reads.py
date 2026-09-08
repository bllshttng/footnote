"""x-3ecf: the `distress-answered` board-collection helper.

The king board's blocked_child queue shells this command once per board
build (never once per row) to learn whether mail landed for a blocked
session after its row's timestamp, plus the fleet watchdog's current word
for that session as informational enrichment.
"""
from __future__ import annotations

import json

import pytest

from fno.paths_testing import use_tmpdir


@pytest.fixture
def bus(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths

    return paths.bus_dir()


def _run(pairs: list[dict]) -> dict:
    from fno.agents.distress_reads import cmd_distress_answered

    import io
    import contextlib

    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        cmd_distress_answered(pairs=json.dumps(pairs))
    return json.loads(out.getvalue())


def test_no_mail_reads_unanswered(bus):
    result = _run([{"session": "sid-a", "after": "2026-09-08T20:00:00Z"}])
    assert result == {"sid-a": {"answered": False, "watchdog_verdict": None}}


def test_mail_after_the_cutoff_reads_answered(bus):
    from fno.bus.log import Envelope, append

    append(
        Envelope.new(
            from_="king", to="sid-a", kind="text", body="go",
            ts="2026-09-08T21:00:00Z",
        )
    )
    result = _run([{"session": "sid-a", "after": "2026-09-08T20:00:00Z"}])
    assert result["sid-a"]["answered"] is True


def test_mail_before_the_cutoff_stays_unanswered(bus):
    from fno.bus.log import Envelope, append

    append(
        Envelope.new(
            from_="king", to="sid-a", kind="text", body="go",
            ts="2026-09-08T19:00:00Z",
        )
    )
    result = _run([{"session": "sid-a", "after": "2026-09-08T20:00:00Z"}])
    assert result["sid-a"]["answered"] is False


def test_mail_to_a_different_session_does_not_answer(bus):
    from fno.bus.log import Envelope, append

    append(
        Envelope.new(
            from_="king", to="sid-b", kind="text", body="go",
            ts="2026-09-08T21:00:00Z",
        )
    )
    result = _run([{"session": "sid-a", "after": "2026-09-08T20:00:00Z"}])
    assert result["sid-a"]["answered"] is False


def test_duplicate_session_keeps_the_oldest_after(bus):
    from fno.bus.log import Envelope, append

    append(
        Envelope.new(
            from_="king", to="sid-a", kind="text", body="go",
            ts="2026-09-08T20:30:00Z",
        )
    )
    # Two open rows for the same session: the board shows the oldest, so a
    # reply clearing the EARLIER cutoff must answer both.
    result = _run(
        [
            {"session": "sid-a", "after": "2026-09-08T20:00:00Z"},
            {"session": "sid-a", "after": "2026-09-08T21:00:00Z"},
        ]
    )
    assert result["sid-a"]["answered"] is True
