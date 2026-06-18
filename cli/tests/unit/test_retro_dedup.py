"""Unit tests for retro dedup (Wave 3.3, US5)."""
from __future__ import annotations

from fno.retro import dedup
from fno.retro.types import KIND_REVIEW, TIER_NODE, Candidate


def _cand(text: str, *, pr: int = 343, sid: str = "c1") -> Candidate:
    return Candidate(
        title="t",
        body=text,
        tier=TIER_NODE,
        priority="p1",
        source_pr=pr,
        source_id=sid,
        extra={"finding_text": text},
    )


def test_hash_stable_and_badge_insensitive():
    """AC5-EDGE basis: same issue, different reviewer badges -> same hash."""
    h_gemini = dedup.content_hash("![high] the lock never releases on error")
    h_codex = dedup.content_hash("![P1 Badge] the lock never releases on error")
    assert h_gemini == h_codex
    # Whitespace/case differences also normalize away.
    assert dedup.content_hash("the lock never releases on error") == dedup.content_hash(
        "The  Lock\nNever  Releases On Error"
    )


def test_ac5_edge_two_reviewers_same_issue_one_node():
    """AC5-EDGE: two reviewers flag the same issue -> one candidate kept."""
    c1 = _cand("![high] the lock never releases on error", sid="gem-1")
    c2 = _cand("![P1 Badge] the lock never releases on error", sid="cdx-2")
    kept, skipped = dedup.dedup_candidates([c1, c2])
    assert len(kept) == 1
    assert len(skipped) == 1


def test_ac5_hp_existing_node_blocks_recreate():
    """AC5-HP: a PR already triaged -> candidate matching an existing node is skipped."""
    c = _cand("flaky retry loop")
    h = dedup.content_hash("flaky retry loop")
    existing = {f"343:{h}"}
    kept, skipped = dedup.dedup_candidates([c], existing_keys=existing)
    assert kept == []
    assert len(skipped) == 1


def test_existing_keys_parsed_from_node_details():
    h = dedup.content_hash("flaky retry loop")
    nodes = [
        {"id": "ab-1", "details": f"some body\n\n{dedup.trailer(343, h)}"},
        {"id": "ab-2", "details": "no trailer here"},
        {"id": "ab-3", "details": None},
    ]
    keys = dedup.existing_keys_from_nodes(nodes)
    assert keys == {f"343:{h}"}


def test_idempotent_round_trip_via_trailer():
    """A landed node's trailer makes a re-run skip the same candidate (AC5-HP idempotency)."""
    c = _cand("a genuine bug in the parser")
    [c_hashed] = dedup.assign_hashes([_cand("a genuine bug in the parser")])
    landed_node = {
        "id": "ab-new",
        "details": f"body\n\n{dedup.trailer(c_hashed.source_pr, c_hashed.content_hash)}",
    }
    existing = dedup.existing_keys_from_nodes([landed_node])
    kept, skipped = dedup.dedup_candidates([c], existing_keys=existing)
    assert kept == []
    assert len(skipped) == 1


def test_ac5_fr_deleted_node_is_recreated():
    """AC5-FR: dedup keys off LIVE nodes; a manually-deleted node is not 'existing'."""
    c = _cand("a finding that was filed then deleted")
    # existing_nodes is empty (the human deleted the previously-landed node), so
    # its key is absent and the candidate is re-kept - documented behavior.
    kept, skipped = dedup.dedup_candidates([c], existing_keys=dedup.existing_keys_from_nodes([]))
    assert len(kept) == 1
    assert skipped == []


def test_different_pr_not_deduped():
    """Same text on a different PR is a distinct key (source_pr is part of the key)."""
    c1 = _cand("same text", pr=1)
    c2 = _cand("same text", pr=2)
    kept, _ = dedup.dedup_candidates([c1, c2])
    assert len(kept) == 2
