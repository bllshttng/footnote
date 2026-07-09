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
    RETURN_CONTRACT_INSTRUCTION,
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


# ── AC7-HP: parser hardening property tests (increase-consistency.md) ──


def test_fenced_block_with_prose_before_and_after_parses():
    """A well-formed block still parses when narration surrounds it."""
    out = (
        "I finished the task and ran the tests.\n"
        '```json\n{"result": "SUCCESS", "task": "4.2", "commit": "beefcafe"}\n```\n'
        "Let me know if anything else is needed."
    )
    r = parse_task_result(out)
    assert r is not None and r.status == "SUCCESS" and r.task_id == "4.2"
    assert r.structured is True


def test_stray_result_line_does_not_hijack_structured_block():
    """A valid JSON block wins even when prose also contains a RESULT: line."""
    out = (
        "Earlier I thought this would be RESULT: FAILED but then I fixed it.\n"
        '```json\n{"result": "SUCCESS", "task": "1.1"}\n```'
    )
    r = parse_task_result(out)
    assert r is not None and r.status == "SUCCESS"


def test_text_grammar_first_result_occurrence_wins():
    """Two RESULT: lines -> the first is authoritative; a later one cannot flip it."""
    out = "RESULT: BLOCKED\nTASK: 2.2\nREASON: waiting\nRESULT: SUCCESS"
    r = parse_task_result(out)
    assert r is not None and r.status == "BLOCKED" and r.task_id == "2.2"


def test_out_of_enum_in_prose_wrapped_block_rejected():
    out = 'prose\n```json\n{"result": "DONELIKE", "task": "1.1"}\n```\nmore prose'
    assert parse_task_result(out) is None


# ── the instruction we ship must agree with the parser ──


def test_instruction_enumerates_every_valid_status():
    for status in VALID_STATUSES:
        assert status in RETURN_CONTRACT_INSTRUCTION


def test_instruction_states_block_last_rule():
    assert "LAST" in RETURN_CONTRACT_INSTRUCTION


def test_instruction_example_round_trips_through_parser():
    """The exact JSON example we tell workers to emit must parse to SUCCESS.

    This is the anti-drift guarantee: the instruction and the parser can never
    disagree about what a well-formed block looks like."""
    import re

    block = re.search(r"```json\s*(\{.*?\})\s*```", RETURN_CONTRACT_INSTRUCTION, re.DOTALL)
    assert block is not None
    r = parse_task_result(f"```json\n{block.group(1)}\n```")
    assert r is not None and r.status == "SUCCESS" and r.task_id and r.structured is True
