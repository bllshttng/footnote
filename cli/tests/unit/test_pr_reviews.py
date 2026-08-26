"""The Python coverage reader's tiled-chain exemption (AC4, Python half).

The Rust producer walks the attestation ranges with git and writes the
`range_tiling` answer onto the `review_coverage` row; Python READS it (the
Ownership rule) so both surfaces apply the same chain exemption. Without the
exemption a chain member's single-sha freshness reads stale and the shaper
invalidates the row - the exact one-artifact-voids-per-fix loop the tiling
exists to end.
"""
from __future__ import annotations

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
