"""A revival fork must prove it came up before the verb reports one.

The reported failure: a ``spawn --resume`` fork of a claude thread worker
returned a session id while the new session never wrote a transcript line and
its job state read ``blocked``. The dispatcher then waited on a worker that
did not exist while the slot was spent and mail queued to nothing.

The poll itself lives in the fno-agents binary (``revive-proof``, with its
own Rust tests); this file covers the Python side of the contract - the
verdict branch, the lock-free stop, the refusal naming the state, and the
journey through the real spawn wiring.

Coverage:
  - refuse: an ``ok: false`` verdict stops the fork (claude_stop, lock-free
    because the caller holds the per-agent flock) and raises naming the
    observed state, exit 1.
  - pass: an ``ok: true`` verdict returns; the binary printed the verified
    transcript line itself.
  - a failed stop is named in the refusal, never swallowed.
  - a missing binary stands the gate down, keeping today's behavior.
  - journey: ``spawn --resume`` exits non-zero through the real wiring.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.agents.dispatch import DispatchAskError
from fno.agents.harnesses import _claude_session_registry as reg
from fno.agents.harnesses._claude_session_registry import (
    revive_proof_or_refuse as REAL_GATE,
)

SOURCE_UUID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

REFUSED = {
    "ok": False,
    "stopped": True,
    "reason": "transcript absent, job state blocked after 60s; the fork was stopped",
}
PROVEN = {"ok": True, "transcript": "/tmp/x/sess-1.jsonl", "job_state": "idle"}


def _arm(monkeypatch, verdict):
    """Fake the binary verdict (the op owns the poll and the stop)."""
    monkeypatch.setattr(reg, "_revive_proof_verdict", lambda _sid: verdict)


# ---------------------------------------------------------------------------
# Unit: the verdict branch
# ---------------------------------------------------------------------------


def test_refuses_on_a_false_verdict(monkeypatch) -> None:
    _arm(monkeypatch, REFUSED)
    with pytest.raises(DispatchAskError) as ei:
        REAL_GATE("rev-agent", "deadbeef")
    msg = str(ei.value)
    assert ei.value.exit_code == 1
    assert "never came up" in msg
    assert "transcript absent" in msg
    assert "fresh worker" in msg


def test_a_failed_stop_rides_the_reason(monkeypatch) -> None:
    verdict = dict(REFUSED, stopped=False, reason="transcript absent; the stop failed")
    _arm(monkeypatch, verdict)
    with pytest.raises(DispatchAskError) as ei:
        REAL_GATE("rev-agent", "deadbeef")
    assert "the stop failed" in str(ei.value)


def test_passes_on_a_true_verdict(monkeypatch) -> None:
    _arm(monkeypatch, PROVEN)
    REAL_GATE("rev-agent", "deadbeef")  # returns; the binary printed the line


def test_a_missing_binary_stands_the_gate_down(monkeypatch) -> None:
    """No fno-agents binary: keep today's behavior instead of refusing every
    revival on a degraded install."""
    monkeypatch.setattr("fno.rust_binary.find_dev_binary", lambda: None)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)
    REAL_GATE("rev-agent", "deadbeef")  # stands down, does not raise


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
                # The hermetic spawn-axes seam refuses a rowless unpinned
                # resume, so the row carries the model the revival would ride.
                requested_model="claude-opus-5",
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
    # Re-arm the real gate the conftest auto-neuter stood down; the binary's
    # poll is faked to the refused verdict. The re-arm must land on the call
    # site's binding: harnesses/claude.py imports the name directly, so the
    # registry module's own attribute is not what the spawn path calls.
    monkeypatch.setattr(claude_mod, "revive_proof_or_refuse", REAL_GATE)
    monkeypatch.setattr(reg, "_revive_proof_verdict", lambda _sid: REFUSED)

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "rev-agent", "-H", "claude", "--resume", SOURCE_UUID,
         "--substrate", "bg", "hi"],
        catch_exceptions=False,
    )
    assert result.exit_code == 1, result.output
    assert "never came up" in result.output
