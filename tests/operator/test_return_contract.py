"""Structured, fail-closed subagent return contract (ab-1394e797).

The old `parse_task_result` failed OPEN: every line with a colon became a field,
and `status` accepted any value (defaulting to "UNKNOWN"). A model that appended
a sentence or invented a status produced a bogus-but-accepted TaskResult.

The contract is now schema-first and fail-closed:
  - a structured ```json / <result> block (the claude path) is preferred and
    validated; a present-but-malformed block is rejected, not scraped;
  - the RESULT: text grammar (codex/gemini fallback) reads ONLY known contract
    keys, first-occurrence-wins, and the status must be EXACTLY one of the enum.
"""
from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent.parent
sys.path.insert(0, str(REPO_ROOT / "skills" / "do"))

from orchestrator import (  # noqa: E402
    VALID_STATUSES,
    parse_structured_result,
    parse_task_result,
)


# ── text grammar (codex/gemini fallback) ──────────────────────────────


def test_text_grammar_happy_path():
    r = parse_task_result("RESULT: SUCCESS\nTASK: 2.1\nCOMMIT: abc123")
    assert r is not None
    assert r.status == "SUCCESS"
    assert r.task_id == "2.1"
    assert r.commit == "abc123"
    assert r.structured is False


def test_text_grammar_done_with_concerns_status():
    r = parse_task_result(
        "RESULT: DONE_WITH_CONCERNS\nTASK: 3.2\nCONCERNS: flaky test left in"
    )
    assert r is not None
    assert r.status == "DONE_WITH_CONCERNS"
    assert r.concerns == "flaky test left in"


def test_text_grammar_rejects_appended_prose_status():
    """The fail-open case the node cites: a status with trailing words must NOT
    be coerced into SUCCESS."""
    assert parse_task_result("RESULT: SUCCESS but it actually failed\nTASK: 1.1") is None


def test_text_grammar_accepts_trailing_punctuation():
    """A bare trailing period/quote is stripped; the enum still matches."""
    assert parse_task_result("RESULT: SUCCESS.\nTASK: 1.1").status == "SUCCESS"
    assert parse_task_result('RESULT: "BLOCKED"\nTASK: 1.1').status == "BLOCKED"


def test_text_grammar_invalid_status_is_rejected_not_unknown():
    """An unrecognized status returns None (no 'UNKNOWN' false-result)."""
    assert parse_task_result("RESULT: MOSTLY_DONE\nTASK: 1.1") is None


def test_text_grammar_ignores_appended_prose_lines():
    """A 'Note: ...' / sentence line with a colon must not become a field, and a
    later stray RESULT: in prose must not override the real one."""
    out = (
        "RESULT: FAILED\n"
        "TASK: 4.4\n"
        "ERROR: build broke\n"
        "Note: I will RESULT: SUCCESS once the dep lands\n"
        "Also: here is some narration with a colon"
    )
    r = parse_task_result(out)
    assert r is not None
    assert r.status == "FAILED"
    assert r.error == "build broke"


def test_text_grammar_missing_task_is_rejected():
    assert parse_task_result("RESULT: SUCCESS") is None


def test_empty_output_is_none():
    assert parse_task_result("") is None
    assert parse_task_result("   \n  ") is None


# ── structured block (the claude path) ────────────────────────────────


def test_structured_fenced_json_preferred():
    out = '```json\n{"result": "SUCCESS", "task": "2.1", "commit": "deadbee"}\n```'
    r = parse_task_result(out)
    assert r is not None
    assert r.structured is True
    assert r.status == "SUCCESS"
    assert r.task_id == "2.1"
    assert r.commit == "deadbee"


def test_structured_result_tag_lowercase_keys():
    out = '<result>{"result": "blocked", "task": "9.9", "reason": "needs creds"}</result>'
    r = parse_structured_result(out)
    assert r is not None
    assert r.status == "BLOCKED"
    assert r.reason == "needs creds"
    assert r.structured is True


def test_structured_present_but_invalid_status_fails_closed():
    """A structured block with a bad status is rejected - and the parser does NOT
    fall back to scraping a stray prose RESULT: line."""
    out = (
        '```json\n{"result": "PROBABLY", "task": "1.1"}\n```\n'
        "RESULT: SUCCESS\nTASK: 1.1"
    )
    assert parse_task_result(out) is None


def test_structured_malformed_json_fails_closed():
    out = "```json\n{not valid json}\n```"
    assert parse_task_result(out) is None
    assert parse_structured_result(out) is None


def test_structured_absent_returns_none_so_caller_falls_back():
    """parse_structured_result alone returns None when there is no block, letting
    parse_task_result use the text grammar."""
    assert parse_structured_result("RESULT: SUCCESS\nTASK: 1.1") is None
    assert parse_task_result("RESULT: SUCCESS\nTASK: 1.1") is not None


def test_valid_statuses_enum_is_the_contract():
    assert VALID_STATUSES == ("SUCCESS", "DONE_WITH_CONCERNS", "FAILED", "BLOCKED")
