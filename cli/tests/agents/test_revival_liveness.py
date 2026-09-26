"""x-eb64: a revival fork must prove it came up before the verb reports one.

The reported failure: a ``spawn --resume`` fork of a claude thread worker
returned a session id while the new session never wrote a transcript line and
its job state read ``blocked``. The dispatcher then waited on a worker that
did not exist while the slot was spent and mail queued to nothing.

Coverage:
  - refuse: no transcript inside the window -> DispatchAskError naming the
    observed transcript/state, the fork stopped (claims released), exit 1.
  - refuse: transcript present but the job state stays wedged.
  - pass: transcript plus a live state -> returns, stderr names the path.
  - pass: a wedged state that recovers inside the window.
  - the stop itself failing is named in the refusal, never swallowed.
  - journey: ``spawn --resume`` exits non-zero through the real wiring.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.agents.dispatch import DispatchAskError
from fno.agents.harnesses import _claude_session_registry as reg
from fno.agents.harnesses._claude_session_registry import (
    StateSnapshot,
    revive_proof_or_refuse as REAL_GATE,
)

SOURCE_UUID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"


@pytest.fixture(autouse=True)
def _fast_window(monkeypatch):
    """Retime the gate and silence its sleep; readers are faked per test."""
    monkeypatch.setattr(reg, "FORK_LIVENESS_WINDOW_S", 0.3)
    monkeypatch.setattr(reg, "FORK_LIVENESS_POLL_S", 0.05)
    monkeypatch.setattr(reg.time, "sleep", lambda _s: None)


def _arm(monkeypatch, *, uuid=SOURCE_UUID, transcript=None, state="blocked", stop=None):
    """Fake the gate's readers. A callable reader gets the tick number
    (counted by the state read, which the gate polls once per tick) and may
    vary its answer, e.g. ``transcript=lambda n: path if n >= 1 else None``.
    ``state=None`` reads as state.json being unreadable (OSError)."""
    tick = {"n": 0}

    def _by_tick(value):
        return value(tick["n"]) if callable(value) else value

    def _state_read(*_a, **_k):
        answer = _by_tick(state)
        tick["n"] += 1
        if answer is None:
            raise OSError("no state.json")
        return StateSnapshot(state=answer, updated_at=None, output_result=None)

    monkeypatch.setattr(reg, "resolve_session_uuid", lambda _sid: _by_tick(uuid))
    monkeypatch.setattr(reg, "_fork_transcript", lambda _u: _by_tick(transcript))
    monkeypatch.setattr(reg, "read_state_json", _state_read)

    stops = {"n": 0}

    def _stop(name, **_k):
        stops["n"] += 1
        if stop is not None:
            raise stop

    monkeypatch.setattr("fno.agents.stop_release.stop_agent", _stop)
    return stops


# ---------------------------------------------------------------------------
# Unit: the gate
# ---------------------------------------------------------------------------


def test_refuses_a_fork_that_never_wrote_a_transcript(tmp_path, monkeypatch) -> None:
    stops = _arm(monkeypatch)
    with pytest.raises(DispatchAskError) as ei:
        REAL_GATE("rev-agent", "deadbeef")
    msg = str(ei.value)
    assert ei.value.exit_code == 1
    assert "never came up" in msg
    assert "transcript absent" in msg
    assert "'blocked'" in msg
    assert "spawn a fresh worker" in msg
    assert stops == {"n": 1}


def test_refuses_when_state_stays_wedged_despite_a_transcript(
    tmp_path, monkeypatch
) -> None:
    transcript = tmp_path / "fork.jsonl"
    _arm(monkeypatch, transcript=transcript)
    with pytest.raises(DispatchAskError) as ei:
        REAL_GATE("rev-agent", "deadbeef")
    assert "transcript present" in str(ei.value)


def test_a_stopped_job_state_is_wedged(tmp_path, monkeypatch) -> None:
    transcript = tmp_path / "fork.jsonl"
    _arm(monkeypatch, transcript=transcript, state="stopped")
    with pytest.raises(DispatchAskError):
        REAL_GATE("rev-agent", "deadbeef")


def test_an_unreadable_job_state_with_a_transcript_passes(
    tmp_path, monkeypatch
) -> None:
    """The transcript is the load-bearing proof: a session streaming lines is
    up even when its state snapshot cannot be read."""
    transcript = tmp_path / "fork.jsonl"
    _arm(monkeypatch, transcript=transcript, state=None)
    REAL_GATE("rev-agent", "deadbeef")


def test_passes_once_transcript_and_live_state_arrive(
    tmp_path, monkeypatch, capsys
) -> None:
    transcript = tmp_path / "fork.jsonl"
    _arm(
        monkeypatch,
        transcript=lambda n: transcript if n >= 1 else None,
        state=lambda n: "idle" if n >= 1 else "blocked",
    )
    REAL_GATE("rev-agent", "deadbeef")  # returns, does not raise
    err = capsys.readouterr().err
    assert "revival liveness verified for deadbeef" in err
    assert str(transcript) in err


def test_an_unresolvable_session_id_refuses(monkeypatch) -> None:
    _arm(monkeypatch, uuid=None, state=None)
    with pytest.raises(DispatchAskError) as ei:
        REAL_GATE("rev-agent", "deadbeef")
    assert "transcript absent" in str(ei.value)


def test_a_failed_stop_is_named_not_swallowed(monkeypatch) -> None:
    _arm(monkeypatch, uuid=None, state=None, stop=RuntimeError("boom"))
    with pytest.raises(DispatchAskError) as ei:
        REAL_GATE("rev-agent", "deadbeef")
    msg = str(ei.value)
    assert "stop failed (boom)" in msg
    assert "still holds its slot" in msg


# ---------------------------------------------------------------------------
# Journey: the spawn verb refuses through the real wiring
# ---------------------------------------------------------------------------


@pytest.fixture
def workdir_claude(tmp_path, monkeypatch):
    """Isolated fno home with the fake claude on PATH (emits short_id 7c5dcf5d)."""
    from fno.paths_testing import use_tmpdir
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


def _seed_row(name: str, short_id: str, uuid) -> None:
    from fno.agents.registry import AgentEntry, update_registry

    update_registry(
        lambda entries: entries
        + [
            AgentEntry(
                name=name,
                harness="claude",
                cwd="/tmp",
                log_path="/tmp/rev.log",
                short_id=short_id,
                harness_session_id=uuid,
            )
        ]
    )


def test_spawn_resume_exits_non_zero_when_the_fork_never_comes_up(
    workdir_claude, monkeypatch
) -> None:
    """The reported shape, end to end: the revival door returns a session id,
    the fork writes nothing, and the verb must refuse instead of succeeding."""
    from fno.agents.cli import agents_app
    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: False)
    _seed_row("rev-agent", "deadbeef", SOURCE_UUID)
    # Re-arm the real gate the conftest auto-neuter stood down, then fake its
    # readers: the session record never appears, so proof never does.
    monkeypatch.setattr(reg, "revive_proof_or_refuse", REAL_GATE)
    monkeypatch.setattr(reg, "resolve_session_uuid", lambda _sid: None)
    monkeypatch.setattr("fno.agents.stop_release.stop_agent", lambda name, **k: None)

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "rev-agent", "-H", "claude", "--resume", SOURCE_UUID,
         "--substrate", "bg", "hi"],
        catch_exceptions=False,
    )
    assert result.exit_code == 1
    assert "never came up" in result.output
