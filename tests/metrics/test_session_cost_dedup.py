#!/usr/bin/env python3
"""Unit tests for transcript dedup in fno.cost._session_cost (the former
scripts/metrics/session-cost.py).

Claude Code writes one transcript JSONL line per content block; every line
of the same API message repeats identical `message.usage`. Without dedup by
`(message.id, requestId)` the parser overstates tokens ~2.5-2.8x (verified
on a live transcript: 502 assistant lines -> 185 unique pairs).

Covers AC1-HP / AC1-ERR / AC1-UI / AC1-EDGE / AC1-FR plus the Boundaries
and Invariants failure modes from the cost-accuracy plan.

Run: python3 tests/metrics/test_session_cost_dedup.py
 OR: cd cli && uv run pytest ../tests/metrics/test_session_cost_dedup.py -q
"""

import contextlib
import io
import json
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

# session-cost.py moved into the fno package as fno.cost._session_cost.
sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
from fno.cost import _session_cost as session_cost  # noqa: E402

USAGE = {
    "input_tokens": 10,
    "output_tokens": 20,
    "cache_read_input_tokens": 1000,
    "cache_creation_input_tokens": 50,
}


def _assistant_line(
    msg_id: str,
    request_id: str | None,
    usage: dict | None = None,
    model: str = "claude-opus-4-8",
    ts: str = "2026-06-04T20:19:34.000Z",
) -> dict:
    message: dict = {"id": msg_id, "model": model, "usage": dict(usage or USAGE)}
    if msg_id is None:
        del message["id"]
    line = {
        "type": "assistant",
        "timestamp": ts,
        "message": message,
        "gitBranch": "main",
    }
    if request_id is not None:
        line["requestId"] = request_id
    return line


def _write_transcript(path: Path, lines: list) -> None:
    with open(path, "w") as f:
        for line in lines:
            f.write((line if isinstance(line, str) else json.dumps(line)) + "\n")


def _parse(lines: list, **kwargs):
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "t.jsonl"
        _write_transcript(p, lines)
        return session_cost.parse_transcript(str(p), "test-session", **kwargs)


# --- AC1-HP: deduped, correctly priced cost ----------------------------------


def test_duplicate_lines_count_once():
    # Each API message spans 3 JSONL lines with byte-identical usage.
    lines = []
    for i in range(2):
        for _ in range(3):
            lines.append(_assistant_line(f"msg_{i}", f"req_{i}"))
    metrics = _parse(lines)
    assert metrics.input_tokens == 2 * USAGE["input_tokens"], metrics.input_tokens
    assert metrics.output_tokens == 2 * USAGE["output_tokens"]
    assert metrics.cache_read_tokens == 2 * USAGE["cache_read_input_tokens"]
    assert metrics.cache_create_tokens == 2 * USAGE["cache_creation_input_tokens"]
    assert metrics.assistant_messages == 2, metrics.assistant_messages


def test_deduped_cost_uses_modern_opus_48_rates():
    lines = [_assistant_line("msg_0", "req_0") for _ in range(3)]
    metrics = _parse(lines)
    expected = (
        USAGE["input_tokens"] * 5.00
        + USAGE["output_tokens"] * 25.00
        + USAGE["cache_read_input_tokens"] * 0.50
        + USAGE["cache_creation_input_tokens"] * 6.25
    ) / 1_000_000
    assert abs(metrics.cost_usd - expected) < 1e-9, (metrics.cost_usd, expected)


# --- AC1-ERR: missing dedup keys ----------------------------------------------


def test_lines_missing_request_id_count_as_is():
    lines = [
        _assistant_line("msg_0", None),  # no requestId: counted as-is
        _assistant_line("msg_0", None),  # again: counted again (no false dedup)
        _assistant_line("msg_1", "req_1"),
        _assistant_line("msg_1", "req_1"),  # full key: deduped
    ]
    metrics = _parse(lines)
    assert metrics.assistant_messages == 3, metrics.assistant_messages
    assert metrics.input_tokens == 3 * USAGE["input_tokens"]


def test_lines_missing_message_id_count_as_is():
    lines = [
        _assistant_line(None, "req_0"),
        _assistant_line(None, "req_0"),
    ]
    metrics = _parse(lines)
    assert metrics.assistant_messages == 2


def test_non_string_dedup_keys_count_as_is():
    # A future format drift to non-string id/requestId must over-count
    # toward the old per-line behavior, never dedup on unstable keys.
    a = _assistant_line("msg_0", None)
    a["requestId"] = 12345
    b = _assistant_line("msg_0", None)
    b["requestId"] = 12345
    metrics = _parse([a, b])
    assert metrics.assistant_messages == 2


# --- AC1-UI: output shape stability -------------------------------------------


def test_json_output_keys_unchanged():
    metrics = _parse([_assistant_line("msg_0", "req_0")])
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        session_cost.print_metrics(metrics, as_json=True)
    payload = json.loads(out.getvalue())
    assert set(payload) >= {
        "session_id",
        "cost_usd",
        "tokens",
        "messages",
        "compactions",
        "duration_minutes",
        "primary_model",
        "models",
    }
    assert set(payload["tokens"]) == {"input", "output", "cache_read", "cache_create", "total"}
    assert set(payload["messages"]) == {"user", "assistant", "subagent"}


