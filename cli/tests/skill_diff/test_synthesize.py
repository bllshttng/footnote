"""Synthesis-output parser coverage (the one non-deterministic seam's contract)."""
from __future__ import annotations

import pytest

from fno.skill_diff import synthesize


def test_parse_fenced_json():
    p = synthesize.parse_proposal(
        '```json\n{"verdict":"propose_pr","hunks":['
        '{"file":"skills/blueprint/SKILL.md","old_text":"a","new_text":"b","cited_finding_ids":["s1"],"rationale":"r"}]}\n```'
    )
    assert p.verdict == "propose_pr"
    assert p.hunks[0]["cited_finding_ids"] == ["s1"]


def test_parse_bare_object():
    p = synthesize.parse_proposal('noise {"verdict":"no_diff_helps","no_diff_reason":"architectural"} trailer')
    assert p.verdict == "no_diff_helps" and p.no_diff_reason == "architectural"


def test_hunk_defaults_are_normalized():
    p = synthesize.parse_proposal('{"verdict":"propose_pr","hunks":[{"file":"x"}]}')
    h = p.hunks[0]
    assert h["old_text"] == "" and h["new_text"] == "" and h["cited_finding_ids"] == []


def test_unknown_verdict_raises():
    with pytest.raises(synthesize.ProposalParseError):
        synthesize.parse_proposal('{"verdict":"ship_it"}')


def test_no_json_raises():
    with pytest.raises(synthesize.ProposalParseError):
        synthesize.parse_proposal("the model refused")


def test_malformed_hunk_raises():
    with pytest.raises(synthesize.ProposalParseError):
        synthesize.parse_proposal('{"verdict":"propose_pr","hunks":[{"no_file":1}]}')


def test_build_prompt_bounds_evidence_and_flags_driver_skill():
    prompt = synthesize.build_prompt(
        skill_id="fno:blueprint",  # a driver skill
        skill_files={"skills/blueprint/SKILL.md": "body"},
        findings=[{"dimension": "structural_validity", "verdict": "fail",
                   "corpus_item_id": "s1", "evidence": "x" * 900}],
        ranking=[{"dimension": "structural_validity", "fail_count": 2}],
        history=[],
        additive_threshold=15,
    )
    assert "DRIVER skill" in prompt
    assert "x" * 500 in prompt and "x" * 501 not in prompt  # evidence truncated to 500
