"""Tests for fno.agents.self_stamp — a2a envelope auto-stamp (x-605c, US4).

`from` defaults to the invoking session's canonical handle (floor "fno" with no
ambient identity); `model` resolves from the invoking harness's own transcript
store (floor "unknown"). `--from-name` overrides `from` verbatim.
"""
from __future__ import annotations

import json

from fno.agents import discover, self_stamp


_IDENTITY_MARKERS = (
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_SESSION_ID",
    "GEMINI_SESSION_ID",
)


def _set_identity(monkeypatch, marker, session_id):
    for name in _IDENTITY_MARKERS:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv(marker, session_id)


def _write_claude_transcript(projects_dir, *, session_id, model):
    enc = "-Users-x-proj"
    d = projects_dir / enc
    d.mkdir(parents=True, exist_ok=True)
    line = json.dumps({"type": "assistant", "message": {"model": model}})
    (d / f"{session_id}.jsonl").write_text(line + "\n", encoding="utf-8")


def _write_codex_rollout(sessions_dir, *, session_id, records):
    path = sessions_dir / "2026" / "07" / "12" / f"rollout-{session_id}.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
    return path


def test_stamp_from_explicit_wins_verbatim(monkeypatch):
    # An explicit --from-name (even the literal "fno") is returned unchanged.
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "9a063cd3-aaaa-bbbb-cccc-dddddddddddd")
    assert self_stamp.stamp_from("etl") == "etl"
    assert self_stamp.stamp_from("fno") == "fno"


def test_stamp_from_auto_canonical_handle(monkeypatch):
    _set_identity(
        monkeypatch,
        "CLAUDE_CODE_SESSION_ID",
        "9a063cd3-aaaa-bbbb-cccc-dddddddddddd",
    )
    assert self_stamp.stamp_from(None) == "9a063cd3"


def test_ac2_edge_no_ambient_identity_floors(monkeypatch):
    for var in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID",
                "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    assert self_stamp.stamp_from(None) == "fno"
    assert self_stamp.resolve_self_model() == "unknown"


def test_ac1_hp_floors_unknown_when_markers_unset_despite_reachable_transcript(
    tmp_path, monkeypatch
):
    """The markers gate identity: a reachable transcript must NOT resurrect a
    model when no ambient marker is set. This is the hermetic-preflight posture -
    a fresh CI checkout has no marker and floors to "unknown" even though a
    transcript store happens to be reachable (x-bbe7 US3a)."""
    sid = "9a063cd3-aaaa-bbbb-cccc-dddddddddddd"
    projects = tmp_path / "projects"
    _write_claude_transcript(projects, session_id=sid, model="claude-opus-4-8")
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    for name in _IDENTITY_MARKERS:
        monkeypatch.delenv(name, raising=False)
    assert self_stamp.resolve_self_model() == "unknown"


def test_ac3_hp_claude_model_from_own_transcript(tmp_path, monkeypatch):
    sid = "9a063cd3-aaaa-bbbb-cccc-dddddddddddd"
    projects = tmp_path / "projects"
    _write_claude_transcript(projects, session_id=sid, model="claude-opus-4-8")
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    _set_identity(monkeypatch, "CLAUDE_CODE_SESSION_ID", sid)
    assert self_stamp.resolve_self_model() == "claude-opus-4-8"


def test_claude_model_ignores_later_sidechain_assistant(tmp_path, monkeypatch):
    sid = "11111111-aaaa-bbbb-cccc-dddddddddddd"
    projects = tmp_path / "projects"
    transcript = projects / "-Users-x-proj" / f"{sid}.jsonl"
    transcript.parent.mkdir(parents=True)
    records = [
        {"type": "assistant", "message": {"model": "claude-opus-4-8"}},
        {
            "type": "assistant",
            "isSidechain": True,
            "message": {"model": "claude-haiku-4-5"},
        },
        {"type": "user", "message": {"model": "claude-fake"}},
    ]
    transcript.write_text(
        "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
    )
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    _set_identity(monkeypatch, "CLAUDE_CODE_SESSION_ID", sid)

    assert self_stamp.resolve_self_model() == "claude-opus-4-8"


