"""The Python coverage reader's tiled-chain exemption (AC4, Python half).

The Rust producer walks the attestation ranges with git and writes the
`range_tiling` answer onto the `review_coverage` row; Python READS it (the
Ownership rule) so both surfaces apply the same chain exemption. Without the
exemption a chain member's single-sha freshness reads stale and the shaper
invalidates the row - the exact one-artifact-voids-per-fix loop the tiling
exists to end.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.pr import _reviews


# ---- the cached negative expires, and names what it was pinned to ----
#
# An UNCOVERED row pinned to the CURRENT head passes every freshness check the
# reader applied, because the head matches. So it was never refreshed, and a
# valid head-pinned pass attestation landing after it was ignored forever.
# Measured on one PR: the row was written at 07:15:38Z, the attestation for the
# same head landed at 07:48:55Z, and the gate still read uncovered.

_H = "74106361b0000000000000000000000000000000"


def _write_log(tmp_path, *events):
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in events) + "\n", encoding="utf-8"
    )


def _uncovered_row(ts, head=_H, pr=1242):
    return {
        "ts": ts,
        "type": "review_coverage",
        "source": "hook",
        "data": {
            "pr": pr,
            "coverage": "uncovered",
            "review_state": "unreviewed",
            "reviewed_count": 0,
            "head_sha": head,
        },
    }


def _covered_row(ts, head=_H, pr=1242):
    return {
        "ts": ts,
        "type": "review_coverage",
        "source": "hook",
        "data": {
            "pr": pr,
            "coverage": "covered",
            "review_state": "reviewed",
            "reviewed_count": 1,
            "head_sha": head,
            # A modern producer resolves a rung on every covered row; a row
            # without this key reads as written by a pre-posture producer and
            # the gate demands a recompute, which is its own test below.
            "review_posture": {
                "posture": "self_review",
                "rank": 3,
                "source": "legacy",
                "posture_satisfied": True,
                "posture_gaps": [],
            },
            # The shaper refuses a covered row carrying no verdict proof, so a
            # fixture without this shapes back to uncovered and the test would
            # read as "the recompute did not help" when it did.
            "verdicts": [
                {
                    "producer": "local_attestation",
                    "name": "code-review",
                    "verdict": "reviewed",
                    "reviewed_sha": head,
                    "freshness": "fresh",
                    "attestation_origin": "other_session",
                }
            ],
        },
    }


def _pass_attestation(ts, head=_H):
    return {
        "ts": ts,
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": head,
            "verdict": "pass",
            "session_id": "s-other",
            "branch": "feature/x-e555",
        },
    }


class TestPosturelessRowRecomputeScope:
    """A covered row with no review_posture is unusable to a GATE (the
    producer predates the field), but only a caller that can act on the
    producer-upgrade refusal pays the recompute; a receipt surface reports
    the row as-is instead of rewriting the same postureless row every read."""

    def _postureless_world(self, monkeypatch, tmp_path):
        _isolate_logs(monkeypatch, tmp_path)
        row = _covered_row("2026-08-28T08:00:00Z")
        del row["data"]["review_posture"]
        _write_log(tmp_path, row)
        fired = []
        return fired



def _isolate_logs(monkeypatch, tmp_path):
    """Read only the fixture log: no global mirror, no repo slug."""
    monkeypatch.setattr(
        _reviews,
        "_coverage_logs",
        lambda cwd=None, project_events=None: (
            Path(tmp_path) / ".fno" / "events.jsonl",
            None,
            None,
        ),
    )


class TestCachedNegativeNamesItsInput:

    def test_split_pin_note_separates_two_different_claims(self):
        """One says what an instrument did, the other what a stored answer
        rests on. A consumer keyed on the first must not receive the second."""
        joined = "recomputed; coverage row pinned to abc123def at 2026-08-28T07:15:38Z"
        recompute, pin = _reviews.split_pin_note(joined)
        assert recompute == "recomputed"
        assert pin == "coverage row pinned to abc123def at 2026-08-28T07:15:38Z"
        assert _reviews.split_pin_note("") == ("", "")
        assert _reviews.split_pin_note("recomputed") == ("recomputed", "")
