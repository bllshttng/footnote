"""Unit tests for ``fno.mcp.channel`` (envelope build / validate).

Wave 1.1 contract tests. The Wave 1.0 smoke script
(``cli/scripts/smoke/validate-mcp-channel.sh``) covers the structural
pin against the fixture; this file covers the round-trip and
error-path behavior the dispatcher relies on.
"""
from __future__ import annotations

import pytest

from fno.mcp.channel import (
    ENVELOPE_VERSION,
    MCP_CHANNEL_METHOD,
    MCPChannelEnvelopeError,
    build_channel_notification,
    envelope_drift_diff,
    validate_envelope,
)


class TestBuildEnvelope:
    def test_minimal_envelope_has_pinned_shape(self) -> None:
        env = build_channel_notification(content="hello")
        assert env["jsonrpc"] == "2.0"
        assert env["method"] == MCP_CHANNEL_METHOD
        assert env["params"] == {"content": "hello", "meta": {}}

    def test_meta_round_trips(self) -> None:
        env = build_channel_notification(
            content="x", meta={"from_name": "alice", "chat_id": "abc"}
        )
        assert env["params"]["meta"] == {"from_name": "alice", "chat_id": "abc"}

    def test_envelope_version_is_pinned_to_1(self) -> None:
        # Bumping ENVELOPE_VERSION requires updating the fixture +
        # smoke script in the same commit; this test ensures the bump
        # is deliberate.
        assert ENVELOPE_VERSION == "1"

    @pytest.mark.parametrize("bad_key", ["chat-id", "from name", "x.y", ""])
    def test_hyphen_or_non_identifier_meta_keys_are_rejected(self, bad_key: str) -> None:
        with pytest.raises(MCPChannelEnvelopeError) as exc:
            build_channel_notification(content="x", meta={bad_key: "v"})
        assert "meta keys must match" in str(exc.value)
        assert bad_key in str(exc.value)

    def test_non_string_meta_value_rejected(self) -> None:
        with pytest.raises(MCPChannelEnvelopeError):
            build_channel_notification(content="x", meta={"k": 123})  # type: ignore[arg-type]

    def test_non_string_content_rejected(self) -> None:
        with pytest.raises(MCPChannelEnvelopeError):
            build_channel_notification(content=42)  # type: ignore[arg-type]

    def test_none_meta_becomes_empty_dict(self) -> None:
        env = build_channel_notification(content="x", meta=None)
        assert env["params"]["meta"] == {}


class TestValidateEnvelope:
    def test_accepts_minimal_valid_envelope(self) -> None:
        env = build_channel_notification(content="hi")
        ok, reason = validate_envelope(env)
        assert ok is True
        assert reason is None

    def test_rejects_non_dict(self) -> None:
        ok, reason = validate_envelope("not a dict")
        assert ok is False
        assert reason is not None and reason.startswith("envelope_not_dict")

    def test_rejects_wrong_jsonrpc_version(self) -> None:
        env = build_channel_notification(content="hi")
        env["jsonrpc"] = "1.0"
        ok, reason = validate_envelope(env)
        assert ok is False
        assert reason == "jsonrpc_version_missing_or_not_2.0"

    def test_rejects_method_drift(self) -> None:
        env = build_channel_notification(content="hi")
        env["method"] = "notifications/some_other/channel"
        ok, reason = validate_envelope(env)
        assert ok is False
        assert reason is not None and reason.startswith("method_mismatch")

    def test_rejects_missing_content(self) -> None:
        ok, reason = validate_envelope(
            {"jsonrpc": "2.0", "method": MCP_CHANNEL_METHOD, "params": {"meta": {}}}
        )
        assert ok is False
        assert reason == "content_missing"

    def test_rejects_non_string_content(self) -> None:
        ok, reason = validate_envelope(
            {
                "jsonrpc": "2.0",
                "method": MCP_CHANNEL_METHOD,
                "params": {"content": 42, "meta": {}},
            }
        )
        assert ok is False
        assert reason == "content_not_string"

    def test_rejects_non_identifier_meta_key(self) -> None:
        ok, reason = validate_envelope(
            {
                "jsonrpc": "2.0",
                "method": MCP_CHANNEL_METHOD,
                "params": {"content": "x", "meta": {"chat-id": "abc"}},
            }
        )
        assert ok is False
        assert reason is not None and reason.startswith("meta_key_invalid")


class TestDriftDiff:
    def test_no_drift_returns_empty(self) -> None:
        env = build_channel_notification(content="x")
        assert envelope_drift_diff(env, env) == {}

    def test_method_rename_surfaces_in_diff(self) -> None:
        a = build_channel_notification(content="x")
        b = dict(a)
        b["method"] = "notifications/new/channel"
        diff = envelope_drift_diff(a, b)
        assert diff["method_expected"] == MCP_CHANNEL_METHOD
        assert diff["method_received"] == "notifications/new/channel"

    def test_params_renamed_key_surfaces_in_diff(self) -> None:
        a = build_channel_notification(content="x")
        b = {"jsonrpc": "2.0", "method": MCP_CHANNEL_METHOD, "params": {"body": "x"}}
        diff = envelope_drift_diff(a, b)
        assert "content" in diff["params_missing"]
        assert "body" in diff["params_extra"]
