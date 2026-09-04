"""Unit tests for the claim-keyed silence read (x-1182, defect two root cause B).

`fno agents claim list --silent-after` is the harness-neutral candidate set a
stalled worker's own claim always belongs to, unlike `fleet_rows`
(cli/src/fno/agents/watchdog.py), which only ever sees claude rows and is
blind to codex, opencode, and hand-started sessions by construction.

The classifier under test is pure over injected rows and an injected age
lookup, so these tests need no live fleet, no roster, and no real transcript.
"""
from __future__ import annotations

from typing import Optional

import pytest


def _row(key: str, holder: str, *, state: str = "live", metadata: Optional[dict] = None) -> dict:
    row: dict = {"key": key, "state": state, "holder": holder}
    if metadata is not None:
        row["metadata"] = metadata
    return row


def test_silent_claim_is_reported_by_name():
    from fno.claims.cli import _claim_silence_report

    rows = [_row("node:x-884f", "target-session:abc123", metadata={"worktree": "/tmp/wt"})]

    def age_lookup(session_id: str, cwd: str) -> Optional[float]:
        assert session_id == "abc123"
        assert cwd == "/tmp/wt"
        return 3000.0  # 50 minutes

    report = _claim_silence_report(rows, threshold_s=900.0, age_lookup=age_lookup)

    assert report.scanned == 1
    assert [s.key for s in report.silent] == ["node:x-884f"]
    assert report.silent[0].age_s == 3000.0
    assert report.unreadable == []


def test_claim_clears_once_the_transcript_advances():
    from fno.claims.cli import _claim_silence_report

    rows = [_row("node:x-884f", "target-session:abc123", metadata={"worktree": "/tmp/wt"})]

    def age_lookup(session_id: str, cwd: str) -> Optional[float]:
        return 60.0  # one minute, well under the threshold

    report = _claim_silence_report(rows, threshold_s=900.0, age_lookup=age_lookup)

    assert report.scanned == 1
    assert report.silent == []
    assert report.unreadable == []


def test_zero_silent_claims_still_reports_the_scanned_count():
    from fno.claims.cli import _claim_silence_report

    rows = [
        _row("node:a", "target-session:s1", metadata={"worktree": "/tmp/a"}),
        _row("node:b", "target-session:s2", metadata={"worktree": "/tmp/b"}),
    ]

    report = _claim_silence_report(rows, threshold_s=900.0, age_lookup=lambda *_: 10.0)

    assert report.scanned == 2
    assert report.silent == []
    assert report.unreadable == []


def test_empty_fleet_reports_zero_scanned_not_nothing():
    from fno.claims.cli import _claim_silence_report

    report = _claim_silence_report([], threshold_s=900.0, age_lookup=lambda *_: None)

    assert report.scanned == 0
    assert report.silent == []
    assert report.unreadable == []


@pytest.mark.parametrize(
    "holder,metadata",
    [
        ("not-a-parseable-holder-shape", {"worktree": "/tmp/wt"}),  # unresolvable session id
        ("target-session:abc123", {}),  # no worktree/cwd in metadata
    ],
)
def test_unresolvable_holder_or_cwd_is_unreadable_not_silent_or_healthy(holder, metadata):
    from fno.claims.cli import _claim_silence_report

    rows = [_row("node:x", holder, metadata=metadata)]

    report = _claim_silence_report(
        rows, threshold_s=900.0, age_lookup=lambda *_: pytest.fail("should not be called")
    )

    assert report.scanned == 1
    assert report.silent == []
    assert [u.key for u in report.unreadable] == ["node:x"]


def test_unreadable_transcript_is_unreadable_not_silent_or_healthy():
    from fno.claims.cli import _claim_silence_report

    rows = [_row("node:x", "target-session:abc123", metadata={"worktree": "/tmp/wt"})]

    report = _claim_silence_report(rows, threshold_s=900.0, age_lookup=lambda *_: None)

    assert report.scanned == 1
    assert report.silent == []
    assert [u.key for u in report.unreadable] == ["node:x"]


def test_non_live_claims_are_not_scanned():
    from fno.claims.cli import _claim_silence_report

    rows = [
        _row("node:x", "target-session:abc123", state="stale", metadata={"worktree": "/tmp/wt"}),
        _row("node:y", "target-session:def456", state="suspect", metadata={"worktree": "/tmp/wt"}),
    ]

    report = _claim_silence_report(rows, threshold_s=900.0, age_lookup=lambda *_: 3000.0)

    # Only genuinely "live" claims are silence candidates; suspect/stale are
    # a different lane's concern (reap), not this one's.
    assert report.scanned == 0
    assert report.silent == []


def test_render_scanned_line_is_always_positive_even_at_zero_silent():
    from fno.claims.cli import _claim_silence_report, _render_claim_silence_report

    report = _claim_silence_report([], threshold_s=900.0, age_lookup=lambda *_: None)
    line = _render_claim_silence_report(report, threshold_s=900.0)

    # x-1182: a detector that prints nothing on a healthy fleet is
    # indistinguishable from one that never ran, which is how the fifty
    # minutes went unnoticed. Assert the POSITIVE marker, not an absence.
    assert "scanned 0 live claim(s)" in line
    assert "0 silent" in line