def test_codex_model_uses_expanded_tail_before_full_scan(tmp_path, monkeypatch):
    sid = "12121212-aaaa-bbbb-cccc-dddddddddddd"
    sessions = tmp_path / "sessions"
    padding = {"type": "event_msg", "payload": {"text": "x" * 4096}}
    path = _write_codex_rollout(
        sessions,
        session_id=sid,
        records=[
            {"type": "turn_context", "payload": {"model": "gpt-expanded"}},
            *([padding] * 80),
        ],
    )
    real_complete_lines = self_stamp._complete_lines
    scanned = []

    def tracked_complete_lines(candidate, max_bytes):
        scanned.append(max_bytes)
        assert max_bytes is not None
        return real_complete_lines(candidate, max_bytes)

    monkeypatch.setattr(self_stamp, "_complete_lines", tracked_complete_lines)

    assert self_stamp._last_model(path, self_stamp._codex_record_model) == "gpt-expanded"
    assert scanned == [self_stamp._TAIL_BYTES, self_stamp._EXPANDED_TAIL_BYTES]


def test_codex_model_escalates_past_bounded_windows(tmp_path, monkeypatch):
    sid = "22222222-aaaa-bbbb-cccc-dddddddddddd"
    sessions = tmp_path / "sessions"
    padding = {"type": "event_msg", "payload": {"text": "x" * 4096}}
    records = [
        {"type": "turn_context", "payload": {"model": "gpt-a"}},
        {"type": "turn_context", "payload": {"model": "gpt-b"}},
        *([padding] * 520),
    ]
    _write_codex_rollout(sessions, session_id=sid, records=records)
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(sessions))
    _set_identity(monkeypatch, "CODEX_THREAD_ID", sid)

    assert self_stamp.resolve_self_model() == "gpt-b"


def test_codex_model_ignores_ineligible_and_malformed_records(tmp_path, monkeypatch):
    sid = "33333333-aaaa-bbbb-cccc-dddddddddddd"
    sessions = tmp_path / "sessions"
    path = _write_codex_rollout(
        sessions,
        session_id=sid,
        records=[
            {"type": "turn_context", "payload": {"model": "gpt-real"}},
            {"type": "response_item", "payload": {"model": "gpt-fake"}},
        ],
    )
    with path.open("a", encoding="utf-8") as fh:
        fh.write('{"type":"turn_context","payload":{"model":"gpt-broken"}\n')
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(sessions))
    _set_identity(monkeypatch, "CODEX_THREAD_ID", sid)

    assert self_stamp.resolve_self_model() == "gpt-real"


def test_codex_model_waits_for_terminal_newline_commit(tmp_path, monkeypatch):
    sid = "44444444-aaaa-bbbb-cccc-dddddddddddd"
    sessions = tmp_path / "sessions"
    path = _write_codex_rollout(
        sessions,
        session_id=sid,
        records=[{"type": "turn_context", "payload": {"model": "gpt-old"}}],
    )
    with path.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps({"type": "turn_context", "payload": {"model": "gpt-new"}}))
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(sessions))
    _set_identity(monkeypatch, "CODEX_THREAD_ID", sid)

    assert self_stamp.resolve_self_model() == "gpt-old"

    with path.open("a", encoding="utf-8") as fh:
        fh.write("\n")
    assert self_stamp.resolve_self_model() == "gpt-new"


def test_model_floors_unknown_when_transcript_absent(tmp_path, monkeypatch):
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(tmp_path / "empty"))
    _set_identity(
        monkeypatch,
        "CLAUDE_CODE_SESSION_ID",
        "deadbeef-0000-0000-0000-000000000000",
    )
    assert self_stamp.resolve_self_model() == "unknown"
