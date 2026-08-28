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


def _row(*, tiled=None, chain=None, freshness="stale", reviewed_sha="c3"):
    """One review_coverage row: a local_attestation verdict plus tiling data."""
    data = {
        "pr": 42,
        "coverage": "covered",
        "reviewed_count": 1,
        "head_sha": "head",
        "verdicts": [
            {
                "producer": "local_attestation",
                "name": "code-review",
                "verdict": "reviewed",
                "reviewed_sha": reviewed_sha,
                "freshness": freshness,
                "scope": "attested_branch",
            }
        ],
    }
    if tiled is not None or chain is not None:
        data["range_tiling"] = {
            "tiled": bool(tiled),
            "gaps": [] if tiled else ["c1..c3"],
            "dropped": [],
            "chain_heads": list(chain or []),
        }
    return data


class TestTilingChainExemption:
    def test_tiled_chain_member_keeps_the_row_covered(self, monkeypatch):
        # The describes-test fails closed for everything: only the chain
        # exemption can carry this verdict, so the test proves the exemption
        # rather than a lucky freshness carry.
        monkeypatch.setattr(_reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: False)
        shaped = _reviews._shape_review_coverage(
            _row(tiled=True, chain=["c3"], reviewed_sha="c3", freshness="stale"),
            head="head",
            cwd=None,
        )
        assert shaped["coverage"] == "covered"
        assert shaped["review_state"] == "reviewed"
        assert shaped["reviewed_count"] == 1

    def test_no_tiling_data_keeps_today_single_sha_rule(self, monkeypatch):
        monkeypatch.setattr(_reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: False)
        shaped = _reviews._shape_review_coverage(
            _row(reviewed_sha="c3", freshness="stale"),
            head="head",
            cwd=None,
        )
        assert shaped["coverage"] == "uncovered"
        assert shaped["reviewed_count"] == 0

    def test_not_tiled_chain_does_not_rescue(self, monkeypatch):
        monkeypatch.setattr(_reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: False)
        shaped = _reviews._shape_review_coverage(
            _row(tiled=False, chain=["c3"], reviewed_sha="c3", freshness="stale"),
            head="head",
            cwd=None,
        )
        assert shaped["coverage"] == "uncovered"

    def test_chain_member_outside_the_chain_is_not_rescued(self, monkeypatch):
        monkeypatch.setattr(_reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: False)
        shaped = _reviews._shape_review_coverage(
            _row(tiled=True, chain=["c9"], reviewed_sha="c3", freshness="stale"),
            head="head",
            cwd=None,
        )
        assert shaped["coverage"] == "uncovered"

    def test_fresh_verdict_still_counts_without_any_chain(self, monkeypatch):
        monkeypatch.setattr(_reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: True)
        shaped = _reviews._shape_review_coverage(
            _row(reviewed_sha="head", freshness="fresh"),
            head="head",
            cwd=None,
        )
        assert shaped["coverage"] == "covered"


