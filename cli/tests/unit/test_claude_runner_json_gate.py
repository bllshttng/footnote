"""Tests for the gated JSON-findings path in claude_runner (T3.3, ab-6c8f4c61).

The cross-model OFF path (json_findings=False) must be byte-for-byte the legacy
``::finding::`` behavior with no provider attribution; the ON path parses the
strict JSON contract and attributes ``provider="claude"``.
"""
from __future__ import annotations

from pathlib import Path

from fno.review.findings_parser import PARSE_FAILURE_PREFIX
from fno.review.runners.claude_runner import run_via_claude_code


class _FakeAdapter:
    """Returns a canned one-shot dispatch reply."""

    def __init__(self, stdout: str) -> None:
        self._stdout = stdout

    def __call__(self, **_kwargs: object) -> object:
        return type("Reply", (), {"reply": self._stdout})()


def test_off_path_uses_legacy_finding_parser() -> None:
    adapter = _FakeAdapter("::finding high src/foo.py 42 missing null check::")
    outcome = run_via_claude_code(
        "code_reviewer", "p", "d", dispatch=adapter, json_findings=False
    )
    assert outcome.ok is True
    assert outcome.provider is None  # OFF path: no attribution (unchanged)
    assert len(outcome.findings) == 1
    assert outcome.findings[0].severity == "high"
    assert outcome.findings[0].line == 42


def test_off_path_unstructured_is_single_info_finding() -> None:
    """Legacy no-silent-zero rule preserved on the OFF path."""
    adapter = _FakeAdapter("the code looks fine, no issues")
    outcome = run_via_claude_code(
        "code_reviewer", "p", "d", dispatch=adapter, json_findings=False
    )
    assert outcome.ok is True
    assert outcome.provider is None
    assert len(outcome.findings) == 1
    assert outcome.findings[0].severity == "info"


def test_on_path_parses_json_and_attributes_claude() -> None:
    adapter = _FakeAdapter('[{"severity": "medium", "message": "x", "line": 7}]')
    outcome = run_via_claude_code(
        "code_reviewer", "p", "d", dispatch=adapter, json_findings=True
    )
    assert outcome.ok is True
    assert outcome.provider == "claude"
    assert outcome.findings[0].severity == "medium"
    assert outcome.findings[0].line == 7


def test_on_path_prose_is_terminal_soft_fail() -> None:
    adapter = _FakeAdapter("here is my review, all good")
    outcome = run_via_claude_code(
        "code_reviewer", "p", "d", dispatch=adapter, json_findings=True
    )
    assert outcome.ok is False
    assert outcome.error.startswith(PARSE_FAILURE_PREFIX)
    assert outcome.provider == "claude"


def test_on_path_empty_array_is_clean() -> None:
    adapter = _FakeAdapter("[]")
    outcome = run_via_claude_code(
        "code_reviewer", "p", "d", dispatch=adapter, json_findings=True
    )
    assert outcome.ok is True
    assert outcome.findings == []
    assert outcome.provider == "claude"
