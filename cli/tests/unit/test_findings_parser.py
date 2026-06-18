"""Tests for review.findings_parser - the shared strict-JSON parser."""
from __future__ import annotations

import json

import pytest

from fno.review.findings_parser import (
    FindingsParseError,
    parse_findings_json,
)


def test_parses_valid_array() -> None:
    reply = json.dumps(
        [
            {"severity": "high", "message": "missing null check",
             "file": "src/foo.py", "line": 42},
            {"severity": "info", "message": "consider a docstring"},
        ]
    )
    findings = parse_findings_json("code_reviewer", reply)
    assert len(findings) == 2
    assert findings[0].agent == "code_reviewer"
    assert findings[0].severity == "high"
    assert findings[0].file == "src/foo.py"
    assert findings[0].line == 42
    assert findings[1].file is None
    assert findings[1].line is None


def test_empty_array_is_clean_review() -> None:
    assert parse_findings_json("code_reviewer", "[]") == []


def test_strips_json_code_fence() -> None:
    reply = '```json\n[{"severity": "low", "message": "x"}]\n```'
    findings = parse_findings_json("code_reviewer", reply)
    assert len(findings) == 1
    assert findings[0].severity == "low"


def test_strips_bare_code_fence() -> None:
    reply = '```\n[{"severity": "low", "message": "x"}]\n```'
    findings = parse_findings_json("code_reviewer", reply)
    assert len(findings) == 1


def test_prose_raises_parse_error() -> None:
    with pytest.raises(FindingsParseError) as ei:
        parse_findings_json("ux_flow_tester", "Here are my findings: looks good!")
    assert ei.value.agent == "ux_flow_tester"
    assert ei.value.raw_head


def test_json_object_root_raises() -> None:
    with pytest.raises(FindingsParseError):
        parse_findings_json("code_reviewer", '{"severity": "high", "message": "x"}')


def test_item_missing_severity_raises() -> None:
    with pytest.raises(FindingsParseError):
        parse_findings_json("code_reviewer", '[{"message": "x"}]')


def test_item_missing_message_raises() -> None:
    with pytest.raises(FindingsParseError):
        parse_findings_json("code_reviewer", '[{"severity": "high"}]')


def test_non_dict_item_raises() -> None:
    with pytest.raises(FindingsParseError):
        parse_findings_json("code_reviewer", '["just a string"]')


def test_empty_reply_raises() -> None:
    with pytest.raises(FindingsParseError):
        parse_findings_json("code_reviewer", "")


def test_line_string_coerced_to_int() -> None:
    findings = parse_findings_json(
        "code_reviewer", '[{"severity": "low", "message": "x", "line": "17"}]'
    )
    assert findings[0].line == 17


def test_line_garbage_is_none() -> None:
    findings = parse_findings_json(
        "code_reviewer", '[{"severity": "low", "message": "x", "line": "n/a"}]'
    )
    assert findings[0].line is None


def test_severity_lowercased() -> None:
    findings = parse_findings_json(
        "code_reviewer", '[{"severity": "HIGH", "message": "x"}]'
    )
    assert findings[0].severity == "high"


def test_json_findings_prompt_idempotent() -> None:
    """A double call must not append the contract twice (gemini review)."""
    from fno.review.findings_parser import json_findings_prompt

    once = json_findings_prompt("base prompt")
    twice = json_findings_prompt(once)
    assert once == twice


def test_raw_preserves_item_json() -> None:
    findings = parse_findings_json(
        "code_reviewer", '[{"severity": "high", "message": "boom"}]'
    )
    assert "boom" in findings[0].raw
