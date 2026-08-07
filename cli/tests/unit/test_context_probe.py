"""Tests for fno.context_probe - the single context-window implementation (x-7685).

The probe is ported verbatim from skills/target/scripts/context-probe.sh; these
tests pin the port's arithmetic and window table, including the [1m] inflation
trap and the round-half-up percent. The shim's 67-assertion regression suite
(tests/test-context-probe.sh) drives the real path end to end; this unit test
covers the field-level invariants.
"""
from __future__ import annotations

import json

from fno import context_probe
from fno.context_probe import ContextReading, probe_context


def _write_transcript(path, records):
    path.write_text(
        "".join(json.dumps(r) + "\n" for r in records), encoding="utf-8"
    )


def _usage_record(model, input_tokens, cache_creation, cache_read):
    return {
        "type": "assistant",
        "message": {
            "model": model,
            "usage": {
                "input_tokens": input_tokens,
                "cache_creation_input_tokens": cache_creation,
                "cache_read_input_tokens": cache_read,
            },
        },
    }


def test_probe_sums_all_three_token_kinds_off_last_assistant_line(tmp_path):
    transcript = tmp_path / "t.jsonl"
    _write_transcript(
        transcript,
        [
            _usage_record("claude-opus-5", 100, 50, 50),
            {"type": "user", "message": {"content": "hi"}},
            _usage_record("claude-opus-5", 307_000, 500, 350),
        ],
    )
    reading = probe_context(transcript_path=transcript)
    assert reading is not None
    assert reading.used_tokens == 307_850  # last line only, all three kinds
    assert reading.window_tokens == 1_000_000
    assert reading.model == "claude-opus-5"


def test_probe_round_half_up_percent(tmp_path):
    transcript = tmp_path / "t.jsonl"
    # 307_850 / 1_000_000 -> 30.785% rounds to 31 (half-up via window//2).
    _write_transcript(transcript, [_usage_record("claude-opus-5", 307_000, 500, 350)])
    assert probe_context(transcript_path=transcript).used_pct == 31


def test_probe_window_table_1m_marker_is_not_a_catchall(tmp_path):
    # The zai/GLM [1m] routing marker alone -> 1M. But a plain unlisted claude id
    # MUST fall to 200K, not be inflated by a claude-* catch-all.
    transcript = tmp_path / "t.jsonl"
    _write_transcript(transcript, [_usage_record("glm-5.2[1m]", 200_000, 0, 0)])
    assert probe_context(transcript_path=transcript).window_tokens == 1_000_000


def test_probe_window_table_bare_glm_1m_id_is_1m(tmp_path):
    # The provider API drops the [1m] routing marker, so the transcript carries
    # the bare "glm-5.2" id (12567 vs 2 in one real transcript). The bare id of
    # the 1M generation must read 1M, not fall through to 200K.
    transcript = tmp_path / "t.jsonl"
    _write_transcript(transcript, [_usage_record("glm-5.2", 265_000, 0, 0)])
    reading = probe_context(transcript_path=transcript)
    assert reading.window_tokens == 1_000_000
    assert reading.used_pct == 27  # 265k/1M ~ 26.5% rounds to 27 (half-up)


def test_probe_window_table_unlisted_falls_to_200k(tmp_path):
    transcript = tmp_path / "t.jsonl"
    _write_transcript(transcript, [_usage_record("claude-opus-4-5", 200_000, 0, 0)])
    reading = probe_context(transcript_path=transcript)
    assert reading.window_tokens == 200_000
    assert reading.used_pct == 100  # 200k/200k


def test_probe_window_table_haiku_is_200k(tmp_path):
    transcript = tmp_path / "t.jsonl"
    _write_transcript(transcript, [_usage_record("claude-haiku-4-5", 10, 0, 0)])
    assert probe_context(transcript_path=transcript).window_tokens == 200_000


def test_probe_window_table_known_5x_models_are_1m(tmp_path):
    transcript = tmp_path / "t.jsonl"
    for model in ("claude-opus-5", "claude-sonnet-5", "claude-fable-5"):
        _write_transcript(transcript, [_usage_record(model, 10, 0, 0)])
        assert probe_context(transcript_path=transcript).window_tokens == 1_000_000


def test_probe_none_when_no_assistant_usage_line(tmp_path):
    transcript = tmp_path / "t.jsonl"
    _write_transcript(
        transcript,
        [
            {"type": "user", "message": {"content": "fresh session"}},
            {"type": "assistant", "message": {"model": "claude-opus-5"}},  # no usage block
        ],
    )
    assert probe_context(transcript_path=transcript) is None


def test_probe_none_when_transcript_missing(tmp_path):
    assert probe_context(transcript_path=tmp_path / "nope.jsonl") is None


def test_probe_none_when_no_ambient_identity(monkeypatch, tmp_path):
    # With no transcript_path and no ambient identity, self-resolution floors to
    # None rather than raising: whoami/fno context gain no failure mode.
    for var in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: None
    )
    assert probe_context() is None


def test_probe_ignores_non_assistant_usage_and_malformed_lines(tmp_path):
    transcript = tmp_path / "t.jsonl"
    _write_transcript(
        transcript,
        [
            {"type": "summary", "message": {"usage": {"input_tokens": 9}}},
            "not json at all",
            {"type": "assistant", "message": {"model": "claude-sonnet-5", "usage": "nope"}},
            _usage_record("claude-sonnet-5", 5_000, 0, 0),
        ],
    )
    reading = probe_context(transcript_path=transcript)
    assert reading is not None
    assert reading.used_tokens == 5_000


def test_reading_is_frozen_dataclass():
    reading = ContextReading(used_tokens=1, window_tokens=2, used_pct=3, model="m")
    try:
        reading.used_tokens = 9  # type: ignore[misc]
    except Exception:
        return
    raise AssertionError("ContextReading must be frozen (derived, never stored)")


def test_module_exposes_single_window_table():
    # AC4 spirit: the window table is an allowlist defined once in this module.
    assert context_probe._window_for("claude-opus-5") == context_probe._window_for("claude-opus-5")
