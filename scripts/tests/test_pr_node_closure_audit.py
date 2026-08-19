"""Tests for scripts/metrics/pr-node-closure-audit.py (x-59a6 task 1.1).

Covers AC1-HP (an explicit close claim classifies correctly, source text
preserved), AC1-EDGE (dependency/collision language is never a claim, even
with a close verb nearby), and AC1-ERR (an unreadable PR body is recorded,
never silently dropped from the denominator).
"""
from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

_MODULE_PATH = (
    Path(__file__).resolve().parents[2] / "scripts" / "metrics" / "pr-node-closure-audit.py"
)
_spec = importlib.util.spec_from_file_location("pr_node_closure_audit", _MODULE_PATH)
audit = importlib.util.module_from_spec(_spec)
sys.modules["pr_node_closure_audit"] = audit
_spec.loader.exec_module(audit)  # type: ignore[union-attr]


NODE_IDS = {"x-5b99", "x-62a1", "x-3a91", "x-b28b", "x-aaaa", "x-bbbb"}


def test_classify_close_claim():
    assert audit.classify_mention("This PR closes x-5b99 and x-62a1.") == "close_claim"


def test_classify_dependency_wins_over_close_verb():
    # A close verb elsewhere in the sentence must not override dependency
    # language naming the SAME sentence (x-59a6 king correction).
    sentence = "x-3a91 is blocked_by this and gets its own Part 2 plan."
    assert audit.classify_mention(sentence) == "dependency"


def test_classify_collision():
    sentence = 'branch x-3a91 (client.rs, unmerged) are untouched.'
    assert audit.classify_mention(sentence) == "collision"


def test_classify_follow_up():
    sentence = "Also filed x-9999 as a follow-up for the remaining case."
    assert audit.classify_mention(sentence) == "follow_up"


def test_classify_other_with_no_signal():
    assert audit.classify_mention("x-9999 is mentioned here.") == "other"


def test_classify_close_claim_bare_fix_and_fixed():
    # Review fix (x-59a6): the old regex `fixe?s` required a trailing 's',
    # missing the bare "fix"/"fixed" forms of the standard close-verb set.
    assert audit.classify_mention("This will fix x-5b99 as well.") == "close_claim"
    assert audit.classify_mention("Fixed x-5b99 in this pass.") == "close_claim"


# ---------------------------------------------------------------------------
# scan_pr
# ---------------------------------------------------------------------------


def test_scan_pr_ac1_hp_close_claim_preserves_source_text():
    body = "Fixes the thing.\n\nCloses x-5b99 and closes x-62a1 too.\n"
    mentions = audit.scan_pr(836, body, NODE_IDS)
    assert len(mentions) == 1  # x-5b99 is the FIRST (primary); x-62a1 is secondary
    m = mentions[0]
    assert m.node_id == "x-62a1"
    assert m.classification == "close_claim"
    assert "x-62a1" in m.source_text


def test_scan_pr_ac1_edge_dependency_and_collision_never_claims():
    body_827 = "Ships x-5b99 first.\nx-3a91 is blocked_by this and gets its own Part 2 plan.\n"
    mentions = audit.scan_pr(827, body_827, NODE_IDS)
    ids = {m.node_id: m.classification for m in mentions}
    assert ids.get("x-3a91") == "dependency"

    body_848 = "x-5b99 first.\nbranch x-3a91 (client.rs, unmerged) are untouched.\n"
    mentions2 = audit.scan_pr(848, body_848, NODE_IDS)
    ids2 = {m.node_id: m.classification for m in mentions2}
    assert ids2.get("x-3a91") == "collision"


def test_scan_pr_trailer_always_close_claim_regardless_of_prose():
    # Even if the body ALSO has dependency-sounding prose elsewhere, an id on
    # the exact trailer is close_claim - the trailer is the runtime-binding
    # grammar and must never be under-counted here.
    body = (
        "x-5b99 is the primary.\n"
        "x-62a1 depends_on some other work mentioned in passing.\n"
        "Backlog-Closure: x-5b99 x-62a1\n"
    )
    mentions = audit.scan_pr(900, body, NODE_IDS)
    m = next(m for m in mentions if m.node_id == "x-62a1")
    assert m.classification == "close_claim"


