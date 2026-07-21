"""Tests for sigma_dispatch emitter scaffolding (Tasks 1.2 and 1.3).

Covers emit_subagent_spawn, emit_subagent_complete, EventEmitFailed,
and the record_dispatch sidecar writer.

Run: cd cli && uv run pytest src/fno/test_sigma_dispatch.py -v
"""
from __future__ import annotations

import concurrent.futures
import json
import re
import subprocess
from pathlib import Path

import pytest


def _setup_target_state(tmp_path: Path, *, session_id: str, nonce: str) -> Path:
    """Bootstrap a minimal target-state.md so the shell helper finds session context."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / "target-state.md"
    state_file.write_text(
        f"---\nstatus: IN_PROGRESS\nsession_id: {session_id}\n"
        f"provenance_nonce: {nonce}\n---\n",
        encoding="utf-8",
    )
    return state_dir


# ---------------------------------------------------------------------------
# AC1-HP: emit_subagent_spawn writes a parseable jsonl line
# ---------------------------------------------------------------------------

def test_emit_subagent_spawn_appends_valid_jsonl_line(tmp_path: Path) -> None:
    """AC1-HP: emit_subagent_spawn writes a valid subagent_spawn event to events.jsonl."""
    session_id = "test-session-abc123"
    nonce = "e71bc8540bcee49a"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    from fno.sigma_dispatch import emit_subagent_spawn

    emit_subagent_spawn(
        agent_name="code-reviewer",
        provider_id="claude-anthropic",
        cli="claude",
        repo_root=tmp_path,
    )

    events_file = tmp_path / ".fno" / "events.jsonl"
    assert events_file.exists(), "events.jsonl was not created"

    lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
    assert lines, "events.jsonl is empty"

    event = json.loads(lines[-1])
    assert event["type"] == "subagent_spawn", f"expected type=subagent_spawn, got: {event}"
    assert event["data"]["session_id"] == session_id
    assert event["data"]["nonce"] == nonce
    assert event["data"]["agent_name"] == "code-reviewer"
    assert event["data"]["provider_id"] == "claude-anthropic"
    assert event["data"]["cli"] == "claude"
    # Verify ISO-8601 Z timestamp
    ts = event["ts"]
    assert re.match(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", ts), f"bad timestamp: {ts}"


# ---------------------------------------------------------------------------
# AC1-HP: emit_subagent_complete writes a parseable jsonl line
# ---------------------------------------------------------------------------

def test_emit_subagent_complete_appends_valid_jsonl_line(tmp_path: Path) -> None:
    """AC1-HP: emit_subagent_complete writes a valid subagent_complete event to events.jsonl."""
    session_id = "test-session-def456"
    nonce = "a1b2c3d4e5f60001"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    from fno.sigma_dispatch import emit_subagent_complete

    emit_subagent_complete(
        agent_name="sigma-reviewer-backend",
        provider_id="gemini-backup",
        cli="gemini",
        exit_code=0,
        stdout_sha256="deadbeef" * 8,
        stderr_sha256="cafebabe" * 8,
        duration_ms=4200,
        repo_root=tmp_path,
    )

    events_file = tmp_path / ".fno" / "events.jsonl"
    assert events_file.exists(), "events.jsonl was not created"

    lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
    assert lines, "events.jsonl is empty"

    event = json.loads(lines[-1])
    assert event["type"] == "subagent_complete", f"expected type=subagent_complete, got: {event}"
    assert event["data"]["session_id"] == session_id
    assert event["data"]["nonce"] == nonce
    assert event["data"]["agent_name"] == "sigma-reviewer-backend"
    assert event["data"]["provider_id"] == "gemini-backup"
    assert event["data"]["cli"] == "gemini"
    # exit_code and duration_ms are passed as integers; jq parses them as JSON
    # numbers so they arrive as int in the decoded JSON (not strings).
    assert event["data"]["exit_code"] == 0
    assert event["data"]["stdout_sha256"] == "deadbeef" * 8
    assert event["data"]["stderr_sha256"] == "cafebabe" * 8
    assert event["data"]["duration_ms"] == 4200
    ts = event["ts"]
    assert re.match(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", ts), f"bad timestamp: {ts}"


# ---------------------------------------------------------------------------
# AC2-ERR: emitter failure raises EventEmitFailed
# ---------------------------------------------------------------------------

def test_emit_failure_raises_EventEmitFailed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC2-ERR: When emit-gate-transition.sh exits non-zero, EventEmitFailed is raised.

    Strategy: make .fno/events.jsonl a DIRECTORY so the append open()
    raises OSError. (The writer is native Python since ab-d0337fbc deleted
    the emit-gate-transition.sh shell-out; the error-propagation contract is
    what matters, not which substrate failed.)
    """
    session_id = "test-session-err789"
    nonce = "0000111122223333"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)
    # Force the append to fail: events.jsonl as a directory.
    (tmp_path / ".fno" / "events.jsonl").mkdir(parents=True, exist_ok=True)

    from fno.sigma_dispatch import EventEmitFailed, emit_subagent_spawn

    with pytest.raises(EventEmitFailed):
        emit_subagent_spawn(
            agent_name="code-reviewer",
            provider_id="claude-anthropic",
            cli="claude",
            repo_root=tmp_path,
        )


# ---------------------------------------------------------------------------
# Task 1.3: record_dispatch sidecar writer
# ---------------------------------------------------------------------------


def test_record_dispatch_appends_valid_jsonl_line(tmp_path: Path) -> None:
    """AC3-HP: record_dispatch appends a valid JSONL line with all six fields."""
    sidecar_path = tmp_path / ".fno" / "subagent-dispatch.jsonl"
    sidecar_path.parent.mkdir(parents=True, exist_ok=True)

    from fno.sigma_dispatch import record_dispatch

    record_dispatch(
        sidecar_path=sidecar_path,
        turn_index=1,
        ts="2026-05-05T17:30:00Z",
        agent_name="code-reviewer",
        provider_id="claude-anthropic",
        cli="claude",
        exit_code=0,
    )

    assert sidecar_path.exists(), "sidecar file was not created"
    lines = [ln for ln in sidecar_path.read_text(encoding="utf-8").splitlines() if ln.strip()]
    assert len(lines) == 1, f"expected 1 line, got {len(lines)}"

    record = json.loads(lines[0])
    assert record["turn_index"] == 1
    assert record["ts"] == "2026-05-05T17:30:00Z"
    assert record["agent_name"] == "code-reviewer"
    assert record["provider_id"] == "claude-anthropic"
    assert record["cli"] == "claude"
    assert record["exit_code"] == 0


