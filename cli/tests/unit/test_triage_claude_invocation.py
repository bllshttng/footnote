"""Unit tests for triage's ``claude -p`` invocation.

Two bugs in the inbox-drain triage path (found 2026-06-09):
  1. ``--bare`` was hardcoded, but it reads only ANTHROPIC_API_KEY / apiKeyHelper,
     never subscription-OAuth - so the drain stranded with "Not logged in".
  2. ``claude`` exits 0 with an ``is_error`` envelope on auth failure; ``check=True``
     missed it, swallowing the real reason as a generic schema violation.
"""
from __future__ import annotations

import json

import pytest

from fno.inbox.triage import (
    _raise_on_claude_error,
)

# --- Bug 2: fail loud on the is_error envelope -----------------------------

def test_raise_on_is_error_not_logged_in():
    envelope = json.dumps({"is_error": True, "result": "Not logged in · Please run /login"})
    with pytest.raises(ValueError) as exc:
        _raise_on_claude_error(envelope)
    msg = str(exc.value)
    assert "Not logged in" in msg
    assert "ANTHROPIC_API_KEY" in msg  # actionable auth hint for the login case


def test_raise_on_generic_error_without_login_hint():
    envelope = json.dumps({"is_error": True, "result": "rate limited"})
    with pytest.raises(ValueError) as exc:
        _raise_on_claude_error(envelope)
    assert "rate limited" in str(exc.value)
    assert "ANTHROPIC_API_KEY" not in str(exc.value)  # hint only for login errors


def test_no_raise_on_success_schema_object():
    # A successful schema response is the object itself, no is_error key.
    _raise_on_claude_error(json.dumps({"action": "ignore", "body": "nothing to do"}))


def test_no_raise_on_is_error_false_envelope():
    _raise_on_claude_error(json.dumps({"is_error": False, "result": "{}"}))


def test_no_raise_on_non_json_stub_output():
    _raise_on_claude_error("plain text, not json")
