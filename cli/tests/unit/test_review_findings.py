"""The finding classifier: one fail-closed rule over three payload shapes.

Every criterion here asserts a POSITIVE marker - the literal returned value,
the literal count, the literal record count - never that a counter stayed at
zero. An absence also describes a run where the classifier never ran, and a
suite that cannot tell those apart is the trap AC1-ERR exists to close: a
normalizer that silently dropped all five inputs would pass any test that
only checked a count.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.review.findings import (
    BLOCKING,
    NONBLOCKING,
    FindingsNormalizeError,
    FindingRecord,
    classify,
    finding_key,
    normalize,
    resolve_nonblocking_categories,
    summarize,
)

FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "review_findings"


def _full(**overrides) -> FindingRecord:
    """A record carrying every required field, per the ReportFindings contract."""
    base = {
        "category": None,
        "verdict": None,
        "file": "a.py",
        "line": 1,
        "summary": "a summary",
        "failure_scenario": "a real failure scenario",
    }
    base.update(overrides)
    return FindingRecord(**base)


class TestAC1Marker:
    """AC1-MARKER: a correctness finding tagged style still blocks."""

    def test_confirmed_verdict_tagged_style_returns_literal_blocking(self) -> None:
        record = _full(category="style", verdict="CONFIRMED")
        assert classify(record) == BLOCKING
        assert classify(record) == "blocking"

    def test_whitespace_padded_confirmed_still_blocks(self) -> None:
        assert classify(_full(category="docs", verdict=" confirmed ")) == "blocking"

    def test_plausible_verdict_tagged_style_is_nonblocking(self) -> None:
        # PLAUSIBLE is the not-survived-verification arm of the one closed
        # enum; the category decides for it, and style is on the allowlist.
        assert classify(_full(category="style", verdict="PLAUSIBLE")) == "nonblocking"


class TestAC1HP:
    def test_summarize_one_typo_no_verdict(self) -> None:
        summary = summarize([_full(category="typo")])
        assert summary.blocking_count == 0
        assert summary.nonblocking_count == 1


class TestAC1ERR:
    """Five inputs, five literal BLOCKING values, one assert per input.

    A normalizer that silently dropped records cannot pass five separate
    literal assertions; a loop over the inputs with one assert at the end
    could pass on an empty list.
    """

    def test_absent_category_key_blocks(self) -> None:
        assert classify(_full()) == "blocking"

    def test_empty_string_category_blocks(self) -> None:
        assert classify(_full(category="")) == "blocking"

    def test_whitespace_category_blocks(self) -> None:
        assert classify(_full(category="   ")) == "blocking"

    def test_correctness_category_blocks(self) -> None:
        assert classify(_full(category="correctness")) == "blocking"

    def test_never_before_seen_category_blocks(self) -> None:
        assert classify(_full(category="vibes")) == "blocking"

    def test_empty_failure_scenario_blocks(self) -> None:
        record = _full(category="typo", failure_scenario="")
        assert classify(record) == "blocking"
        assert classify(_full(category="typo", failure_scenario="  ")) == "blocking"

    def test_missing_summary_blocks(self) -> None:
        assert classify(_full(category="typo", summary=None)) == "blocking"

    def test_missing_file_blocks(self) -> None:
        assert classify(_full(category="typo", file=None)) == "blocking"


class TestAC1Edge:
    def test_codex_unrecognized_fields_all_unmappable_and_blocking(self) -> None:
        payload = json.loads(
            (FIXTURES / "codex_review_output_unmappable.json").read_text(encoding="utf-8")
        )
        records = normalize(payload, "codex_review_output")
        assert len(records) == 3
        assert [r.unmappable for r in records] == [True, True, True]
        assert [classify(r) for r in records] == ["blocking", "blocking", "blocking"]

    def test_codex_shared_vocabulary_maps(self) -> None:
        records = normalize(
            {"findings": [{"file": "a.py", "summary": "boom"}]},
            "codex_review_output",
        )
        assert len(records) == 1
        assert records[0].unmappable is False
        # file+summary map but failure_scenario is absent: still blocking.
        assert classify(records[0]) == "blocking"

    def test_non_dict_array_element_is_unmappable_and_counted(self) -> None:
        records = normalize({"findings": ["oops", 42]}, "codex_review_output")
        assert len(records) == 2
        assert all(r.unmappable for r in records)
        assert all(classify(r) == "blocking" for r in records)


class TestNormalizers:
    def test_report_findings_payload_from_fixture(self) -> None:
        payload = json.loads(
            (FIXTURES / "report_findings_payload.json").read_text(encoding="utf-8")
        )
        records = normalize(payload, "report_findings")
        assert len(records) == 2
        summary = summarize(records)
        assert summary.blocking_count == 1
        assert summary.nonblocking_count == 1
        assert summary.category_histogram == {"nit": 1, "correctness": 1}

    def test_report_findings_absent_findings_key_raises(self) -> None:
        with pytest.raises(FindingsNormalizeError):
            normalize({"tool_input": {}}, "report_findings")

    def test_report_findings_non_array_raises(self) -> None:
        with pytest.raises(FindingsNormalizeError):
            normalize({"tool_input": {"findings": {}}}, "report_findings")

    def test_fenced_json_parses_every_fence(self) -> None:
        text = 'head\n\n```json\n[{"category": "nit", "file": "a.py", "line": 1, ' \
               '"summary": "s", "failure_scenario": "f"}]\n```\n\ntail\n'
        records = normalize(text, "fenced_json")
        assert len(records) == 1
        assert classify(records[0]) == "nonblocking"

    def test_fenced_json_no_fences_is_zero_records(self) -> None:
        assert normalize("no fences here", "fenced_json") == []

    def test_fenced_json_unparseable_fence_is_one_unmappable_record(self) -> None:
        records = normalize("```json\nnot json\n```", "fenced_json")
        assert len(records) == 1
        assert records[0].unmappable is True
        assert classify(records[0]) == "blocking"

    def test_fenced_json_non_array_fence_is_one_unmappable_record(self) -> None:
        records = normalize('```json\n{"verdict": "clean"}\n```', "fenced_json")
        assert len(records) == 1
        assert records[0].unmappable is True

    def test_fenced_json_non_text_payload_raises(self) -> None:
        with pytest.raises(FindingsNormalizeError):
            normalize({}, "fenced_json")

    def test_unknown_source_raises(self) -> None:
        with pytest.raises(FindingsNormalizeError):
            normalize({}, "vibes")


class TestResolveNonblockingCategories:
    def test_none_resolves_to_default(self) -> None:
        resolved = resolve_nonblocking_categories(None)
        assert "typo" in resolved
        assert "correctness" not in resolved

    def test_configured_list_extends_default(self) -> None:
        resolved = resolve_nonblocking_categories(["vibes"])
        assert "vibes" in resolved
        assert "typo" in resolved

    def test_explicit_empty_list_still_carries_the_default(self) -> None:
        assert "typo" in resolve_nonblocking_categories([])

    def test_malformed_value_degrades_to_default(self) -> None:
        resolved = resolve_nonblocking_categories("style")
        assert "typo" in resolved
        assert "vibes" not in resolved

    def test_entries_are_normalized(self) -> None:
        resolved = resolve_nonblocking_categories(["  VIBES  "])
        assert "vibes" in resolved


class TestSummarize:
    def test_every_record_yields_exactly_one_primitive(self) -> None:
        records = [
            _full(category="typo"),
            _full(category="correctness"),
            FindingRecord(unmappable=True),
        ]
        summary = summarize(records)
        assert len(summary.findings) == 3
        assert summary.blocking_count == 2
        assert summary.nonblocking_count == 1

    def test_primitives_carry_re_derivation_inputs(self) -> None:
        summary = summarize([_full(category="style", verdict="CONFIRMED")])
        primitive = summary.findings[0]
        assert primitive.as_dict() == {
            "category": "style",
            "verdict": "CONFIRMED",
            "blocking": True,
            "has_required_fields": True,
            "finding_key": "a.py:1:style",
        }

    def test_as_dict_shape(self) -> None:
        summary = summarize([_full(category="typo")])
        payload = summary.as_dict()
        assert payload["findings_blocking"] == 0
        assert payload["findings_nonblocking"] == 1
        assert payload["findings"][0]["finding_key"] == "a.py:1:typo"


class TestFindingKey:
    def test_file_line_category(self) -> None:
        assert finding_key(_full(category="Style")) == "a.py:1:style"

    def test_missing_parts_render_empty(self) -> None:
        assert finding_key(FindingRecord()) == "::"

    def test_category_casing_folds_for_stable_identity(self) -> None:
        assert finding_key(_full(category="NIT")) == finding_key(_full(category="nit"))