def test_json_surfaces_pricing_fallback_models():
    session_cost.FALLBACK_MODELS_SEEN.clear()
    with contextlib.redirect_stderr(io.StringIO()):
        metrics = _parse([_assistant_line("msg_0", "req_0", model="claude-opus-next")])
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        session_cost.print_metrics(metrics, as_json=True)
    payload = json.loads(out.getvalue())
    assert payload.get("pricing_fallback_models") == ["claude-opus-next"]
    session_cost.FALLBACK_MODELS_SEEN.clear()


def test_json_omits_fallback_field_when_no_fallback():
    session_cost.FALLBACK_MODELS_SEEN.clear()
    metrics = _parse([_assistant_line("msg_0", "req_0")])
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        session_cost.print_metrics(metrics, as_json=True)
    payload = json.loads(out.getvalue())
    assert "pricing_fallback_models" not in payload


# --- AC1-EDGE: multi-transcript dedup ------------------------------------------


def test_resumed_session_history_not_recounted():
    # Resumed sessions copy prior history lines (with usage) into the new
    # transcript file; a shared seen set across files must count each
    # unique message once.
    first = [_assistant_line("msg_0", "req_0"), _assistant_line("msg_1", "req_1")]
    second = [
        _assistant_line("msg_0", "req_0"),  # copied history
        _assistant_line("msg_1", "req_1"),  # copied history
        _assistant_line("msg_2", "req_2"),  # new work
    ]
    with tempfile.TemporaryDirectory() as td:
        p1, p2 = Path(td) / "s1.jsonl", Path(td) / "s2.jsonl"
        _write_transcript(p1, first)
        _write_transcript(p2, second)
        shared: set = set()
        m1 = session_cost.parse_transcript(str(p1), "s1", seen=shared)
        m2 = session_cost.parse_transcript(str(p2), "s2", seen=shared)
    combined = session_cost.merge_metrics([m1, m2])
    assert combined.input_tokens == 3 * USAGE["input_tokens"], combined.input_tokens
    assert combined.assistant_messages == 3


def test_per_file_dedup_without_shared_set():
    # Without an explicit seen set, each parse still dedups within its file.
    lines = [_assistant_line("msg_0", "req_0") for _ in range(3)]
    metrics = _parse(lines)
    assert metrics.input_tokens == USAGE["input_tokens"]


# --- AC1-FR: malformed transcript recovery -------------------------------------


def test_malformed_lines_skipped_with_warning():
    lines = [
        _assistant_line("msg_0", "req_0"),
        "{not valid json",
        _assistant_line("msg_1", "req_1"),
        "also not json}",
    ]
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr):
        metrics = _parse(lines)
    assert metrics.assistant_messages == 2
    assert "2 malformed lines" in stderr.getvalue()


# --- Boundaries -----------------------------------------------------------------


def test_zero_assistant_lines():
    metrics = _parse([{"type": "user", "timestamp": "2026-06-04T20:00:00.000Z"}])
    assert metrics.cost_usd == 0.0
    assert metrics.assistant_messages == 0
    assert metrics.total_tokens == 0


def test_null_usage_fields_coerced():
    usage = {"input_tokens": None, "output_tokens": 5}
    metrics = _parse([_assistant_line("msg_0", "req_0", usage=usage)])
    assert metrics.input_tokens == 0
    assert metrics.output_tokens == 5


# --- Invariants -------------------------------------------------------------------


def test_dedup_never_increases_token_counts():
    lines = [
        _assistant_line("msg_0", "req_0"),
        _assistant_line("msg_0", "req_0"),
        _assistant_line("msg_1", None),
        _assistant_line("msg_2", "req_2"),
    ]
    deduped = _parse(lines)
    # Per-line accounting baseline: parse with dedup keys made unique.
    unique_lines = [
        _assistant_line(f"msg_{i}", f"uniq_req_{i}") for i in range(len(lines))
    ]
    per_line = _parse(unique_lines)
    assert deduped.total_tokens <= per_line.total_tokens
    assert deduped.total_tokens == 3 * sum(USAGE.values())


def test_compaction_detection_unaffected_by_duplicates():
    big = {"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 100_000,
           "cache_creation_input_tokens": 0}
    small = {"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 10_000,
             "cache_creation_input_tokens": 0}
    lines = [
        _assistant_line("msg_0", "req_0", usage=big),
        _assistant_line("msg_0", "req_0", usage=big),  # duplicate line
        _assistant_line("msg_1", "req_1", usage=small),  # >50% context drop
        _assistant_line("msg_1", "req_1", usage=small),  # duplicate line
    ]
    metrics = _parse(lines)
    assert metrics.compaction_count == 1, metrics.compaction_count


def test_render_tasks_md_provenance_note():
    # Open question 2: ledger.md carries a one-line provenance note once
    # any entry has been corrected; pre-backfill ledgers render unchanged.
    with_marker = session_cost.render_tasks_md([{"title": "x", "cost_backfill": "recomputed"}])
    assert "backfill-cost-recompute.py" in with_marker
    without_marker = session_cost.render_tasks_md([{"title": "x"}])
    assert "backfill-cost-recompute.py" not in without_marker


def test_branch_breakdown_dedups():
    lines = [_assistant_line("msg_0", "req_0") for _ in range(3)]
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "t.jsonl"
        _write_transcript(p, lines)
        branches = session_cost.get_branch_breakdown(str(p), "sid")
    assert branches["main"].input_tokens == USAGE["input_tokens"]
    assert branches["main"].assistant_messages == 1


def _main() -> int:
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"  PASS {name}")
            except AssertionError as exc:
                failures += 1
                print(f"  FAIL {name}: {exc}")
    print(f"{'OK' if failures == 0 else 'FAILED'} ({failures} failures)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(_main())
