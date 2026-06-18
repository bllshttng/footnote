"""Tests for the MCP request_id wire-through.

Tasks 3.1 + 3.2 from 2026-05-22-fno-agents-observability.md.

3.1: build_channel_notification accepts request_id kwarg and stamps it
     onto meta["request_id"].
3.2: channel_server forwards envelopes verbatim, so meta["request_id"]
     reaches the recipient as a <channel request_id="..."> tag attr
     (CC renders meta keys as tag attributes — no extra server logic).

Locks in:
- AC4-HP: producer-side request_id ends up byte-identical in the
  envelope meta that the recipient sees (round-trip preservation).
- AC4-ERR: empty / non-string request_id raises MCPChannelEnvelopeError
  at envelope construction (fail-loud at the producer).
- AC4-INVARIANT: the validator accepts request_id (lowercase hex matches
  META_KEY_RE on the key; value is an arbitrary string per the wire
  contract).
"""
from __future__ import annotations

import pytest

from fno.mcp.channel import (
    MCPChannelEnvelopeError,
    build_channel_notification,
    validate_envelope,
)


REQUEST_ID = "a1b2c3d4e5f6789012345678901234ab"  # 32 lowercase hex chars


# ---------------------------------------------------------------------------
# 3.1 — build_channel_notification accepts + propagates request_id
# ---------------------------------------------------------------------------


def test_build_channel_includes_request_id_in_meta() -> None:
    """request_id kwarg lands in params.meta as the 'request_id' key."""
    env = build_channel_notification(content="hi", request_id=REQUEST_ID)
    meta = env["params"]["meta"]
    assert meta["request_id"] == REQUEST_ID


def test_build_channel_no_request_id_means_no_meta_key() -> None:
    """When request_id is not passed, meta has no 'request_id' key."""
    env = build_channel_notification(content="hi")
    meta = env["params"]["meta"]
    assert "request_id" not in meta


def test_build_channel_request_id_merges_with_existing_meta() -> None:
    """Passing both meta and request_id merges; request_id wins on collision."""
    env = build_channel_notification(
        content="hi",
        meta={"from_name": "alpha", "request_id": "old-shouldnt-survive"},
        request_id=REQUEST_ID,
    )
    meta = env["params"]["meta"]
    assert meta["from_name"] == "alpha"
    assert meta["request_id"] == REQUEST_ID


def test_build_channel_request_id_does_not_mutate_caller_meta() -> None:
    """build_channel_notification must not mutate the caller's meta dict."""
    original = {"from_name": "alpha"}
    build_channel_notification(content="hi", meta=original, request_id=REQUEST_ID)
    assert original == {"from_name": "alpha"}, "caller meta was mutated"


def test_build_channel_rejects_empty_request_id() -> None:
    """AC4-ERR: empty string request_id is fail-loud at the producer."""
    with pytest.raises(MCPChannelEnvelopeError) as excinfo:
        build_channel_notification(content="hi", request_id="")
    assert "request_id" in str(excinfo.value)


def test_build_channel_rejects_non_string_request_id() -> None:
    """AC4-ERR: non-string request_id fails fast at envelope build time."""
    with pytest.raises(MCPChannelEnvelopeError):
        build_channel_notification(content="hi", request_id=12345)  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# Validator accepts request_id (no AC4-INVARIANT format enforcement at wire)
# ---------------------------------------------------------------------------


def test_validator_accepts_envelope_with_request_id() -> None:
    """validate_envelope does not reject envelopes carrying meta.request_id."""
    env = build_channel_notification(content="hi", request_id=REQUEST_ID)
    ok, reason = validate_envelope(env)
    assert ok, f"validator rejected envelope with request_id: {reason}"


def test_validator_accepts_envelope_without_request_id() -> None:
    """Locked Decision #12: request_id is OPTIONAL at receive time."""
    env = build_channel_notification(content="hi")
    ok, reason = validate_envelope(env)
    assert ok, f"validator rejected request_id-less envelope: {reason}"


# ---------------------------------------------------------------------------
# 3.2 — Round-trip: producer request_id preserved through forward pipeline
# ---------------------------------------------------------------------------


def test_roundtrip_request_id_byte_identical() -> None:
    """The recipient-visible meta.request_id equals the producer's exactly.

    channel_server forwards envelopes verbatim via _write_message
    (see channel_server._sidecar_forward_loop). The producer's request_id
    must therefore survive byte-for-byte. This test exercises the
    contract at the envelope layer (the same envelope channel_server
    forwards).
    """
    producer_env = build_channel_notification(content="hello", request_id=REQUEST_ID)

    # Simulate the validator gate that channel_server runs before
    # forwarding: a malformed envelope would be dropped (so the round-
    # trip test must also pass validation).
    ok, _reason = validate_envelope(producer_env)
    assert ok

    # The forwarded envelope is the same object (verbatim per
    # channel_server._sidecar_forward_loop, line ~287 _write_message).
    recipient_visible_meta = producer_env["params"]["meta"]
    assert recipient_visible_meta["request_id"] == REQUEST_ID


def test_roundtrip_with_extra_meta_keys() -> None:
    """A real-world envelope has multiple meta keys; request_id is one of them."""
    env = build_channel_notification(
        content="hello",
        meta={
            "from_name": "sender-agent",
            "from_session_id": "session-abc",
        },
        request_id=REQUEST_ID,
    )
    meta = env["params"]["meta"]
    assert meta["from_name"] == "sender-agent"
    assert meta["from_session_id"] == "session-abc"
    assert meta["request_id"] == REQUEST_ID
    ok, _ = validate_envelope(env)
    assert ok
