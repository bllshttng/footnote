"""Unit tests for fno.provenance.autobrief (x-d1f4).

The dispatch-brief priority chain + mechanical synthesis + per-harness
transcript tail. Every store is injected so no read ever touches the
developer's real ~/.claude, ~/.codex, or opencode.db.
"""
from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from fno.provenance.autobrief import _BRIEF_MAX_BYTES, resolve_dispatch_brief


@pytest.fixture(autouse=True)
def _no_ambient_self(monkeypatch):
    """Deterministic envelope: no live dispatcher identity unless a test sets it."""
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)


# --------------------------------------------------------------------------- #
# Store fixtures
# --------------------------------------------------------------------------- #

def _write_claude_transcript(root: Path, cwd: str, sid: str, records: list[dict]) -> None:
    slug = cwd.replace("/", "-").replace(".", "-")
    proj = root / slug
    proj.mkdir(parents=True, exist_ok=True)
    (proj / f"{sid}.jsonl").write_text(
        "\n".join(json.dumps(r) for r in records) + "\n", encoding="utf-8"
    )


def _claude_msg(role: str, text: str, ts: str) -> dict:
    return {
        "type": role,
        "timestamp": ts,
        "message": {"role": role, "content": [{"type": "text", "text": text}]},
    }


def _make_opencode_db(tmp_path: Path, sid: str, turns: list[tuple[str, str, int]]) -> Path:
    db = tmp_path / "opencode.db"
    con = sqlite3.connect(db)
    con.executescript(
        """
        CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                              time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                           time_created INTEGER, data TEXT);
        """
    )
    con.execute("INSERT INTO session VALUES (?,?,?)", (sid, "/Users/bb16", 0))
    for i, (role, text, ts_ms) in enumerate(turns):
        mid = f"msg_{i}"
        con.execute(
            "INSERT INTO message VALUES (?,?,?,?)",
            (mid, sid, ts_ms, json.dumps({"role": role})),
        )
        con.execute(
            "INSERT INTO part VALUES (?,?,?,?,?)",
            (f"prt_{i}", mid, sid, ts_ms, json.dumps({"type": "text", "text": text})),
        )
    con.commit()
    con.close()
    return db


# --------------------------------------------------------------------------- #
# AC3-HP / AC6-ERR: explicit rung
# --------------------------------------------------------------------------- #

