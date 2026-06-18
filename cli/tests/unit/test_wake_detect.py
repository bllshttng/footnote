import os
import time
from pathlib import Path

from fno.wake.detect import SessionState, detect_session_state


def _touch(path: Path, seconds_ago: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        path.write_text("", encoding="utf-8")
    ts = time.time() - seconds_ago
    os.utime(path, (ts, ts))


def test_idle_when_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))  # isolate ~/.claude lookup
    assert detect_session_state(tmp_path) == SessionState.IDLE


def test_target_active_recent(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    state = tmp_path / ".fno" / "target-state.md"
    state.parent.mkdir(parents=True)
    state.write_text("---\nstatus: IN_PROGRESS\n---\n")
    _touch(state, 30)  # 30s ago
    assert detect_session_state(tmp_path) == SessionState.TARGET_ACTIVE


def test_target_stale_treated_as_idle(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    state = tmp_path / ".fno" / "target-state.md"
    state.parent.mkdir(parents=True)
    state.write_text("---\nstatus: IN_PROGRESS\n---\n")
    _touch(state, 600)  # 10 min ago
    assert detect_session_state(tmp_path) == SessionState.IDLE


def test_interactive_recent_transcript(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    # Encoded path mirrors Claude Code's convention (slashes -> dashes)
    encoded = str(tmp_path).replace("/", "-").lstrip("-")
    transcript_dir = tmp_path / ".claude" / "projects" / encoded
    _touch(transcript_dir / "abc.jsonl", 60)
    assert detect_session_state(tmp_path) == SessionState.INTERACTIVE_ACTIVE
