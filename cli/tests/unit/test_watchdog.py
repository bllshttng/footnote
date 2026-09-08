"""x-3ecf: :func:`session_verdict`, the thin per-session filter over
:func:`fno.agents.watchdog.run_sweep` the king board's blocked_child queue
calls instead of restating "is this session stuck" against raw transcripts.
"""
from __future__ import annotations

from datetime import datetime, timezone

from fno.agents.watchdog import GHOST, VERDICTS, Row, session_verdict

NOW_1840 = datetime(2026, 8, 16, 18, 40, 0, tzinfo=timezone.utc).timestamp()


def test_session_verdict_returns_the_matching_row_word():
    row = Row("aaaa1111-0000", "w1", "blocked", None, "/tmp/w1")
    verdict = session_verdict(
        "aaaa1111-0000",
        now_s=NOW_1840,
        rows_provider=lambda: ([row], []),
        transcript_fn=lambda sid: None,
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    # No transcript facts at all: the classifier's honest read is GHOST (no
    # evidence to classify from), not a fabricated LEAVE/WAKE.
    assert verdict == GHOST


def test_session_verdict_none_when_session_absent_from_the_sweep():
    row = Row("aaaa1111-0000", "w1", "blocked", None, "/tmp/w1")
    verdict = session_verdict(
        "bbbb2222-0000",
        now_s=NOW_1840,
        rows_provider=lambda: ([row], []),
        transcript_fn=lambda sid: None,
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    assert verdict is None


def test_session_verdict_none_on_a_failed_sweep():
    def _boom():
        raise RuntimeError("roster unreadable")

    assert session_verdict("aaaa1111-0000", rows_provider=_boom) is None


def test_session_verdict_passes_every_seam_through_to_run_sweep():
    row = Row("cccc3333-0000", "r1", "blocked", None, "/tmp/r1")
    seen: dict = {}

    def transcript_fn(sid: str):
        seen["transcript_sid"] = sid
        return None

    def claim_fn(node: str):
        seen["claim_node"] = node
        return {}

    verdict = session_verdict(
        "cccc3333-0000",
        now_s=NOW_1840,
        rows_provider=lambda: ([row], []),
        transcript_fn=transcript_fn,
        claim_fn=claim_fn,
        graph_fn=lambda: {},
    )
    assert verdict in VERDICTS
    assert seen["transcript_sid"] == "cccc3333-0000"