def test_record_dispatch_creates_missing_parent_dir(tmp_path: Path) -> None:
    """AC3-HP: record_dispatch auto-creates parent dir if it doesn't exist."""
    sidecar_path = tmp_path / "nested" / ".fno" / "subagent-dispatch.jsonl"
    # Confirm parent does NOT exist
    assert not sidecar_path.parent.exists()

    from fno.sigma_dispatch import record_dispatch

    record_dispatch(
        sidecar_path=sidecar_path,
        turn_index=5,
        ts="2026-05-05T18:00:00Z",
        agent_name="sigma-reviewer",
        provider_id="gemini-backup",
        cli="gemini",
        exit_code=1,
    )

    assert sidecar_path.exists(), "sidecar file was not created"
    lines = [ln for ln in sidecar_path.read_text(encoding="utf-8").splitlines() if ln.strip()]
    assert len(lines) == 1
    record = json.loads(lines[0])
    assert record["turn_index"] == 5
    assert record["agent_name"] == "sigma-reviewer"


def test_record_dispatch_concurrent_appends_survive_race(tmp_path: Path) -> None:
    """AC3-EDGE: 10 threads x 100 calls produce exactly 1000 valid JSONL lines with no corruption."""
    sidecar_path = tmp_path / ".fno" / "subagent-dispatch.jsonl"
    sidecar_path.parent.mkdir(parents=True, exist_ok=True)

    from fno.sigma_dispatch import record_dispatch

    def write_batch(thread_index: int) -> None:
        base = thread_index * 100
        for i in range(100):
            record_dispatch(
                sidecar_path=sidecar_path,
                turn_index=base + i,
                ts="2026-05-05T19:00:00Z",
                agent_name=f"agent-{thread_index}",
                provider_id="claude-anthropic",
                cli="claude",
                exit_code=0,
            )

    with concurrent.futures.ThreadPoolExecutor(max_workers=10) as pool:
        futures = [pool.submit(write_batch, t) for t in range(10)]
        concurrent.futures.wait(futures)

    lines = [ln for ln in sidecar_path.read_text(encoding="utf-8").splitlines() if ln.strip()]
    assert len(lines) == 1000, f"expected 1000 lines, got {len(lines)}"

    seen_indices = set()
    for line in lines:
        record = json.loads(line)  # will raise if malformed
        seen_indices.add(record["turn_index"])

    assert seen_indices == set(range(1000)), "turn_index values are not exactly 0..999"