class TestTilingChainReader:
    def test_tiled_row_yields_its_chain_heads(self):
        assert _reviews._tiling_chain(_row(tiled=True, chain=["c1", "c3"])) == {"c1", "c3"}

    def test_not_tiled_row_yields_empty(self):
        assert _reviews._tiling_chain(_row(tiled=False, chain=["c1"])) == set()

    def test_row_without_tiling_yields_empty(self):
        assert _reviews._tiling_chain(_row()) == set()

    def test_malformed_tiling_yields_empty(self):
        assert _reviews._tiling_chain({"range_tiling": {"tiled": "yes"}}) == set()
        assert _reviews._tiling_chain({"range_tiling": None}) == set()
        assert _reviews._tiling_chain({"range_tiling": {"tiled": True, "chain_heads": "c1"}}) == set()

    def test_non_string_heads_are_dropped(self):
        assert _reviews._tiling_chain(
            {"range_tiling": {"tiled": True, "chain_heads": ["c1", 7, None, ""]}}
        ) == {"c1"}


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
    def test_a_later_pass_at_the_same_head_forces_a_recompute(
        self, monkeypatch, tmp_path
    ):
        """The measured failure. Asserting that a fresh attestation on a FRESH
        head works proves nothing: that path already worked, which is exactly
        why this went unnoticed for two readers on one PR."""
        _isolate_logs(monkeypatch, tmp_path)
        _write_log(
            tmp_path,
            _uncovered_row("2026-08-28T07:15:38Z"),
            _pass_attestation("2026-08-28T07:48:55Z"),
        )
        fired = []

        def fake_fire(pr_number, cwd, head):
            fired.append((pr_number, head))
            # The producer's job: it rewrites the row for this head.
            _write_log(
                tmp_path,
                _uncovered_row("2026-08-28T07:15:38Z"),
                _pass_attestation("2026-08-28T07:48:55Z"),
                _covered_row("2026-08-28T08:00:00Z"),
            )
            return True, ""

        monkeypatch.setattr(_reviews, "_fire_review_coverage_verb", fake_fire)
        data, note = _reviews.review_coverage_for_gate(1242, str(tmp_path), _H)

        # Positive markers only: the recompute RAN, and the answer flipped.
        assert fired == [(1242, _H)], fired
        assert data["coverage"] == "covered"
        assert "recomputed" in note

    def test_an_earlier_attestation_does_not_force_a_recompute(
        self, monkeypatch, tmp_path
    ):
        """The arm is narrow on purpose. An attestation OLDER than the row is
        evidence the row already accounted for, and recomputing on it would
        fire on every uncovered read forever - a refusal turned into a spin."""
        _isolate_logs(monkeypatch, tmp_path)
        _write_log(
            tmp_path,
            _pass_attestation("2026-08-28T06:00:00Z"),
            _uncovered_row("2026-08-28T07:15:38Z"),
        )
        fired = []
        monkeypatch.setattr(
            _reviews,
            "_fire_review_coverage_verb",
            lambda *a, **k: (fired.append(a) or (True, "")),
        )
        data, note = _reviews.review_coverage_for_gate(1242, str(tmp_path), _H)

        assert fired == []
        assert data["coverage"] == "uncovered"
        # The row still names what it was pinned to: the corollary-5 half holds
        # whether or not the recompute arm fires.
        assert note == "coverage row pinned to 74106361b at 2026-08-28T07:15:38Z"

    def test_a_fail_attestation_is_not_a_later_pass(self, monkeypatch, tmp_path):
        """Only a PASS overtakes an uncovered row. A later FAIL agrees with it."""
        _isolate_logs(monkeypatch, tmp_path)
        failed = _pass_attestation("2026-08-28T07:48:55Z")
        failed["data"]["verdict"] = "fail"
        _write_log(tmp_path, _uncovered_row("2026-08-28T07:15:38Z"), failed)
        fired = []
        monkeypatch.setattr(
            _reviews,
            "_fire_review_coverage_verb",
            lambda *a, **k: (fired.append(a) or (True, "")),
        )
        _reviews.review_coverage_for_gate(1242, str(tmp_path), _H)
        assert fired == []

    def test_the_pin_names_the_head_and_the_time(self, monkeypatch, tmp_path):
        """Corollary 5's field. A reader who cannot see the pin cannot tell a
        live NO from a stored NO whose reason expired."""
        _isolate_logs(monkeypatch, tmp_path)
        _write_log(tmp_path, _uncovered_row("2026-08-28T07:15:38Z"))
        monkeypatch.setattr(
            _reviews, "_fire_review_coverage_verb", lambda *a, **k: (False, "no binary")
        )
        _data, note = _reviews.review_coverage_for_gate(1242, str(tmp_path), _H)

        assert "coverage row pinned to 74106361b" in note
        assert "2026-08-28T07:15:38Z" in note

    def test_a_covered_row_carries_no_pin(self, monkeypatch, tmp_path):
        """The contract binds a cached NEGATIVE. A covered row that fails a
        conjunct refuses with a sentence that already names both heads, so
        pinning every covered receipt would be noise, not measurement."""
        _isolate_logs(monkeypatch, tmp_path)
        _write_log(tmp_path, _covered_row("2026-08-28T07:15:38Z"))
        _data, note = _reviews.review_coverage_for_gate(1242, str(tmp_path), _H)
        assert note == ""

    def test_a_covered_row_that_shapes_to_uncovered_still_carries_the_pin(
        self, monkeypatch, tmp_path
    ):
        """The pin keys on the SHAPED row, not the raw one.

        `_shape_review_coverage` rewrites a covered row to uncovered when its
        verdicts are stale or malformed. That is exactly a stored answer whose
        reason expired - the case the pin exists to explain - and keying on the
        raw row leaves it unpinned, because the raw row still says covered.
        """
        _isolate_logs(monkeypatch, tmp_path)
        row = _covered_row("2026-08-28T07:15:38Z")
        row["data"]["verdicts"] = []  # covered with no verdict proof
        _write_log(tmp_path, row)
        monkeypatch.setattr(
            _reviews, "_fire_review_coverage_verb", lambda *a, **k: (False, "no binary")
        )
        data, note = _reviews.review_coverage_for_gate(1242, str(tmp_path), _H)

        assert data["coverage"] == "uncovered", "fixture drifted: shaping did not fire"
        assert "coverage row pinned to 74106361b at 2026-08-28T07:15:38Z" in note, note

    def test_the_no_recompute_surface_reads_the_log_once(
        self, monkeypatch, tmp_path
    ):
        """The row and its pin come from ONE scan.

        These logs reach tens of MB and this path is the pre-push hook, so a
        second scan for a note the first read already has is I/O nobody asked
        for. Counted, not assumed.
        """
        _isolate_logs(monkeypatch, tmp_path)
        _write_log(tmp_path, _uncovered_row("2026-08-28T07:15:38Z"))
        scans = []
        real = _reviews.latest_review_coverage_row

        def counting(pr_number, cwd=None, project_events=None):
            scans.append(pr_number)
            return real(pr_number, cwd, project_events)

        monkeypatch.setattr(_reviews, "latest_review_coverage_row", counting)
        data, note = _reviews.review_coverage_for_head_row(1242, str(tmp_path), _H)

        assert len(scans) == 1, scans
        assert data["coverage"] == "uncovered"
        assert "coverage row pinned to 74106361b" in note, note
