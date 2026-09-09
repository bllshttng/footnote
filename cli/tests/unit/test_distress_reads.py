"""x-3ecf: the `distress-verdicts` board-collection helper.

The king board's blocked_child queue shells this command once per board
build to learn the fleet watchdog's current word for each blocked session,
as informational enrichment (Change 2: one classifier, two callers). The
mail-answered signal itself (AC3-EDGE) is read natively in Rust.
"""
from __future__ import annotations

import contextlib
import io
import json

from fno.agents.distress_reads import cmd_distress_verdicts


def _run(sessions: list) -> dict:
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        cmd_distress_verdicts(sessions=json.dumps(sessions))
    return json.loads(out.getvalue())


def test_an_unknown_session_reads_null():
    assert _run(["sid-a"]) == {"sid-a": None}


def test_a_non_string_id_is_dropped_silently():
    assert _run(["sid-a", 5, None]) == {"sid-a": None}


def test_a_failed_sweep_still_answers_null_for_every_session(monkeypatch):
    def _boom():
        raise RuntimeError("roster unreadable")

    monkeypatch.setattr("fno.agents.watchdog.run_sweep", _boom)
    assert _run(["sid-a", "sid-b"]) == {"sid-a": None, "sid-b": None}


def test_a_matching_row_id_carries_its_verdict(monkeypatch):
    def _sweep():
        return {"verdicts": [{"row_id": "sid-a", "verdict": "ghost"}]}, []

    monkeypatch.setattr("fno.agents.watchdog.run_sweep", _sweep)
    assert _run(["sid-a", "sid-b"]) == {"sid-a": "ghost", "sid-b": None}