def test_record_dispatch_silent_on_unwritable_dir(
    tmp_path: Path, caplog: pytest.LogCaptureFixture, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC3-FR: record_dispatch logs WARNING and returns normally when sidecar write fails.

    Strategy: monkeypatch builtins.open inside the sigma_dispatch module to raise
    PermissionError when called with the sidecar path. This is more portable than
    chmod 0500 because macOS and Linux CI runners sometimes run as root and bypass
    filesystem permission checks. We patch builtins.open (via the module's global)
    rather than Path.open because record_dispatch uses the open() builtin, not the
    Path method.
    """
    import builtins
    import logging

    sidecar_path = tmp_path / ".fno" / "subagent-dispatch.jsonl"

    original_open = builtins.open

    def mock_open(file: object, *args: object, **kwargs: object) -> object:
        # Block opens to the sidecar path regardless of how the path was given
        if str(file) == str(sidecar_path):
            raise PermissionError("permission denied (monkeypatched)")
        return original_open(file, *args, **kwargs)  # type: ignore[call-overload]

    # Patch builtins.open globally; record_dispatch calls open() which resolves
    # through builtins, not Path.open. Monkeypatched back to original after test.
    monkeypatch.setattr(builtins, "open", mock_open)

    from fno.sigma_dispatch import record_dispatch

    with caplog.at_level(logging.WARNING, logger="fno.sigma_dispatch"):
        record_dispatch(
            sidecar_path=sidecar_path,
            turn_index=99,
            ts="2026-05-05T20:00:00Z",
            agent_name="code-reviewer",
            provider_id="claude-anthropic",
            cli="claude",
            exit_code=0,
        )

    # Must not raise - best-effort policy
    warning_records = [r for r in caplog.records if r.levelno >= logging.WARNING]
    assert warning_records, "expected at least one WARNING log record"
    assert any(
        "subagent-dispatch sidecar write failed" in r.message for r in warning_records
    ), f"expected warning about sidecar write failure, got: {[r.message for r in warning_records]}"


# ---------------------------------------------------------------------------
# Task 2.1: dispatch_sigma_subagent() - Claude Task asymmetric path
# ---------------------------------------------------------------------------


class TestDispatchClaudeTask:
    """Tests for the _DispatchClaudeTask context manager (Task 2.1, ab-978e93ed).

    The Claude path is structurally asymmetric: the dispatcher cannot wrap a
    subprocess because the Task tool is a Claude-internal call. The context
    manager emits subagent_spawn on __enter__ and guarantees subagent_complete
    on __exit__ regardless of whether record_complete was called.
    """

    def test_claude_path_emits_spawn_then_complete(self, tmp_path: Path) -> None:
        """AC4-HP: entering emits spawn; record_complete + exit emits complete with outcome=ok."""
        session_id = "test-session-hp-001"
        nonce = "c0ffee1234567890"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        events_file = tmp_path / ".fno" / "events.jsonl"

        with dispatch_sigma_subagent(
            agent_name="code-reviewer",
            provider_id="claude-anthropic",
            cli="claude",
            repo_root=tmp_path,
        ) as d:
            # On __enter__, spawn event must already be on disk.
            assert events_file.exists(), "events.jsonl missing after __enter__"
            lines_on_enter = [
                ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()
            ]
            assert lines_on_enter, "no events after __enter__"
            spawn_event = json.loads(lines_on_enter[-1])
            assert spawn_event["type"] == "subagent_spawn", f"wrong event type: {spawn_event}"

            # Caller records the Task result before exiting the block.
            d.record_complete(stdout="RESULT: SUCCESS\nfoo", exit_code=0)

        # After __exit__, paired complete event must be present.
        lines_after = [
            ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()
        ]
        assert len(lines_after) >= 2, "expected at least 2 events (spawn + complete)"

        complete_event = json.loads(lines_after[-1])
        assert complete_event["type"] == "subagent_complete", (
            f"last event is not subagent_complete: {complete_event}"
        )
        assert complete_event["data"].get("outcome") == "ok", (
            f"expected outcome=ok, got: {complete_event['data'].get('outcome')}"
        )
        assert complete_event["data"]["exit_code"] == 0

    def test_claude_path_skipped_finalizer_records_orchestrator_skipped(
        self, tmp_path: Path
    ) -> None:
        """AC4-EDGE: if record_complete is never called, __exit__ emits outcome=orchestrator_skipped."""
        session_id = "test-session-edge-002"
        nonce = "deadbeef00000001"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        events_file = tmp_path / ".fno" / "events.jsonl"

        with dispatch_sigma_subagent(
            agent_name="code-reviewer",
            provider_id="claude-anthropic",
            cli="claude",
            repo_root=tmp_path,
        ):
            pass  # deliberately do NOT call record_complete

        lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
        assert len(lines) >= 2, f"expected at least 2 events, got {len(lines)}"

        complete_event = json.loads(lines[-1])
        assert complete_event["type"] == "subagent_complete"
        assert complete_event["data"].get("outcome") == "orchestrator_skipped", (
            f"expected outcome=orchestrator_skipped, got: {complete_event['data'].get('outcome')}"
        )
        # exit_code must be the null sentinel when record_complete was skipped.
        # The shell helper (emit-gate-transition.sh) serializes the string "null"
        # via jq --arg (not --argjson) because jq -e treats null as falsy and
        # returns rc=1. So the field arrives as the string "null" rather than
        # JSON null. Both None and "null" are valid null sentinels here.
        exit_code_val = complete_event["data"].get("exit_code")
        assert exit_code_val is None or exit_code_val == "null", (
            f"expected exit_code null sentinel (None or 'null'), got: {exit_code_val!r}"
        )

    def test_claude_path_spawn_emit_failure_halts_before_record_complete(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC4-FR: if emit_subagent_spawn raises EventEmitFailed, __enter__ propagates it
        and no subagent_complete event is written (no spawn -> no complete)."""
        session_id = "test-session-fr-003"
        nonce = "badf00d100000001"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        from fno.sigma_dispatch import EventEmitFailed, dispatch_sigma_subagent

        # Force the native append to fail: events.jsonl as a directory
        # (the shell-out died with ab-d0337fbc; OSError is the failure path).
        events_file = tmp_path / ".fno" / "events.jsonl"
        events_file.mkdir(parents=True, exist_ok=True)

        with pytest.raises(EventEmitFailed):
            with dispatch_sigma_subagent(
                agent_name="code-reviewer",
                provider_id="claude-anthropic",
                cli="claude",
                repo_root=tmp_path,
            ) as d:
                d.record_complete(stdout="should not reach here", exit_code=0)

        # No complete event must exist (spawn failed so no paired complete).
        # events.jsonl is the failure-injection directory here, so a real
        # file at that path would itself be a bug; a dir contains no events.
        if events_file.is_file():
            complete_lines = [
                ln
                for ln in events_file.read_text(encoding="utf-8").splitlines()
                if ln.strip() and "subagent_complete" in ln
            ]
            assert not complete_lines, (
                f"subagent_complete should not be written when spawn failed, "
                f"got: {complete_lines}"
            )

    def test_claude_path_pair_carries_matching_nonce(self, tmp_path: Path) -> None:
        """AC4-HP: spawn and complete events carry the same provenance nonce."""
        session_id = "test-session-nonce-004"
        nonce = "e71bc8540bcee49a"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        events_file = tmp_path / ".fno" / "events.jsonl"

        with dispatch_sigma_subagent(
            agent_name="code-reviewer",
            provider_id="claude-anthropic",
            cli="claude",
            repo_root=tmp_path,
        ) as d:
            d.record_complete(stdout="ok", exit_code=0)

        lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
        assert len(lines) >= 2, f"expected spawn + complete, got {len(lines)} lines"

        spawn_nonce = json.loads(lines[-2])["data"]["nonce"]
        complete_nonce = json.loads(lines[-1])["data"]["nonce"]
        assert spawn_nonce == complete_nonce, (
            f"nonce mismatch: spawn={spawn_nonce!r} vs complete={complete_nonce!r}"
        )

    def test_dispatch_unknown_cli_raises_ValueError(
        self, tmp_path: Path
    ) -> None:
        """Task 2.2 adds gemini/codex/openclaw/hermes; unknown CLIs raise ValueError."""
        session_id = "test-session-stub-005"
        nonce = "1111222233334444"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        with pytest.raises(ValueError, match="unknown cli"):
            with dispatch_sigma_subagent(
                agent_name="silent-failure-hunter",
                provider_id="some-provider",
                cli="unknown-cli",
                repo_root=tmp_path,
            ):
                pass


# ---------------------------------------------------------------------------
# Task 2.2: dispatch_sigma_subagent() - non-Claude subprocess paths
# ---------------------------------------------------------------------------


class TestDispatchSubprocess:
    """Tests for _DispatchSubprocess (Task 2.2, ab-978e93ed).

    Covers gemini / codex / openclaw subprocess paths and the hermes stub.
    subprocess.run is monkeypatched so tests are deterministic and CI-safe
    (no requirement to have gemini/codex/openclaw on PATH).

    spawn_with_provider_snapshot is monkeypatched in fno.sigma_dispatch
    (the import site) rather than the original dispatch module so monkeypatch
    intercepts what _DispatchSubprocess actually calls.
    """

    def _make_fake_popen(
        self,
        *,
        returncode: int = 0,
        stdout: bytes = b"",
        stderr: bytes = b"",
    ) -> subprocess.Popen:
        """Return a minimal fake Popen that supports .communicate() and .returncode."""

        class _FakePopen:
            def __init__(self) -> None:
                self.returncode = returncode
                self._stdout = stdout
                self._stderr = stderr

            def communicate(self, timeout=None):
                return (self._stdout, self._stderr)

        return _FakePopen()  # type: ignore[return-value]

    def test_gemini_path_emits_paired_events(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC5-HP: happy-path on gemini -- spawn + complete events land with cli=gemini."""
        session_id = "20260505T100000Z-55500-aabbcc"
        nonce = "deadbeef11223344"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        import fno.sigma_dispatch as mod

        fake_popen = self._make_fake_popen(
            returncode=0, stdout=b"approved", stderr=b""
        )
        captured_calls: list[dict] = []

        def fake_spawn(cmd, *, settings_path=None, env=None, **kwargs):
            captured_calls.append({"cmd": cmd, "kwargs": kwargs})
            return fake_popen

        monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        with dispatch_sigma_subagent(
            agent_name="silent-failure-hunter",
            provider_id="gemini-pro-1",
            cli="gemini",
            prompt="review this diff",
            repo_root=tmp_path,
        ):
            pass  # subprocess path blocks inside __exit__

        events_file = tmp_path / ".fno" / "events.jsonl"
        assert events_file.exists(), "events.jsonl not created"
        lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
        assert len(lines) >= 2, f"expected spawn + complete, got {len(lines)} events"

        spawn_event = json.loads(lines[-2])
        assert spawn_event["type"] == "subagent_spawn"
        assert spawn_event["data"]["cli"] == "gemini"
        assert spawn_event["data"]["provider_id"] == "gemini-pro-1"

        complete_event = json.loads(lines[-1])
        assert complete_event["type"] == "subagent_complete"
        assert complete_event["data"]["exit_code"] == 0
        stdout_sha = complete_event["data"]["stdout_sha256"]
        assert len(stdout_sha) == 64, f"stdout_sha256 should be 64-char hex: {stdout_sha!r}"
        assert complete_event["data"]["duration_ms"] >= 0

    def test_codex_path_feeds_prompt_via_stdin(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC5-HP codex: prompt is fed via stdin (input kwarg), not a positional arg."""
        session_id = "20260505T100001Z-55501-bbccdd"
        nonce = "cafebabe11223344"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        import fno.sigma_dispatch as mod

        fake_popen = self._make_fake_popen(returncode=0, stdout=b"ok", stderr=b"")
        spawn_calls: list[dict] = []

        def fake_spawn(cmd, *, settings_path=None, env=None, **kwargs):
            spawn_calls.append({"cmd": cmd, "kwargs": kwargs})
            return fake_popen

        monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

        prompt_text = "check the codex diff"

        from fno.sigma_dispatch import dispatch_sigma_subagent

        with dispatch_sigma_subagent(
            agent_name="code-reviewer",
            provider_id="codex-v1",
            cli="codex",
            prompt=prompt_text,
            repo_root=tmp_path,
        ):
            pass

        # codex is spawned with argv=["codex"] and prompt via stdin kwarg
        assert spawn_calls, "spawn_with_provider_snapshot was not called"
        call = spawn_calls[0]
        assert call["cmd"] == ["codex"], f"expected ['codex'], got {call['cmd']!r}"
        # stdin bytes must encode the full prompt
        assert call["kwargs"].get("stdin") == prompt_text.encode("utf-8"), (
            f"expected stdin=prompt bytes, got {call['kwargs'].get('stdin')!r}"
        )

    def test_openclaw_path_uses_dash_p_flag(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC5-HP openclaw: spawn argv is ['openclaw', '-p', prompt]."""
        session_id = "20260505T100002Z-55502-ccdde"
        nonce = "babe00ff11223344"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        import fno.sigma_dispatch as mod

        fake_popen = self._make_fake_popen(returncode=0, stdout=b"pass", stderr=b"")
        spawn_calls: list[dict] = []

        def fake_spawn(cmd, *, settings_path=None, env=None, **kwargs):
            spawn_calls.append({"cmd": cmd})
            return fake_popen

        monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

        prompt_text = "openclaw review"

        from fno.sigma_dispatch import dispatch_sigma_subagent

        with dispatch_sigma_subagent(
            agent_name="silent-failure-hunter",
            provider_id="openclaw-v1",
            cli="openclaw",
            prompt=prompt_text,
            repo_root=tmp_path,
        ):
            pass

        assert spawn_calls, "spawn_with_provider_snapshot was not called"
        assert spawn_calls[0]["cmd"] == ["openclaw", "-p", prompt_text], (
            f"expected ['openclaw', '-p', prompt], got {spawn_calls[0]['cmd']!r}"
        )

    def test_subprocess_crash_still_emits_complete(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC5-FR: subprocess crash (rc=139 SIGSEGV) still emits subagent_complete with exit_code=139."""
        session_id = "20260505T100003Z-55503-ddeeff"
        nonce = "dead00de11223344"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        import fno.sigma_dispatch as mod

        fake_popen = self._make_fake_popen(
            returncode=139, stdout=b"partial out", stderr=b"Segmentation fault"
        )

        def fake_spawn(cmd, *, settings_path=None, env=None, **kwargs):
            return fake_popen

        monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        # Must not raise despite crash exit code
        with dispatch_sigma_subagent(
            agent_name="code-reviewer",
            provider_id="gemini-pro-1",
            cli="gemini",
            prompt="diff review",
            repo_root=tmp_path,
        ):
            pass

        events_file = tmp_path / ".fno" / "events.jsonl"
        lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
        complete_event = json.loads(lines[-1])
        assert complete_event["type"] == "subagent_complete"
        assert complete_event["data"]["exit_code"] == 139, (
            f"expected exit_code=139, got {complete_event['data']['exit_code']!r}"
        )
        # Fix 2 (panel HIGH): crash must emit outcome="error", not omit the field.
        # outcome=None (omitted) lets the verifier silently pass a crashed agent.
        assert complete_event["data"].get("outcome") == "error", (
            f"expected outcome='error' on crash, got: {complete_event['data'].get('outcome')!r}"
        )

    def test_snapshot_stickiness_under_concurrent_failover(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC5-EDGE (invariant 9): provider snapshot is captured once at __enter__.

        A failover swap that fires AFTER __enter__ must not corrupt the
        spawn event's provider_id. The spawn event carries the snapshot
        taken at entry, not any post-swap value.
        """
        session_id = "20260505T100004Z-55504-eeff00"
        nonce = "cafe1234deadbeef"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        import fno.sigma_dispatch as mod

        call_count = 0
        original_provider_id = "gemini-pro-original"

        def fake_spawn(cmd, *, settings_path=None, env=None, **kwargs):
            nonlocal call_count
            call_count += 1
            # Simulate: first call sees original; second call (if any) sees swapped.
            # _DispatchSubprocess must only call this ONCE (at __enter__).
            popen = _FakePopenForStickiness(call_count)
            return popen

        class _FakePopenForStickiness:
            def __init__(self, count: int) -> None:
                self.returncode = 0
                self._count = count

            def communicate(self, timeout=None):
                return (b"ok", b"")

        monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

        # Simulate: _DispatchSubprocess uses provider_id passed by the orchestrator
        # (captured at dispatch time), not re-read from settings at spawn time.
        # The spawn event's provider_id must match what was passed in.
        from fno.sigma_dispatch import dispatch_sigma_subagent

        with dispatch_sigma_subagent(
            agent_name="silent-failure-hunter",
            provider_id=original_provider_id,
            cli="gemini",
            prompt="review",
            repo_root=tmp_path,
        ):
            # Simulate: concurrent failover fires mid-block (after __enter__).
            # spawn_with_provider_snapshot is NOT called again here.
            pass

        # spawn_with_provider_snapshot must be called EXACTLY ONCE (at spawn time).
        assert call_count == 1, (
            f"spawn_with_provider_snapshot called {call_count} times; expected 1 "
            "(snapshot must be captured exactly once at __enter__)"
        )

        events_file = tmp_path / ".fno" / "events.jsonl"
        lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
        spawn_event = json.loads(lines[-2])
        assert spawn_event["type"] == "subagent_spawn"
        assert spawn_event["data"]["provider_id"] == original_provider_id, (
            f"spawn event carried {spawn_event['data']['provider_id']!r}, "
            f"expected {original_provider_id!r} (snapshot stickiness violated)"
        )

    def test_hermes_path_raises_not_implemented_yet(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Hermes adapter wiring is deferred to a follow-up task.

        _DispatchSubprocess recognizes cli='hermes' and routes to
        _delegate_via_hermes(), which raises NotImplementedError until
        the hermes adapter is wired in this repo.

        Intended future behavior (not yet implemented):
          - _delegate_via_hermes(prompt, snapshot, timeout_ms=1800000) is called.
          - On success: emits subagent_complete with exit_code from hermes response.
          - On timeout: emits subagent_complete with exit_code=124 (timeout sentinel).
        """
        session_id = "20260505T100005Z-55505-ff0011"
        nonce = "f00df00d11223344"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        with pytest.raises(NotImplementedError, match="hermes"):
            with dispatch_sigma_subagent(
                agent_name="silent-failure-hunter",
                provider_id="hermes-1",
                cli="hermes",
                prompt="review diff",
                repo_root=tmp_path,
            ):
                pass


# ---------------------------------------------------------------------------
# Task 2b.1: _capture_stdout per-agent sidecar writer
# ---------------------------------------------------------------------------


class TestCaptureStdout:
    """Tests for _capture_stdout (Task 2b.1, ab-978e93ed).

    _capture_stdout writes per-agent .out/.err files at:
      .fno/sigma-review/{session_id}/{agent_name}.{out,err}

    Best-effort policy: OSError/PermissionError logs WARNING and returns.
    Session-scoped: same (session_id, agent_name) pair appends with separator.
    NOT gate-side: Wave 3.1's verifier reads events.jsonl exclusively.
    """

    def test_capture_creates_both_out_and_err(self, tmp_path: Path) -> None:
        """AC6-HP: creates .out and .err files with the correct content."""
        from fno.sigma_dispatch import _capture_stdout

        session_id = "20260505T110000Z-66600-aabbcc"
        agent_name = "code-reviewer"

        _capture_stdout(
            session_id=session_id,
            agent_name=agent_name,
            stdout="approved",
            stderr="warning: foo",
            repo_root=tmp_path,
        )

        base = tmp_path / ".fno" / "sigma-review" / session_id
        out_file = base / f"{agent_name}.out"
        err_file = base / f"{agent_name}.err"

        assert out_file.exists(), ".out file was not created"
        assert err_file.exists(), ".err file was not created"
        assert out_file.read_text(encoding="utf-8") == "approved"
        assert err_file.read_text(encoding="utf-8") == "warning: foo"

    def test_capture_creates_missing_parent_dir(self, tmp_path: Path) -> None:
        """AC6-HP: auto-creates parent directories when .fno/sigma-review/... doesn't exist."""
        from fno.sigma_dispatch import _capture_stdout

        # Confirm no .fno dir exists yet under tmp_path
        assert not (tmp_path / ".fno").exists()

        _capture_stdout(
            session_id="20260505T110001Z-66601-bbccdd",
            agent_name="silent-failure-hunter",
            stdout="findings",
            stderr="",
            repo_root=tmp_path,
        )

        base = tmp_path / ".fno" / "sigma-review" / "20260505T110001Z-66601-bbccdd"
        assert (base / "silent-failure-hunter.out").exists()
        assert (base / "silent-failure-hunter.err").exists()

    def test_capture_appends_with_separator_on_second_dispatch(self, tmp_path: Path) -> None:
        """AC6-EDGE: second call to same (session_id, agent_name) appends with separator."""
        from fno.sigma_dispatch import _capture_stdout

        session_id = "20260505T110002Z-66602-ccdde"
        agent_name = "code-reviewer"

        _capture_stdout(
            session_id=session_id,
            agent_name=agent_name,
            stdout="run 1",
            stderr="",
            repo_root=tmp_path,
        )
        _capture_stdout(
            session_id=session_id,
            agent_name=agent_name,
            stdout="run 2",
            stderr="",
            repo_root=tmp_path,
        )

        out_content = (
            tmp_path / ".fno" / "sigma-review" / session_id / f"{agent_name}.out"
        ).read_text(encoding="utf-8")

        assert "run 1" in out_content, "first dispatch content missing"
        assert "run 2" in out_content, "second dispatch content missing"
        assert "--- dispatch at 2026" in out_content, (
            f"separator line not found in: {out_content!r}"
        )

    def test_capture_silent_on_unwritable_dir(
        self, tmp_path: Path, caplog: pytest.LogCaptureFixture, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC6-FR: logs WARNING and returns normally when write fails (best-effort policy).

        Strategy: monkeypatch builtins.open inside the sigma_dispatch module to raise
        PermissionError when called on the target .out/.err paths. More portable than
        chmod 0500 because macOS/Linux CI may run as root and bypass fs permission checks.
        """
        import builtins
        import logging

        from fno.sigma_dispatch import _capture_stdout

        session_id = "20260505T110003Z-66603-ddeeff"
        agent_name = "code-reviewer"
        base = tmp_path / ".fno" / "sigma-review" / session_id
        base.mkdir(parents=True, exist_ok=True)

        original_open = builtins.open

        def mock_open(file: object, *args: object, **kwargs: object) -> object:
            file_str = str(file)
            if f"{agent_name}.out" in file_str or f"{agent_name}.err" in file_str:
                raise PermissionError("permission denied (monkeypatched)")
            return original_open(file, *args, **kwargs)  # type: ignore[call-overload]  # type: ignore[call-overload]

        monkeypatch.setattr(builtins, "open", mock_open)

        with caplog.at_level(logging.WARNING, logger="fno.sigma_dispatch"):
            _capture_stdout(
                session_id=session_id,
                agent_name=agent_name,
                stdout="some output",
                stderr="some error",
                repo_root=tmp_path,
            )

        # Must not raise
        warning_records = [r for r in caplog.records if r.levelno >= logging.WARNING]
        assert warning_records, "expected at least one WARNING log record"
        assert any(
            "sigma-review stdout capture failed" in r.message for r in warning_records
        ), f"expected warning about capture failure, got: {[r.message for r in warning_records]}"

    def test_claude_path_calls_capture_stdout_in_record_complete(
        self, tmp_path: Path
    ) -> None:
        """Wiring: _DispatchClaudeTask.record_complete calls _capture_stdout.

        After record_complete is called inside the `with` block, the sidecar
        .out file must exist with the captured stdout text before __exit__ runs.
        Proves the helper is wired into the Claude path.
        """
        session_id = "20260505T110004Z-66604-eeff00"
        nonce = "c0ffeecafe000001"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        with dispatch_sigma_subagent(
            agent_name="code-reviewer",
            provider_id="claude-anthropic",
            cli="claude",
            repo_root=tmp_path,
        ) as d:
            d.record_complete(stdout="RESULT: SUCCESS\nsome output", exit_code=0)
            # Capture happens inside record_complete, so .out must exist NOW.
            out_file = (
                tmp_path
                / ".fno"
                / "sigma-review"
                / session_id
                / "code-reviewer.out"
            )
            assert out_file.exists(), ".out file missing after record_complete"
            assert "RESULT: SUCCESS" in out_file.read_text(encoding="utf-8")

    def test_subprocess_path_calls_capture_stdout_after_wait(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Wiring: _DispatchSubprocess.__exit__ calls _capture_stdout after Popen.communicate().

        After the `with` block exits, the sidecar .out file must exist with the
        decoded subprocess stdout bytes. Proves the helper is wired into the
        subprocess path.
        """
        session_id = "20260505T110005Z-66605-ff0011"
        nonce = "deadbeefcafe0002"
        _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

        import fno.sigma_dispatch as mod

        fake_popen_instance = None

        class _FakePopen:
            returncode = 0

            def communicate(self, timeout=None):
                return (b"gemini approved output", b"gemini stderr")

        def fake_spawn(cmd, *, settings_path=None, env=None, **kwargs):
            nonlocal fake_popen_instance
            fake_popen_instance = _FakePopen()
            return fake_popen_instance

        monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

        from fno.sigma_dispatch import dispatch_sigma_subagent

        with dispatch_sigma_subagent(
            agent_name="silent-failure-hunter",
            provider_id="gemini-pro-1",
            cli="gemini",
            prompt="review this diff",
            repo_root=tmp_path,
        ):
            pass

        out_file = (
            tmp_path
            / ".fno"
            / "sigma-review"
            / session_id
            / "silent-failure-hunter.out"
        )
        assert out_file.exists(), ".out file missing after subprocess dispatch"
        content = out_file.read_text(encoding="utf-8")
        assert "gemini approved output" in content, (
            f"expected subprocess stdout in .out file, got: {content!r}"
        )


# ---------------------------------------------------------------------------
# Fix 1 (panel HIGH): _capture_stdout concurrent lock correctness
# ---------------------------------------------------------------------------


def test_subprocess_communicate_timeout_emits_complete_with_124(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 6 (panel MEDIUM-HIGH): _DispatchSubprocess.__exit__ must pass timeout=
    to communicate(). On TimeoutExpired, it must kill the process, emit a complete
    event with exit_code=124 and outcome='timeout', then return normally.

    Before the fix: communicate() had no timeout parameter and could block forever.
    After the fix: timeout defaults to 1800s; TimeoutExpired is caught, process
    killed, complete event emitted with exit_code=124 and outcome='timeout'.
    """
    session_id = "fix6-timeout-test-session"
    nonce = "aabbccdd11223344"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    import fno.sigma_dispatch as mod

    class _HangingPopen:
        returncode = None  # not set until killed
        _killed = False

        def communicate(self, timeout=None):
            # Simulate a timeout: raise TimeoutExpired immediately.
            import subprocess as _sp
            raise _sp.TimeoutExpired(cmd=["gemini", "-p", "review"], timeout=timeout)

        def kill(self) -> None:
            self._killed = True
            self.returncode = -9  # killed

        def communicate_after_kill(self, timeout=None):
            # Second communicate() after kill() returns empty bytes.
            return (b"", b"")

    fake_popen = _HangingPopen()
    # After kill(), communicate() should return empty to avoid infinite loop.
    call_count = [0]

    original_communicate = fake_popen.communicate

    def patched_communicate(timeout=None):
        call_count[0] += 1
        if call_count[0] == 1:
            return original_communicate(timeout=timeout)
        # Second call (after kill) returns empty.
        return (b"", b"")

    fake_popen.communicate = patched_communicate  # type: ignore[method-assign]

    def fake_spawn(cmd, *, settings_path=None, env=None, **kwargs):
        return fake_popen

    monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

    from fno.sigma_dispatch import dispatch_sigma_subagent

    # Must not raise or hang.
    with dispatch_sigma_subagent(
        agent_name="code-reviewer",
        provider_id="gemini-pro-1",
        cli="gemini",
        prompt="review this diff",
        repo_root=tmp_path,
    ):
        pass

    events_file = tmp_path / ".fno" / "events.jsonl"
    assert events_file.exists(), "events.jsonl not created"
    lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
    complete_event = json.loads(lines[-1])

    assert complete_event["type"] == "subagent_complete"
    assert complete_event["data"]["exit_code"] == 124, (
        f"expected exit_code=124 (timeout convention), got: {complete_event['data']['exit_code']!r}"
    )
    assert complete_event["data"].get("outcome") == "timeout", (
        f"expected outcome='timeout', got: {complete_event['data'].get('outcome')!r}"
    )


def test_record_complete_handles_non_str_stdout(tmp_path: Path) -> None:
    """Fix 5 (panel MEDIUM): record_complete must not raise AttributeError when
    stdout is None or a non-str type. Before the fix, None.encode('utf-8') raised
    AttributeError before _completed=True was set, so __exit__ emitted
    outcome='orchestrator_skipped' masking the real error.

    After the fix: None is coerced to "" before hashing. The resulting complete
    event must have stdout_sha256 equal to the SHA-256 of an empty string.
    """
    import hashlib

    session_id = "fix5-non-str-stdout-session"
    nonce = "1122334455667788"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    EMPTY_SHA = hashlib.sha256(b"").hexdigest()  # e3b0c44...

    from fno.sigma_dispatch import dispatch_sigma_subagent

    events_file = tmp_path / ".fno" / "events.jsonl"

    # Must not raise; None stdout must be coerced to empty string.
    with dispatch_sigma_subagent(
        agent_name="code-reviewer",
        provider_id="claude-anthropic",
        cli="claude",
        repo_root=tmp_path,
    ) as d:
        d.record_complete(stdout=None, exit_code=0)  # type: ignore[arg-type]

    lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
    complete_event = json.loads(lines[-1])
    assert complete_event["type"] == "subagent_complete"
    assert complete_event["data"].get("outcome") == "ok", (
        f"expected outcome=ok (record_complete was called), got: {complete_event['data'].get('outcome')!r}"
    )
    assert complete_event["data"]["stdout_sha256"] == EMPTY_SHA, (
        f"expected empty-string SHA for None stdout, got: {complete_event['data']['stdout_sha256']!r}"
    )


def test_emit_subagent_spawn_no_override_fields_in_event(tmp_path: Path) -> None:
    """Fix 3 (panel HIGH): session_id_override and provenance_nonce_override must NOT
    appear as fields in the emitted events.jsonl payload.

    These parameters were passed as KEY=VALUE positional args to emit-gate-transition.sh
    which reads session_id and nonce exclusively from target-state.md and ignores them.
    The dead fields leaked into the JSON data payload as extra keys with no effect.

    After the fix: the emitter passes no override fields; events.jsonl carries
    session_id and nonce from state file resolution only.
    """
    session_id = "fix3-no-override-test-session"
    nonce = "aabb1122ccdd3344"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    from fno.sigma_dispatch import emit_subagent_spawn

    emit_subagent_spawn(
        agent_name="code-reviewer",
        provider_id="claude-anthropic",
        cli="claude",
        repo_root=tmp_path,
    )

    events_file = tmp_path / ".fno" / "events.jsonl"
    lines = [ln for ln in events_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
    assert lines, "events.jsonl is empty"

    event = json.loads(lines[-1])
    data = event["data"]
    assert "session_id_override" not in data, (
        f"session_id_override must not leak into event payload, got data={data}"
    )
    assert "provenance_nonce_override" not in data, (
        f"provenance_nonce_override must not leak into event payload, got data={data}"
    )
    # The real session_id and nonce must still be present (from state file).
    assert data.get("session_id") == session_id, (
        f"session_id missing or wrong: {data.get('session_id')!r}"
    )
    assert data.get("nonce") == nonce, (
        f"nonce missing or wrong: {data.get('nonce')!r}"
    )


def test_capture_stdout_concurrent_same_agent(tmp_path: Path) -> None:
    """Fix 1 (panel HIGH): concurrent _capture_stdout calls for same (session_id, agent_name)
    must NOT produce torn/interleaved bytes. Each written string must appear as
    a complete unit. Both threads write 100 distinct strings; all 200 must be
    recoverable line-by-line after both threads finish.

    Before the fix: naive open-append with no lock -> byte interleaving possible.
    After the fix: fcntl.LOCK_EX serializes appends, same as record_dispatch.
    """
    import concurrent.futures

    from fno.sigma_dispatch import _capture_stdout

    session_id = "fix1-concurrent-test-session"
    agent_name = "code-reviewer"

    strings_a = [f"thread-A-line-{i:03d}\n" for i in range(100)]
    strings_b = [f"thread-B-line-{i:03d}\n" for i in range(100)]

    def write_batch(strings: list[str]) -> None:
        for s in strings:
            _capture_stdout(
                session_id=session_id,
                agent_name=agent_name,
                stdout=s,
                stderr="",
                repo_root=tmp_path,
            )

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        fa = pool.submit(write_batch, strings_a)
        fb = pool.submit(write_batch, strings_b)
        concurrent.futures.wait([fa, fb])
        fa.result()  # propagate any exception
        fb.result()

    out_file = tmp_path / ".fno" / "sigma-review" / session_id / f"{agent_name}.out"
    assert out_file.exists(), ".out file was not created"
    content = out_file.read_text(encoding="utf-8")

    # Each of the 200 distinct strings must be fully present (no torn writes).
    all_expected = strings_a + strings_b
    for expected in all_expected:
        assert expected.strip() in content, (
            f"string {expected!r} missing from captured output; "
            "likely indicates byte interleaving from missing fcntl lock"
        )


# ---------------------------------------------------------------------------
# CG8 (Plan B, ab-0e5a921e): resolve_dispatch_target precedence chain
# ---------------------------------------------------------------------------

def _write_settings_combo_test(
    tmp_path: Path,
    *,
    active_provider: str | None = "a",
    active_combo: str | None = None,
    combos: dict | None = None,
    agents: dict | None = None,
):
    """Helper: write a settings.yaml with three providers + optional combos/agents."""
    import yaml
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    providers_block: dict = {
        "active": active_provider,
        "records": [
            {"id": "a", "name": "A", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
            {"id": "b", "name": "B", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
            {"id": "c", "name": "C", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
        ],
    }
    if active_combo is not None:
        providers_block["active_combo"] = active_combo
    if combos:
        providers_block["combos"] = combos
    payload: dict = {"config": {"providers": providers_block}}
    if agents:
        payload["config"]["agents"] = agents
    settings.write_text(yaml.safe_dump(payload, sort_keys=False), encoding="utf-8")
    return tmp_path


class TestResolveDispatchTargetPrecedence:
    def test_per_agent_pin_wins_over_env_combo(self, tmp_path: Path):
        """AC8.2-EDGE: per-agent pin used even when TARGET_COMBO is set."""
        from fno.sigma_dispatch import resolve_dispatch_target

        _write_settings_combo_test(
            tmp_path,
            combos={"my-stack": {"providers": ["a", "b"]}},
            agents={"reviewer": {"provider": "c"}},
        )
        target = resolve_dispatch_target(
            "reviewer",
            repo_root=tmp_path,
            env={"TARGET_COMBO": "my-stack"},
        )
        assert target.provider_id == "c"
        assert target.combo_name is None
        assert target.source == "per_agent_pin"

    def test_env_combo_used_when_no_per_agent_pin(self, tmp_path: Path):
        """AC8.1-HP: combo wins over fall-through when no per-agent pin."""
        from fno.sigma_dispatch import resolve_dispatch_target

        _write_settings_combo_test(
            tmp_path,
            combos={"my-stack": {"providers": ["a", "b"]}},
        )
        target = resolve_dispatch_target(
            "any-agent",
            repo_root=tmp_path,
            env={"TARGET_COMBO": "my-stack"},
        )
        assert target.combo_name == "my-stack"
        assert target.provider_id is None
        assert target.source == "env_combo"

    def test_unknown_env_combo_falls_through_with_warning(self, tmp_path: Path):
        """AC8.3-FR: bad TARGET_COMBO logs warning + falls to active provider."""
        from fno.sigma_dispatch import resolve_dispatch_target

        _write_settings_combo_test(tmp_path, combos={})
        target = resolve_dispatch_target(
            "any-agent",
            repo_root=tmp_path,
            env={"TARGET_COMBO": "deleted-stack"},
        )
        assert target.provider_id == "a"
        assert target.combo_name is None
        assert target.source == "active_provider"

    def test_settings_active_combo_used_when_no_env(self, tmp_path: Path):
        from fno.sigma_dispatch import resolve_dispatch_target

        _write_settings_combo_test(
            tmp_path,
            active_combo="my-stack",
            combos={"my-stack": {"providers": ["a", "b"]}},
        )
        target = resolve_dispatch_target(
            "any-agent",
            repo_root=tmp_path,
            env={},
        )
        assert target.combo_name == "my-stack"
        assert target.source == "settings_combo"

    def test_no_combo_no_pin_returns_active_provider(self, tmp_path: Path):
        from fno.sigma_dispatch import resolve_dispatch_target

        _write_settings_combo_test(tmp_path)
        target = resolve_dispatch_target(
            "any-agent", repo_root=tmp_path, env={},
        )
        assert target.provider_id == "a"
        assert target.source == "active_provider"

    def test_no_settings_file_returns_unresolved(self, tmp_path: Path):
        from fno.sigma_dispatch import resolve_dispatch_target

        target = resolve_dispatch_target(
            "any-agent", repo_root=tmp_path, env={},
        )
        assert target.provider_id is None
        assert target.combo_name is None
        assert target.source == "unresolved"

    def test_global_active_combo_falls_back_when_no_project_override(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """PR #230 Gemini MEDIUM #2: active_combo from ~/.fno/ should
        be read when project-local settings.yaml lacks the field."""
        import yaml as _yaml
        from fno.sigma_dispatch import resolve_dispatch_target

        # The autouse conftest pins FNO_GLOBAL_SETTINGS_PATH=/dev/null for
        # test isolation. This test specifically exercises the HOME-based
        # global fallback path, so opt out of the pin to restore the default
        # Path.home() resolution behavior.
        monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH", raising=False)

        # Project-local settings: providers + combos defined, but no active_combo.
        project = tmp_path / "project"
        _write_settings_combo_test(
            project,
            combos={"global-stack": {"providers": ["a", "b"]}},
        )

        # Global home: declares active_combo: global-stack only.
        home = tmp_path / "home"
        global_settings = home / ".fno" / "settings.yaml"
        global_settings.parent.mkdir(parents=True, exist_ok=True)
        global_settings.write_text(
            _yaml.safe_dump({
                "config": {
                    "providers": {
                        "active_combo": "global-stack",
                        "records": [
                            {"id": "a", "name": "A", "cli": "claude",
                             "auth": "oauth_dir", "credentials_source": "~/.claude"},
                        ],
                    }
                }
            }, sort_keys=False),
            encoding="utf-8",
        )
        monkeypatch.setenv("HOME", str(home))

        target = resolve_dispatch_target(
            "any-agent", repo_root=project, env={},
        )
        assert target.combo_name == "global-stack"
        assert target.source == "settings_combo"