def test_ac3_explicit_brief_wins(tmp_path):
    """AC3-HP: dispatch_brief + sidecar + details all present -> explicit wins."""
    (tmp_path / "x-1.md").write_text("SIDECAR", encoding="utf-8")
    node = {
        "id": "x-1",
        "dispatch_brief": "EXPLICIT",
        "has_brief": True,
        "details": "some details",
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert brief == "EXPLICIT"
    assert tag == "explicit"


def test_ac6_explicit_over_budget_returned_verbatim():
    """AC6-ERR: an oversized explicit brief is returned VERBATIM (the >8 KB
    fail-closed error lives downstream in harness_map, not here)."""
    big = "x" * (_BRIEF_MAX_BYTES + 1000)
    brief, tag = resolve_dispatch_brief({"id": "x-1", "dispatch_brief": big})
    assert brief == big
    assert tag == "explicit"


# --------------------------------------------------------------------------- #
# AC2-HP / AC6-ERR: sidecar rung
# --------------------------------------------------------------------------- #

def test_ac2_sidecar_rides(tmp_path):
    """AC2-HP: has_brief + non-empty sidecar -> its content, tag=sidecar."""
    (tmp_path / "x-2.md").write_text("the sidecar brief body", encoding="utf-8")
    node = {"id": "x-2", "has_brief": True, "details": "ignored when sidecar present"}
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert brief == "the sidecar brief body"
    assert tag == "sidecar"


def test_sidecar_empty_falls_through_to_synthesis(tmp_path):
    """An empty sidecar is skipped (non-empty required) -> synthesis."""
    (tmp_path / "x-2.md").write_text("   \n  ", encoding="utf-8")
    node = {"id": "x-2", "has_brief": True, "details": "real details here"}
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert tag == "synth-details"
    assert "real details here" in brief


def test_sidecar_missing_file_falls_through(tmp_path):
    """has_brief=True but no file on disk -> synthesis, never a crash."""
    node = {"id": "x-2", "has_brief": True, "details": "real details here"}
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert tag == "synth-details"


def test_ac6_oversized_sidecar_clamped_with_marker(tmp_path):
    """AC6-ERR: a 20 KB sidecar clamps to <= 8192 bytes ending in the marker."""
    (tmp_path / "x-2.md").write_text("y" * 20000, encoding="utf-8")
    node = {"id": "x-2", "has_brief": True}
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert tag == "sidecar"
    assert len(brief.encode("utf-8")) <= _BRIEF_MAX_BYTES
    assert brief.endswith("[truncated]")


def test_clamp_never_splits_multibyte_codepoint(tmp_path):
    """The byte clamp must land on a codepoint boundary (utf-8 safety)."""
    # Each 'é' is 2 bytes; a naive byte slice could split one.
    (tmp_path / "x-2.md").write_text("é" * 10000, encoding="utf-8")
    node = {"id": "x-2", "has_brief": True}
    brief, _ = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    # Re-encoding must round-trip cleanly (no replacement chars from a split).
    assert "�" not in brief
    assert len(brief.encode("utf-8")) <= _BRIEF_MAX_BYTES


# --------------------------------------------------------------------------- #
# AC1-HP: details synthesize
# --------------------------------------------------------------------------- #

def test_ac1_details_synthesize(tmp_path):
    """AC1-HP: details set, no dispatch_brief, no sidecar -> id + title + details,
    tag=synth-details."""
    node = {
        "id": "x-1234",
        "slug": "add-retry",
        "title": "Add retry logic",
        "details": "We need exponential backoff on the dispatch path. " * 5,
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert tag == "synth-details"
    assert "x-1234" in brief
    assert "Add retry logic" in brief
    assert "exponential backoff" in brief
    assert brief.startswith("<fno_spawn ")
    assert 'source="synth-details"' in brief


def test_description_used_when_details_absent(tmp_path):
    """details missing -> falls back to description."""
    node = {"id": "x-1", "title": "T", "description": "the description text " * 5}
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert tag == "synth-details"
    assert "the description text" in brief


def test_envelope_from_omitted_when_no_provenance(tmp_path):
    """Locked Decision 10: attributes degrade by omission, never fabricated."""
    node = {"id": "x-1", "title": "T", "details": "d " * 300}
    brief, _ = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert "from=" not in brief
    assert "reply:" not in brief


def test_envelope_from_uses_live_dispatcher_identity(tmp_path, monkeypatch):
    """from precedence: a live dispatching session's ambient identity wins."""
    monkeypatch.setenv("FNO_AGENT_SELF", "athens")
    node = {"id": "x-1", "title": "T", "details": "d " * 300,
            "source_session_id": "20260506T213611Z-58489-6764ea"}
    brief, _ = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert 'from="athens"' in brief
    assert "reply: fno mail send athens" in brief


def test_envelope_from_falls_back_to_source_session(tmp_path):
    """from precedence: no live identity -> node source_session_id provenance."""
    node = {"id": "x-1", "title": "T", "details": "d " * 300,
            "source_session_id": "20260506T213611Z-58489-6764ea"}
    brief, _ = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert 'from="20260506T213611Z-58489-6764ea"' in brief


# --------------------------------------------------------------------------- #
# AC7-EDGE / AC8-EDGE: transcript tail
# --------------------------------------------------------------------------- #

def test_ac7_thin_details_pull_tail_near_created_at(tmp_path):
    """AC7-EDGE: thin details + a resolvable claude transcript -> tail near
    created_at rides; records after created_at+120s are excluded."""
    cwd = "/Users/bb16/code/footnote"
    sid = "abcd1234-0000-0000-0000-000000000000"
    _write_claude_transcript(
        tmp_path,
        cwd,
        sid,
        [
            _claude_msg("user", "the motivating question", "2026-07-21T00:00:00Z"),
            _claude_msg("assistant", "the motivating answer", "2026-07-21T00:01:00Z"),
            # Well after created_at + 120s: an unrelated later topic.
            _claude_msg("user", "UNRELATED_LATER_TOPIC", "2026-07-21T02:00:00Z"),
        ],
    )
    node = {
        "id": "x-1",
        "title": "T",
        "details": "one-liner",  # < 400 bytes -> thin -> pull tail
        "source_harness": "claude",
        "source_session_id": sid,
        "source_cwd": cwd,
        "created_at": "2026-07-21T00:00:00Z",
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path, projects_root=tmp_path)
    assert tag == "synth-details+tail"
    assert "one-liner" in brief
    assert "the motivating question" in brief
    assert "UNRELATED_LATER_TOPIC" not in brief


def test_tail_unparseable_created_at_falls_back_to_plain_tail(tmp_path):
    """AC7-EDGE: with an unparseable created_at the brief falls back to the file
    tail (tag unchanged, no error)."""
    cwd = "/Users/bb16/code/footnote"
    sid = "abcd1234-0000-0000-0000-000000000001"
    _write_claude_transcript(
        tmp_path, cwd, sid,
        [_claude_msg("user", "some conversation", "2026-07-21T00:00:00Z")],
    )
    node = {
        "id": "x-1", "title": "T", "details": "one-liner",
        "source_harness": "claude", "source_session_id": sid, "source_cwd": cwd,
        "created_at": "not-a-timestamp",
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path, projects_root=tmp_path)
    assert tag == "synth-details+tail"
    assert "some conversation" in brief


def test_ac8_ambiguous_resolution_skips_tail(tmp_path):
    """AC8-EDGE: an ambiguous claude prefix-glob skips the tail rung entirely."""
    cwd = "/Users/bb16/code/footnote"
    slug = cwd.replace("/", "-").replace(".", "-")
    proj = tmp_path / slug
    proj.mkdir(parents=True, exist_ok=True)
    # Two files under the same 8-hex prefix -> ambiguous.
    for suffix in ("aaaa", "bbbb"):
        (proj / f"abcd1234-{suffix}-0000-0000-000000000000.jsonl").write_text(
            json.dumps(_claude_msg("user", "wrong-session-text", "2026-07-21T00:00:00Z")) + "\n",
            encoding="utf-8",
        )
    node = {
        "id": "x-1", "title": "T", "details": "one-liner",
        "source_harness": "claude", "source_session_id": "abcd1234",
        "source_cwd": cwd, "created_at": "2026-07-21T00:00:00Z",
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path, projects_root=tmp_path)
    assert tag == "synth-details"  # tail skipped
    assert "wrong-session-text" not in brief


def test_tail_extracts_from_opencode_store(tmp_path):
    """A thin-details node with an opencode source resolves + reads the SQLite tail."""
    sid = "ses_test"
    db = _make_opencode_db(
        tmp_path, sid,
        [("user", "opencode question", 1000), ("assistant", "opencode reply", 2000)],
    )
    node = {
        "id": "x-1", "title": "T", "details": "one-liner",
        "source_harness": "opencode", "source_session_id": sid,
        "created_at": None,  # no window -> plain tail
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path, opencode_db_path=db)
    assert tag == "synth-details+tail"
    assert "opencode question" in brief
    assert "opencode reply" in brief


def test_tail_extracts_from_codex_rollout(tmp_path):
    """A thin-details node with a codex source reads the rollout tail."""
    sid = "019f837c-3461-7911-811f-3290b8b34934"
    cwd = "/Users/bb16/code/footnote"
    day = tmp_path / "2026" / "07" / "21"
    day.mkdir(parents=True, exist_ok=True)
    (day / f"rollout-2026-07-21T00-00-00-{sid}.jsonl").write_text(
        "\n".join(
            json.dumps(r)
            for r in [
                {"type": "session_meta", "payload": {"id": sid, "cwd": cwd}},
                {
                    "type": "response_item",
                    "timestamp": "2026-07-21T00:00:10Z",
                    "payload": {
                        "type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "codex question"}],
                    },
                },
                {
                    "type": "response_item",
                    "timestamp": "2026-07-21T00:00:20Z",
                    "payload": {
                        "type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "codex answer"}],
                    },
                },
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    node = {
        "id": "x-1", "title": "T", "details": "one-liner",
        "source_harness": "codex", "source_session_id": sid, "source_cwd": cwd,
        "created_at": None,
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path, codex_sessions_dir=tmp_path)
    assert tag == "synth-details+tail"
    assert "codex question" in brief
    assert "codex answer" in brief


# --------------------------------------------------------------------------- #
# AC5-ERR / AC9-FR: degrade + none
# --------------------------------------------------------------------------- #

def test_ac9_nothing_to_synthesize_returns_none(tmp_path):
    """AC9-FR: nothing to synthesize -> (None, 'none')."""
    node = {"id": "x-1"}  # no brief, no sidecar, no details, no provenance
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert brief is None
    assert tag == "none"


def test_ac5_unresolvable_transcript_degrades_to_details_only(tmp_path):
    """AC5-ERR: a thin-details node whose transcript store errors falls to
    details-only, no exception."""
    node = {
        "id": "x-1", "title": "T", "details": "one-liner",
        "source_harness": "claude",
        "source_session_id": "missing-0000-0000-0000-000000000000",
        "source_cwd": "/Users/bb16/code/footnote",
        "created_at": "2026-07-21T00:00:00Z",
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path, projects_root=tmp_path)
    assert tag == "synth-details"
    assert "one-liner" in brief


def test_details_only_when_no_source_provenance(tmp_path):
    """A thin-details node with no source_* provenance stays details-only."""
    node = {"id": "x-1", "title": "T", "details": "one-liner"}
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path)
    assert tag == "synth-details"


def test_never_raises_on_garbage_node(tmp_path):
    """The whole feature is best-effort: a malformed node never raises."""
    for bad in [{}, {"id": None}, {"details": 12345}, {"dispatch_brief": 999}]:
        brief, tag = resolve_dispatch_brief(bad, briefs_dir=tmp_path)
        assert tag in {"none", "synth-details", "synth-details+tail", "synth-tail"}


def test_all_records_after_cutoff_omits_tail(tmp_path):
    """codex review P1: when every timestamped record falls after created_at+120s
    (the node's session kept going long after filing and the bounded tail holds
    only later turns), the tail is OMITTED, not filled with the later unrelated
    conversation."""
    cwd = "/Users/bb16/code/footnote"
    sid = "abcd1234-0000-0000-0000-00000000dead"
    _write_claude_transcript(
        tmp_path, cwd, sid,
        [
            # Both records are well after created_at + 120s.
            _claude_msg("user", "LATER_UNRELATED_ONE", "2026-07-21T05:00:00Z"),
            _claude_msg("assistant", "LATER_UNRELATED_TWO", "2026-07-21T05:01:00Z"),
        ],
    )
    node = {
        "id": "x-1", "title": "T", "details": "one-liner",
        "source_harness": "claude", "source_session_id": sid, "source_cwd": cwd,
        "created_at": "2026-07-21T00:00:00Z",
    }
    brief, tag = resolve_dispatch_brief(node, briefs_dir=tmp_path, projects_root=tmp_path)
    assert tag == "synth-details"  # tail omitted, not injected
    assert "LATER_UNRELATED" not in brief
