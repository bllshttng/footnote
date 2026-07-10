"""Tests for fno.agents.self_stamp — a2a envelope auto-stamp (x-605c, US4).

`from` defaults to the invoking session's canonical handle (floor "fno" with no
ambient identity); `model` resolves from the invoking harness's own transcript
store (floor "unknown"). `--from-name` overrides `from` verbatim.
"""
from __future__ import annotations

import json

from fno.agents import discover, self_stamp


def _write_claude_transcript(projects_dir, *, session_id, model):
    enc = "-Users-x-proj"
    d = projects_dir / enc
    d.mkdir(parents=True, exist_ok=True)
    line = json.dumps({"type": "assistant", "message": {"model": model}})
    (d / f"{session_id}.jsonl").write_text(line + "\n", encoding="utf-8")


def test_stamp_from_explicit_wins_verbatim(monkeypatch):
    # An explicit --from-name (even the literal "fno") is returned unchanged.
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "9a063cd3-aaaa-bbbb-cccc-dddddddddddd")
    assert self_stamp.stamp_from("etl") == "etl"
    assert self_stamp.stamp_from("fno") == "fno"


def test_stamp_from_auto_canonical_handle(monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "9a063cd3-aaaa-bbbb-cccc-dddddddddddd")
    assert self_stamp.stamp_from(None) == "claude-9a063cd3"


def test_ac2_edge_no_ambient_identity_floors(monkeypatch):
    for var in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID",
                "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    assert self_stamp.stamp_from(None) == "fno"
    assert self_stamp.resolve_self_model() == "unknown"


def test_ac3_hp_claude_model_from_own_transcript(tmp_path, monkeypatch):
    sid = "9a063cd3-aaaa-bbbb-cccc-dddddddddddd"
    projects = tmp_path / "projects"
    _write_claude_transcript(projects, session_id=sid, model="claude-opus-4-8")
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sid)
    assert self_stamp.resolve_self_model() == "claude-opus-4-8"


def test_model_floors_unknown_when_transcript_absent(tmp_path, monkeypatch):
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(tmp_path / "empty"))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "deadbeef-0000-0000-0000-000000000000")
    assert self_stamp.resolve_self_model() == "unknown"
