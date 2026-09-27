"""Tests for the Python bridge to the Rust-owned context probe."""
from __future__ import annotations

import json

import pytest

from fno.context_probe import ContextReading, probe_context


def _usage_record(model: str, input_tokens: int) -> dict:
    return {
        "type": "assistant",
        "message": {"model": model, "usage": {"input_tokens": input_tokens}},
    }


def test_probe_returns_the_native_context_receipt(monkeypatch, tmp_path):
    transcript = tmp_path / "t.jsonl"
    calls = []

    def native(verb, args, *, timeout):
        calls.append((verb, args, timeout))
        return None, {
            "used_tokens": 23,
            "window_tokens": 100,
            "used_pct": 23,
            "model": "codex-test-model",
        }

    monkeypatch.setattr("fno.rust_binary.call_binary_json", native)

    assert probe_context(transcript_path=transcript) == ContextReading(
        23, 100, 23, "codex-test-model"
    )
    assert calls == [
        ("context-run", ["--probe", "--transcript", str(transcript), "--json"], 5)
    ]


def test_probe_does_not_fall_back_to_a_python_context_reader(monkeypatch, tmp_path):
    transcript = tmp_path / "t.jsonl"
    transcript.write_text(json.dumps(_usage_record("claude-opus-5", 307_000)) + "\n")
    monkeypatch.setattr(
        "fno.rust_binary.call_binary_json", lambda *a, **k: ("native unavailable", None)
    )

    assert probe_context(transcript_path=transcript) is None


def test_probe_rejects_malformed_native_receipts(monkeypatch, tmp_path):
    monkeypatch.setattr(
        "fno.rust_binary.call_binary_json",
        lambda *a, **k: (None, {"used_tokens": 23}),
    )

    assert probe_context(transcript_path=tmp_path / "t.jsonl") is None


def test_probe_none_when_no_ambient_identity(monkeypatch):
    for var in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: None
    )

    assert probe_context() is None


def test_reading_is_frozen_dataclass():
    reading = ContextReading(used_tokens=1, window_tokens=2, used_pct=3, model="m")
    with pytest.raises((AttributeError, TypeError)):
        reading.used_tokens = 9  # type: ignore[misc]
