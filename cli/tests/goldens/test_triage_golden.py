"""Golden receipts: triage validate / apply (the deterministic proposal door)."""
from __future__ import annotations

import json

from tests.goldens._door import door, make_sandbox, seed_node, warm

PROPOSAL = {
    "dependencies": [],
    "priority_changes": [{"id": "x-fff66000", "to": "p1", "reason": "operator asked"}],
    "duplicates": [],
    "defer": [],
    "candidates": [],
    "ideas": [],
    "scope": "all projects",
}

BAD_PRIORITY_PROPOSAL = {
    "dependencies": [],
    "priority_changes": [{"id": "x-fff66000", "priority": "p1"}],
    "duplicates": [],
    "defer": [],
    "candidates": [],
    "ideas": [],
    "scope": "all projects",
}


def _write(tmp_path, payload) -> str:
    proposal = tmp_path / "proposal.json"
    proposal.write_text(json.dumps(payload))
    return str(proposal)


def test_validate_a_missing_file_refuses(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff66000")])
    warm(root, "x-fff66000")
    code, out, err = door(root, ["triage", "validate", "/nonexistent/proposal.json"])
    assert code == 2, err
    assert err == "Error: proposal file not found: /nonexistent/proposal.json\n", err


def test_validate_a_conforming_proposal_answers_clean(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff66000")])
    warm(root, "x-fff66000")
    path = _write(tmp_path, PROPOSAL)
    code, out, err = door(root, ["triage", "validate", path])
    assert code == 0, err
    parsed = json.loads(out)
    assert parsed["validation_errors"] == []
    assert parsed["priority_changes"] == PROPOSAL["priority_changes"]


def test_validate_drops_a_malformed_priority_change_and_names_it(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff66000")])
    warm(root, "x-fff66000")
    path = _write(tmp_path, BAD_PRIORITY_PROPOSAL)
    code, out, err = door(root, ["triage", "validate", path])
    assert code == 3, err
    parsed = json.loads(out)
    assert parsed["priority_changes"] == []
    assert parsed["validation_errors"] == ["priority_change invalid priority: None"]


def test_apply_stamps_the_priority_change_and_counts_it(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff66000", priority="p3")])
    warm(root, "x-fff66000")
    path = _write(tmp_path, PROPOSAL)
    code, out, err = door(root, ["triage", "apply", path])
    assert code == 0, err
    parsed = json.loads(out)
    assert parsed["applied"]["priority_changes"] == 1
    assert parsed["dropped_due_to_validation"] == 0


def test_apply_of_an_invalid_proposal_drops_and_reports(tmp_path):
    root = make_sandbox(tmp_path, [seed_node("x-fff66000")])
    warm(root, "x-fff66000")
    path = _write(tmp_path, BAD_PRIORITY_PROPOSAL)
    code, out, err = door(root, ["triage", "apply", path])
    assert code == 3, err
    parsed = json.loads(out)
    assert parsed["applied"]["priority_changes"] == 0
    assert parsed["dropped_due_to_validation"] == 1
