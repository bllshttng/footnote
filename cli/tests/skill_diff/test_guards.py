"""Mechanical-guard coverage."""
from __future__ import annotations

from fno.skill_diff import guards


def test_filter_drops_uncited_hunks():  # AC2-ERR
    hunks = [
        {"cited_finding_ids": ["f1"], "new_text": "keep"},
        {"cited_finding_ids": [], "new_text": "drop-empty"},
        {"cited_finding_ids": [""], "new_text": "drop-blank"},
        {"new_text": "drop-missing"},
    ]
    kept, dropped = guards.filter_cited_hunks(hunks)
    assert [h["new_text"] for h in kept] == ["keep"]
    assert len(dropped) == 3


def test_additive_only_needs_justification_true_when_over_threshold():  # AC4-UI
    hunks = [{"old_text": "", "new_text": "x\n" * (guards.ADDITIVE_LINE_THRESHOLD + 1)}]
    assert guards.additive_only_needs_justification(hunks)


def test_additive_with_a_removal_does_not_need_justification():
    hunks = [{"old_text": "gone", "new_text": "x\n" * 50}]
    assert not guards.additive_only_needs_justification(hunks)


def test_small_addition_below_threshold_ok():
    hunks = [{"old_text": "", "new_text": "one\ntwo"}]
    assert not guards.additive_only_needs_justification(hunks)


def test_count_lines_ignores_blanks():
    added, removed = guards.count_lines([{"old_text": "a\n\nb", "new_text": "c\n\n\nd\ne"}])
    assert (added, removed) == (3, 2)


def test_bloat_flag_trips_and_clears():  # AC5-UI
    over = [{"added_lines": 200, "removed_lines": 10}]
    assert guards.bloat_flag(over)["net_growth"] == 190
    under = [{"added_lines": 10, "removed_lines": 5}]
    assert guards.bloat_flag(under) is None


def test_bloat_flag_uses_only_trailing_window():
    # Old huge growth outside the window must not count.
    hist = [{"added_lines": 999, "removed_lines": 0}] + [
        {"added_lines": 1, "removed_lines": 0}
    ] * guards.BLOAT_WINDOW
    assert guards.bloat_flag(hist) is None


def test_redaction_catches_internal_and_fno_paths():  # A3
    assert "internal/" in guards.redaction_violations("cites internal/fno/plan.md", [])
    assert "~/.fno" in guards.redaction_violations("path ~/.fno/graph.json", [])


def test_redaction_catches_foreign_project_name_but_not_fno():  # A3
    hits = guards.redaction_violations("touches acme and fno", ["acme", "fno"])
    assert hits == ["project:acme"]  # fno (self) never flagged


def test_redaction_token_boundary():
    # "acme" must not match inside "acmefoundation".
    assert guards.redaction_violations("the acmefoundation ships", ["acme"]) == []
