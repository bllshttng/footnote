"""Tests for fno.agents.providers._claude_session_registry.

US2 Task 2.2: helpers that read claude 2.1.143's session registry and
jobs directory. All paths are derived from ``Path.home()`` so the tests
monkeypatch ``HOME`` to a tmp directory.

Covers acceptance criteria for Task 2.2:
  - locate_session(short_id) returns SessionLocator when jobId matches AND kind="bg"
  - locate_session returns None when no entry matches OR messagingSocketPath=null
  - read_state_json parses state.json; retries once on JSONDecodeError
  - read_timeline_tail reads from byte offset; concatenates text from terminal/needs-input rows
  - HOME monkeypatchable (all paths via Path.home())
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


def _claude_home_setup(tmp_path: Path, monkeypatch) -> Path:
    """Point HOME at tmp_path and prepare ~/.claude/{sessions,jobs}."""
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / ".claude" / "sessions").mkdir(parents=True)
    (tmp_path / ".claude" / "jobs").mkdir(parents=True)
    return tmp_path


def _write_session_file(home: Path, pid: int, **fields) -> Path:
    """Write a ~/.claude/sessions/<pid>.json with the given fields."""
    base = {
        "messagingSocketPath": fields.get("messagingSocketPath", f"/tmp/sock-{pid}"),
        "jobId": fields["jobId"],
        "kind": fields.get("kind", "bg"),
        "sessionId": fields.get("sessionId", f"sess-{pid}"),
        "cwd": fields.get("cwd", "/tmp"),
        "status": fields.get("status", "running"),
        "state": fields.get("state", "idle"),
        "detail": fields.get("detail", ""),
        "tempo": fields.get("tempo", 1),
    }
    path = home / ".claude" / "sessions" / f"{pid}.json"
    path.write_text(json.dumps(base), encoding="utf-8")
    return path


def _write_state_json(home: Path, short_id: str, payload: dict) -> Path:
    jobs_dir = home / ".claude" / "jobs" / short_id
    jobs_dir.mkdir(parents=True, exist_ok=True)
    path = jobs_dir / "state.json"
    path.write_text(json.dumps(payload), encoding="utf-8")
    return path


def _write_timeline_jsonl(home: Path, short_id: str, rows: list[dict]) -> Path:
    jobs_dir = home / ".claude" / "jobs" / short_id
    jobs_dir.mkdir(parents=True, exist_ok=True)
    path = jobs_dir / "timeline.jsonl"
    path.write_text("\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8")
    return path


# ---------------------------------------------------------------------------
# locate_session
# ---------------------------------------------------------------------------


def test_locate_session_returns_locator_when_jobid_matches(tmp_path, monkeypatch):
    """jobId match + kind=bg + non-null socket -> SessionLocator returned."""
    from fno.agents.providers._claude_session_registry import locate_session

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_session_file(home, pid=12345, jobId="7c5dcf5d", kind="bg",
                        messagingSocketPath="/tmp/sock.msg")

    loc = locate_session("7c5dcf5d")
    assert loc is not None
    assert loc.pid == 12345
    assert loc.messaging_socket_path == "/tmp/sock.msg"
    assert loc.jobs_dir == home / ".claude" / "jobs" / "7c5dcf5d"


def test_locate_session_returns_none_when_no_match(tmp_path, monkeypatch):
    """No session file with that jobId -> None (orphan)."""
    from fno.agents.providers._claude_session_registry import locate_session

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_session_file(home, pid=1, jobId="aaaaaaaa")
    _write_session_file(home, pid=2, jobId="bbbbbbbb")

    assert locate_session("ffffffff") is None


def test_locate_session_returns_none_when_socket_null(tmp_path, monkeypatch):
    """Match exists but messagingSocketPath=null (suspended) -> None."""
    from fno.agents.providers._claude_session_registry import locate_session

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_session_file(home, pid=99, jobId="abc12345",
                        messagingSocketPath=None)

    assert locate_session("abc12345") is None


def test_locate_session_skips_non_bg_kind(tmp_path, monkeypatch):
    """Interactive session with matching jobId is skipped (kind != "bg")."""
    from fno.agents.providers._claude_session_registry import locate_session

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_session_file(home, pid=1, jobId="match01a", kind="interactive")

    assert locate_session("match01a") is None


def test_locate_session_ignores_malformed_session_files(tmp_path, monkeypatch):
    """A corrupt JSON file in sessions/ is skipped, not raised."""
    from fno.agents.providers._claude_session_registry import locate_session

    home = _claude_home_setup(tmp_path, monkeypatch)
    (home / ".claude" / "sessions" / "junk.json").write_text(
        "not json {", encoding="utf-8"
    )
    _write_session_file(home, pid=2, jobId="good0001")

    loc = locate_session("good0001")
    assert loc is not None
    assert loc.pid == 2


def test_session_locator_carries_session_id_and_cwd(tmp_path, monkeypatch):
    """SessionLocator preserves sessionId and cwd from the session file."""
    from fno.agents.providers._claude_session_registry import locate_session

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_session_file(
        home, pid=42, jobId="deadbeef",
        sessionId="claude-sess-42", cwd="/work/project",
    )

    loc = locate_session("deadbeef")
    assert loc.session_id == "claude-sess-42"
    assert loc.cwd == "/work/project"


# ---------------------------------------------------------------------------
# read_state_json
# ---------------------------------------------------------------------------


def test_read_state_json_returns_parsed_snapshot(tmp_path, monkeypatch):
    """state.json parses into a StateSnapshot with the documented fields."""
    from fno.agents.providers._claude_session_registry import read_state_json

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "abc12345"
    _write_state_json(
        home, "abc12345",
        {
            "state": "completed",
            "updatedAt": "2026-05-20T22:00:00Z",
            "output": {"result": "all done"},
            "intent": "user-message",
        },
    )

    snap = read_state_json(jobs_dir)
    assert snap.state == "completed"
    assert snap.updated_at == "2026-05-20T22:00:00Z"
    assert snap.output_result == "all done"
    assert snap.intent == "user-message"


def test_read_state_json_retries_once_on_json_decode_error(tmp_path, monkeypatch):
    """Atomic-rename window: read_state_json retries once on JSONDecodeError."""
    from fno.agents.providers import _claude_session_registry as mod

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "race0001"
    jobs_dir.mkdir(parents=True, exist_ok=True)
    state_path = jobs_dir / "state.json"
    # First read sees an empty file (caught mid-rename); second sees valid JSON.
    state_path.write_text("", encoding="utf-8")

    real_read_text = Path.read_text
    call_count = {"n": 0}

    def fake_read_text(self, *args, **kwargs):
        if self == state_path:
            call_count["n"] += 1
            if call_count["n"] == 1:
                return ""  # invalid
            return json.dumps(
                {"state": "done", "updatedAt": "T1", "output": {"result": "x"}}
            )
        return real_read_text(self, *args, **kwargs)

    monkeypatch.setattr(Path, "read_text", fake_read_text)
    # Make retry instantaneous so the test stays fast.
    monkeypatch.setattr(mod, "_RETRY_BACKOFF_SEC", 0.0)

    snap = mod.read_state_json(jobs_dir)
    assert call_count["n"] == 2
    assert snap.state == "done"
    assert snap.output_result == "x"


def test_read_state_json_raises_after_second_failure(tmp_path, monkeypatch):
    """If retry also fails, the underlying JSONDecodeError surfaces."""
    from fno.agents.providers import _claude_session_registry as mod

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "broken01"
    jobs_dir.mkdir(parents=True, exist_ok=True)
    (jobs_dir / "state.json").write_text("not even {valid", encoding="utf-8")

    monkeypatch.setattr(mod, "_RETRY_BACKOFF_SEC", 0.0)

    with pytest.raises(json.JSONDecodeError):
        mod.read_state_json(jobs_dir)


def test_read_state_json_handles_missing_output_result(tmp_path, monkeypatch):
    """output.result may be absent (e.g., needs-input); snap.output_result is None."""
    from fno.agents.providers._claude_session_registry import read_state_json

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "noresult"
    _write_state_json(
        home, "noresult",
        {"state": "needs-input", "updatedAt": "T0", "output": {}},
    )

    snap = read_state_json(jobs_dir)
    assert snap.state == "needs-input"
    assert snap.output_result is None


# ---------------------------------------------------------------------------
# read_timeline_tail
# ---------------------------------------------------------------------------


def test_read_timeline_tail_returns_empty_when_offset_is_eof(tmp_path, monkeypatch):
    """If offset equals the current file size, tail is empty."""
    from fno.agents.providers._claude_session_registry import read_timeline_tail

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "empty001"
    timeline = _write_timeline_jsonl(home, "empty001", [
        {"at": "T0", "state": "running", "detail": "", "text": "old"},
    ])
    size = timeline.stat().st_size

    assert read_timeline_tail(jobs_dir, offset=size) == ""


def test_read_timeline_tail_concatenates_terminal_state_text(tmp_path, monkeypatch):
    """Only lines with state in {done, completed, failed, needs-input} contribute text."""
    from fno.agents.providers._claude_session_registry import read_timeline_tail

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "mixed001"
    timeline = _write_timeline_jsonl(home, "mixed001", [
        {"at": "T0", "state": "running", "text": "should-be-ignored"},
        {"at": "T1", "state": "done", "text": "first part"},
        {"at": "T2", "state": "completed", "text": "second part"},
        {"at": "T3", "state": "idle", "text": "not terminal"},
    ])
    # offset=0 reads everything appended since baseline.
    tail = read_timeline_tail(jobs_dir, offset=0)
    assert "first part" in tail
    assert "second part" in tail
    assert "should-be-ignored" not in tail
    assert "not terminal" not in tail


def test_read_timeline_tail_starts_at_offset(tmp_path, monkeypatch):
    """Tail reads only bytes >= offset; earlier rows are excluded."""
    from fno.agents.providers._claude_session_registry import read_timeline_tail

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "offset01"

    # Write a baseline row, capture its size, then append a post-baseline row.
    timeline = _write_timeline_jsonl(home, "offset01", [
        {"at": "T0", "state": "done", "text": "pre-baseline"},
    ])
    baseline = timeline.stat().st_size

    with open(timeline, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(
            {"at": "T1", "state": "done", "text": "post-baseline"}
        ) + "\n")

    tail = read_timeline_tail(jobs_dir, offset=baseline)
    assert "post-baseline" in tail
    assert "pre-baseline" not in tail


def test_read_timeline_tail_handles_missing_file(tmp_path, monkeypatch):
    """timeline.jsonl missing -> empty string (job hasn't emitted yet)."""
    from fno.agents.providers._claude_session_registry import read_timeline_tail

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "gone0001"
    jobs_dir.mkdir(parents=True, exist_ok=True)

    assert read_timeline_tail(jobs_dir, offset=0) == ""


def test_read_timeline_tail_skips_malformed_lines(tmp_path, monkeypatch):
    """Garbage JSONL lines are skipped, not raised."""
    from fno.agents.providers._claude_session_registry import read_timeline_tail

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "garbage1"
    jobs_dir.mkdir(parents=True, exist_ok=True)
    (jobs_dir / "timeline.jsonl").write_text(
        json.dumps({"at": "T0", "state": "done", "text": "alpha"}) + "\n"
        + "not json {\n"
        + json.dumps({"at": "T1", "state": "completed", "text": "beta"}) + "\n",
        encoding="utf-8",
    )

    tail = read_timeline_tail(jobs_dir, offset=0)
    assert "alpha" in tail
    assert "beta" in tail


# ---------------------------------------------------------------------------
# HOME monkeypatch invariant
# ---------------------------------------------------------------------------


def test_paths_resolved_via_path_home(tmp_path, monkeypatch):
    """All session/jobs paths must derive from Path.home() so HOME is the only knob."""
    from fno.agents.providers._claude_session_registry import (
        _sessions_dir,
        _jobs_dir_for,
    )

    monkeypatch.setenv("HOME", str(tmp_path))
    assert _sessions_dir() == tmp_path / ".claude" / "sessions"
    assert _jobs_dir_for("xyz12345") == tmp_path / ".claude" / "jobs" / "xyz12345"


# ---------------------------------------------------------------------------
# resolve_session_uuid (Task 1.1 - socket-independent full-UUID resolution)
# ---------------------------------------------------------------------------


def test_resolve_session_uuid_returns_uuid_for_idle_socket_null(tmp_path, monkeypatch):
    """An IDLE bg session (messagingSocketPath=null) still resolves its full
    UUID. This is the whole point: locate_session SKIPS socket-null sessions,
    but an idle session is exactly the stream-json resume target, so the
    resolver must read sessionId regardless of socket state."""
    from fno.agents.providers._claude_session_registry import resolve_session_uuid

    home = _claude_home_setup(tmp_path, monkeypatch)
    full = "019e7157-4236-7bb1-b274-ebbac6040ace"
    _write_session_file(
        home, pid=4242, jobId="7c5dcf5d", kind="bg",
        messagingSocketPath=None, sessionId=full,
    )

    assert resolve_session_uuid("7c5dcf5d") == full


def test_resolve_session_uuid_returns_uuid_for_live_socket(tmp_path, monkeypatch):
    """A live (non-null socket) bg session resolves its UUID too."""
    from fno.agents.providers._claude_session_registry import resolve_session_uuid

    home = _claude_home_setup(tmp_path, monkeypatch)
    full = "019e7157-4236-7bb1-b274-ebbac6040ace"
    _write_session_file(
        home, pid=4243, jobId="aa11bb22", kind="bg",
        messagingSocketPath="/tmp/sock.msg", sessionId=full,
    )

    assert resolve_session_uuid("aa11bb22") == full


def test_resolve_session_uuid_prefers_live_socket_supervisor(tmp_path, monkeypatch):
    """After a supervisor auto-update respawn, the dead pid's file (socket-null)
    and the live pid's file can share a jobId. The resolver prefers the live
    supervisor's sessionId but still resolves if only the stale one remains."""
    from fno.agents.providers._claude_session_registry import resolve_session_uuid

    home = _claude_home_setup(tmp_path, monkeypatch)
    stale = "00000000-0000-0000-0000-000000000000"
    live = "019e7157-4236-7bb1-b274-ebbac6040ace"
    _write_session_file(
        home, pid=100, jobId="dup00000", kind="bg",
        messagingSocketPath=None, sessionId=stale,
    )
    _write_session_file(
        home, pid=200, jobId="dup00000", kind="bg",
        messagingSocketPath="/tmp/live.msg", sessionId=live,
    )

    assert resolve_session_uuid("dup00000") == live


def test_resolve_session_uuid_returns_none_when_not_found(tmp_path, monkeypatch):
    """No session file with a matching jobId -> None (never ran / typo)."""
    from fno.agents.providers._claude_session_registry import resolve_session_uuid

    _claude_home_setup(tmp_path, monkeypatch)
    assert resolve_session_uuid("ffffffff") is None


def test_resolve_session_uuid_ignores_non_bg_kind(tmp_path, monkeypatch):
    """An interactive (kind != bg) session with the matching jobId is ignored:
    the stream-json lane only resumes bg supervisor transcripts."""
    from fno.agents.providers._claude_session_registry import resolve_session_uuid

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_session_file(
        home, pid=300, jobId="11112222", kind="interactive",
        messagingSocketPath="/tmp/i.msg", sessionId="should-not-be-returned",
    )
    assert resolve_session_uuid("11112222") is None


def test_resolve_session_uuid_none_when_sessions_dir_absent(tmp_path, monkeypatch):
    """No ~/.claude/sessions dir (claude never ran) -> None, not a crash."""
    from fno.agents.providers._claude_session_registry import resolve_session_uuid

    monkeypatch.setenv("HOME", str(tmp_path))  # no .claude tree created
    assert resolve_session_uuid("7c5dcf5d") is None


# ---------------------------------------------------------------------------
# roster_live (x-2681): daemon-roster membership check for the ask fallback
# ---------------------------------------------------------------------------

_ROSTER_UUID = "abc12345-1111-2222-3333-444455556666"


def _write_roster(home: Path, workers: dict) -> Path:
    daemon = home / ".claude" / "daemon"
    daemon.mkdir(parents=True, exist_ok=True)
    path = daemon / "roster.json"
    path.write_text(json.dumps({"workers": workers}), encoding="utf-8")
    return path


def test_roster_live_true_when_short_id_present(tmp_path, monkeypatch):
    from fno.agents.providers._claude_session_registry import roster_live

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_roster(home, {"w1": {"sessionId": _ROSTER_UUID, "pid": 5}})
    assert roster_live("abc12345") is True


def test_roster_live_false_when_short_id_absent(tmp_path, monkeypatch):
    from fno.agents.providers._claude_session_registry import roster_live

    home = _claude_home_setup(tmp_path, monkeypatch)
    _write_roster(home, {"w1": {"sessionId": "deadbeef-1111-2222-3333-444455556666"}})
    assert roster_live("abc12345") is False


def test_roster_live_false_when_roster_missing(tmp_path, monkeypatch):
    from fno.agents.providers._claude_session_registry import roster_live

    _claude_home_setup(tmp_path, monkeypatch)  # no roster.json written
    assert roster_live("abc12345") is False


def test_roster_live_lenient_on_torn_roster(tmp_path, monkeypatch):
    """A torn/garbage roster degrades to False, never raises (procStart drift)."""
    from fno.agents.providers._claude_session_registry import roster_live

    home = _claude_home_setup(tmp_path, monkeypatch)
    daemon = home / ".claude" / "daemon"
    daemon.mkdir(parents=True, exist_ok=True)
    (daemon / "roster.json").write_text("{not json", encoding="utf-8")
    assert roster_live("abc12345") is False


def test_roster_live_honors_daemon_dir_env(tmp_path, monkeypatch):
    from fno.agents.providers._claude_session_registry import roster_live

    _claude_home_setup(tmp_path, monkeypatch)
    alt = tmp_path / "altdaemon"
    alt.mkdir()
    (alt / "roster.json").write_text(
        json.dumps({"workers": {"w": {"sessionId": _ROSTER_UUID}}}),
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(alt))
    assert roster_live("abc12345") is True


def test_read_state_json_rejects_non_object_json(tmp_path, monkeypatch):
    """A valid-but-non-object state.json (list/primitive) raises JSONDecodeError,
    not AttributeError, so callers' `except json.JSONDecodeError` degrades cleanly
    (gemini review on PR #293)."""
    import json as _json

    from fno.agents.providers._claude_session_registry import read_state_json

    home = _claude_home_setup(tmp_path, monkeypatch)
    jobs_dir = home / ".claude" / "jobs" / "abc12345"
    jobs_dir.mkdir(parents=True)
    (jobs_dir / "state.json").write_text("[]", encoding="utf-8")

    with pytest.raises(_json.JSONDecodeError):
        read_state_json(jobs_dir)