def test_scan_pr_trailer_comma_with_no_space_still_close_claim():
    # Round-10 review fix: the runtime binder tokenizes on "," same as
    # whitespace (parse_closure_trailer's `.replace(",", " ")`), so a
    # no-space comma-joined trailer binds both ids at merge time. The audit
    # must classify the secondary as close_claim too, not undercount it.
    body = (
        "x-5b99 is the primary.\n"
        "x-3a91 is mentioned in passing.\n"
        "Backlog-Closure: x-5b99,x-3a91\n"
    )
    mentions = audit.scan_pr(901, body, NODE_IDS)
    m = next(m for m in mentions if m.node_id == "x-3a91")
    assert m.classification == "close_claim"


def test_scan_pr_ignores_non_graph_ids():
    # A hex-shaped token that is NOT a real graph node (e.g. a carveout id
    # coincidentally matching the format) must never count as a mention.
    body = "x-5b99 first. cv-1234 is unrelated. x-9999 is not in the graph.\n"
    mentions = audit.scan_pr(1, body, NODE_IDS)
    assert mentions == []  # only one REAL secondary would be needed; here there are none


def test_scan_pr_does_not_match_an_id_glued_inside_a_longer_token():
    # Round-6 review fix: a plain substring search for "x-3a91" would match
    # "x-3a910" (glued, no boundary) before ever reaching the real, separate
    # "x-3a91 is blocked_by this" sentence - misclassifying the mention off
    # the wrong unit's text. Word-boundary matching must skip the decoy.
    body = (
        "Ships x-5b99 first.\n"
        "See old ref x-3a910 for context.\n"
        "x-3a91 is blocked_by this.\n"
    )
    mentions = audit.scan_pr(1, body, NODE_IDS)
    m = next(m for m in mentions if m.node_id == "x-3a91")
    assert m.classification == "dependency"
    assert "blocked_by" in m.source_text
    assert "x-3a910" not in m.source_text


def test_scan_pr_single_mention_is_not_a_secondary():
    # A body naming only ONE real node has no secondary to classify.
    body = "This just fixes x-5b99.\n"
    assert audit.scan_pr(2, body, NODE_IDS) == []


def test_scan_pr_b28b_prs_620_and_740():
    # Mirrors the plan's explicit instruction to verify x-b28b in these two PRs.
    body_620 = "Ships x-5b99. Also touches x-b28b as a dependency: blocked_by upstream work.\n"
    m620 = audit.scan_pr(620, body_620, NODE_IDS)
    assert {m.node_id: m.classification for m in m620} == {"x-b28b": "dependency"}

    body_740 = "Closes x-5b99 and closes x-b28b.\n"
    m740 = audit.scan_pr(740, body_740, NODE_IDS)
    assert {m.node_id: m.classification for m in m740} == {"x-b28b": "close_claim"}


# ---------------------------------------------------------------------------
# run_audit (pure aggregation - no I/O)
# ---------------------------------------------------------------------------


def test_run_audit_counts_and_x_b28b_check():
    prs = [
        {"number": 620, "body": "Ships x-5b99. Also touches x-b28b: blocked_by upstream.\n"},
        {"number": 740, "body": "Closes x-5b99 and closes x-b28b.\n"},
        {"number": 1, "body": "This just fixes x-5b99.\n"},  # no secondary
    ]
    result = audit.run_audit(prs, NODE_IDS)
    assert result.status == "complete"
    assert result.corpus_size == 3
    assert result.prs_with_secondaries == 2
    assert result.counts["dependency"] == 1
    assert result.counts["close_claim"] == 1
    assert result.x_b28b_check == {620: "dependency", 740: "close_claim"}


def test_run_audit_ac1_err_unreadable_body_recorded_not_dropped():
    prs = [
        {"number": 1, "body": "Closes x-5b99 and closes x-62a1."},
        {"number": 2, "body": None},  # unreadable: gh omitted the body
    ]
    result = audit.run_audit(prs, NODE_IDS)
    # AC1-ERR: the denominator is NOT shrunk - corpus_size still counts it.
    assert result.corpus_size == 2
    assert result.unreadable == [2]
    assert result.status == "complete_with_unreadable"
    assert result.counts["close_claim"] == 1


def test_as_dict_caps_specimens_and_reports_counts_for_every_bucket():
    prs = [{"number": n, "body": f"Closes x-5b99 and closes x-62a1 (#{n})."} for n in range(1, 5)]
    result = audit.run_audit(prs, NODE_IDS)
    payload = result.as_dict(specimen_cap=2)
    assert set(payload["counts"]) == set(audit.CLASSIFICATIONS)
    assert len(payload["specimens"]["close_claim"]) == 2  # capped, not silently truncated to 0
    assert payload["counts"]["close_claim"] == 4  # the full count survives the cap
