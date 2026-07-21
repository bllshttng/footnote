"""x-9844 Fix 3: `spawn --resume` revives an exited same-name claude row in place.

The name-collision check stops refusing the one case that is a revival: an
exited claude row whose own recorded ``claude_session_uuid`` equals the
``--resume`` target. That row is updated in place (new short_id, same uuid)
instead of refused or duplicated. Everything else stays fail-closed.

Coverage:
  - ``_is_revival`` gate: dead+uuid-match -> True; live / mismatch / no-resume /
    non-claude -> False. Liveness is the reality probe, never the status field.
  - AC1-HP: spawn --resume against an exited own-name row updates it in place
    (one row, new short_id, same uuid) instead of exiting 2.
  - AC2-EDGE: a uuid mismatch stays a collision (exit 2, row unchanged).
  - AC2-HP: a same-name spawn with no --resume stays a collision.

x-7fef extends this module with the writer-claim LIFETIME contract: the
``session:<uuid>`` claim is taken in the create path for every resume, re-pinned
to the spawned supervisor's pid, and outlives the acquiring process on success
AND on registry-write failure. It is released only when no child was spawned.
"""
from __future__ import annotations

import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir
from fno.agents import dispatch
from fno.agents.registry import AgentEntry, load_registry, update_registry

DEAD_UUID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
OTHER_UUID = "ffffffff-1111-2222-3333-444444444444"


@pytest.fixture
def workdir_claude(tmp_path, monkeypatch):
    """Isolated fno home with the fake claude on PATH (emits short_id 7c5dcf5d).
    Mirrors test_spawn_uuid_capture's fixture."""
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


def _seed_row(name: str, short_id: str, uuid) -> None:
    row = AgentEntry(
        name=name,
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/rev.log",
        short_id=short_id,
        harness_session_id=uuid,
    )
    update_registry(lambda entries: entries + [row])


# ---------------------------------------------------------------------------
# Unit: the _is_revival gate (probes reality, not the status field)
# ---------------------------------------------------------------------------


def _mk(**kw) -> AgentEntry:
    base = dict(
        name="w",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/rev.log",
        short_id="deadbeef",
        harness_session_id=DEAD_UUID,
    )
    base.update(kw)
    return AgentEntry(**base)


def test_is_revival_gate(monkeypatch) -> None:
    from fno.agents.providers import claude as claude_mod

    # Dead supervisor: a --resume that matches the row's own uuid is a revival.
    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: False)
    row = _mk()
    assert dispatch._is_revival(row, "claude", DEAD_UUID) is True
    assert dispatch._is_revival(row, "claude", None) is False  # no --resume
    assert dispatch._is_revival(row, "claude", OTHER_UUID) is False  # uuid mismatch
    assert dispatch._is_revival(row, "codex", DEAD_UUID) is False  # non-claude spawn
    assert (
        dispatch._is_revival(_mk(harness="codex"), "claude", DEAD_UUID) is False
    )  # non-claude row

    # A live supervisor is a collision, never a revival - even with a uuid match.
    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: True)
    assert dispatch._is_revival(row, "claude", DEAD_UUID) is False


# ---------------------------------------------------------------------------
# Integration: the CLI spawn path
# ---------------------------------------------------------------------------


def test_spawn_resume_revives_in_place(workdir_claude, monkeypatch) -> None:
    """AC1-HP: spawn --resume against the row's own exited name updates it in
    place - one row, fresh short_id, same uuid - instead of exiting 2."""
    from fno.agents.cli import agents_app
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: False)
    _seed_row("rev-agent", "deadbeef", DEAD_UUID)

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "rev-agent", "-p", "claude", "--resume", DEAD_UUID,
         "--substrate", "bg", "hi"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output

    rows = [e for e in load_registry() if e.name == "rev-agent"]
    assert len(rows) == 1  # revived in place, not a same-name duplicate
    assert rows[0].short_id == "7c5dcf5d"  # fresh short_id from the new spawn
    assert rows[0].harness_session_id == DEAD_UUID  # same conversation preserved


def test_spawn_resume_uuid_mismatch_is_collision(workdir_claude, monkeypatch) -> None:
    """AC2-EDGE: a --resume uuid that is not the row's own is a collision, not a
    revival; the row is left untouched."""
    from fno.agents.cli import agents_app
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: False)
    _seed_row("rev-agent", "deadbeef", DEAD_UUID)

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "rev-agent", "-p", "claude", "--resume", OTHER_UUID,
         "--substrate", "bg", "hi"],
        catch_exceptions=False,
    )
    assert result.exit_code == 2, result.output
    rows = [e for e in load_registry() if e.name == "rev-agent"]
    assert len(rows) == 1
    assert rows[0].short_id == "deadbeef"  # unchanged


def test_spawn_same_name_no_resume_is_collision(workdir_claude, monkeypatch) -> None:
    """AC2-HP: a same-name spawn without --resume is the ordinary collision."""
    from fno.agents.cli import agents_app
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: False)
    _seed_row("rev-agent", "deadbeef", DEAD_UUID)

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "rev-agent", "-p", "claude", "--substrate", "bg", "hi"],
        catch_exceptions=False,
    )
    assert result.exit_code == 2, result.output


def test_spawn_resume_refused_when_session_claim_held(
    workdir_claude, monkeypatch, tmp_path
) -> None:
    """x-9844 Lane 2: a detached revival refuses (exit 11) when another live
    writer already holds the session:<uuid> claim, instead of spawning a second
    supervisor onto one transcript. The row is left untouched."""
    import os as _os

    from fno.agents.cli import agents_app
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: False)
    _seed_row("rev-agent", "deadbeef", DEAD_UUID)

    # A different live writer already holds the session single-writer claim.
    claude_mod.acquire_session_writer_claim(
        session_uuid=DEAD_UUID, holder="other-writer", pid=_os.getpid()
    )

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "rev-agent", "-p", "claude", "--resume", DEAD_UUID,
         "--substrate", "bg", "hi"],
        catch_exceptions=False,
    )
    assert result.exit_code == 11, result.output
    rows = [e for e in load_registry() if e.name == "rev-agent"]
    assert len(rows) == 1
    assert rows[0].short_id == "deadbeef"  # not revived - no 2nd supervisor


# ---------------------------------------------------------------------------
# x-7fef: writer-claim lifetime - pinned to the supervisor, outlives us
# ---------------------------------------------------------------------------

CLAIM_KEY = f"session:{DEAD_UUID}"


@pytest.fixture
def revive_ready(workdir_claude, monkeypatch, tmp_path):
    """A seeded exited row + isolated claims root, ready for `spawn --resume`.

    Yields a real, LIVE pid that stands in for the spawned supervisor. It must be
    live so the pinned claim classifies live (a dead pid gets reclaimed by stale
    recovery, and held-vs-freed becomes indistinguishable), and it must NOT be
    this process's pid or the re-pin assertion would hold vacuously - the claim
    starts out pinned to the acquiring process.
    """
    import subprocess
    import sys

    from fno.agents.providers import claude as claude_mod

    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setattr(claude_mod, "session_is_live", lambda sid: False)
    _seed_row("rev-agent", "deadbeef", DEAD_UUID)

    # sys.executable, not `sleep`: workdir_claude stubs PATH to the fake-claude
    # bin dir, so a bare command name is unresolvable here.
    sleeper = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
    assert sleeper.pid != os.getpid()
    try:
        yield sleeper.pid
    finally:
        sleeper.terminate()
        sleeper.wait()


def _pin_supervisor(monkeypatch, pid: int | None) -> None:
    """Point ``locate_session`` at a supervisor with ``pid`` (None = sidecar miss)."""
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers._claude_session_registry import SessionLocator

    def _locate(short_id: str):
        if pid is None:
            return None
        return SessionLocator(
            pid=pid,
            short_id=short_id,
            messaging_socket_path="/tmp/fake.sock",
            jobs_dir=Path("/tmp"),
        )

    monkeypatch.setattr(claude_mod, "locate_session", _locate)


def _spawn_resume() -> object:
    from fno.agents.cli import agents_app

    return CliRunner().invoke(
        agents_app,
        ["spawn", "rev-agent", "-p", "claude", "--resume", DEAD_UUID,
         "--substrate", "bg", "hi"],
        catch_exceptions=False,
    )


def _status() -> dict:
    from fno.claims import claim_status
    from fno.claims.io import global_claims_root

    return claim_status(CLAIM_KEY, root=global_claims_root())


def test_writer_claim_pinned_to_supervisor_and_survives(revive_ready, monkeypatch):
    """AC3-HP: after a successful wake the claim is HELD and pinned to the
    supervisor's pid - not released, and not pinned to the spawning process
    (which is what made the old claim dead-pid the moment it exited)."""
    supervisor_pid = revive_ready
    _pin_supervisor(monkeypatch, supervisor_pid)

    result = _spawn_resume()
    assert result.exit_code == 0, result.output

    st = _status()
    assert st["state"] == "live"
    assert st["pid"] == supervisor_pid


def test_writer_claim_held_when_registry_write_fails(revive_ready, monkeypatch):
    """AC4-FR: the exit-12 orphan path KEEPS the claim. The supervisor is writing
    the transcript even though no registry row names it, so freeing the claim
    here is exactly the double-writer window codex flagged."""
    from fno.claims import acquire_claim
    from fno.claims.core import ClaimHeldByOther
    from fno.claims.io import global_claims_root

    supervisor_pid = revive_ready
    _pin_supervisor(monkeypatch, supervisor_pid)

    def _boom(_fn):
        raise OSError("registry unwritable")

    monkeypatch.setattr(dispatch, "update_registry", _boom)

    result = _spawn_resume()
    assert result.exit_code == 12, result.output

    st = _status()
    assert st["state"] == "live"
    assert st["pid"] == supervisor_pid

    with pytest.raises(ClaimHeldByOther):
        acquire_claim(CLAIM_KEY, "someone-else", root=global_claims_root())


def test_writer_claim_released_when_spawn_fails(revive_ready, monkeypatch):
    """AC5-ERR: no child spawned means nothing is writing, so the claim must be
    freed - otherwise a failed wake locks the session out of every later wake."""
    from fno.agents.providers import claude as claude_mod

    _pin_supervisor(monkeypatch, revive_ready)

    def _fail(**_kw):
        raise claude_mod.ProviderSubprocessError(1, "spawn exploded")

    monkeypatch.setattr(claude_mod, "bg_create", _fail)

    result = _spawn_resume()
    assert result.exit_code == 1, result.output
    assert _status()["state"] == "free"

    # And a fresh holder can immediately take it.
    from fno.claims import acquire_claim
    from fno.claims.io import global_claims_root

    assert acquire_claim(CLAIM_KEY, "next-writer", root=global_claims_root())


def test_writer_claim_degrades_when_supervisor_pid_unresolvable(
    revive_ready, monkeypatch
):
    """Degrade: a sidecar race leaves no pid to pin to. Fall back to the old
    lifetime (release + warn) rather than strand a claim pinned to this exiting
    process - and never block the wake itself."""
    _pin_supervisor(monkeypatch, None)

    result = _spawn_resume()
    assert result.exit_code == 0, result.output  # the wake still succeeds
    assert _status()["state"] == "free"

    combined = result.output + (result.stderr if result.stderr_bytes else "")
    assert "could not resolve supervisor pid" in combined


def test_wake_and_deliver_takes_no_outer_claim(revive_ready, monkeypatch):
    """Nesting regression: `wake_and_deliver` no longer acquires the claim itself.
    The outer/inner same-holder pair was the bug - the inner release dropped the
    outer claim, because same-holder re-acquire is idempotent, not refcounted."""
    calls: list[dict] = []
    from fno.agents.providers import claude as claude_mod

    real_acquire = claude_mod.acquire_session_writer_claim

    def _spy(**kw):
        calls.append(kw)
        return real_acquire(**kw)

    monkeypatch.setattr(claude_mod, "acquire_session_writer_claim", _spy)
    monkeypatch.setattr(
        dispatch, "dispatch_spawn", lambda **kw: type("R", (), {"short_id": "7c5dcf5d"})()
    )

    ok, short = dispatch.wake_and_deliver(DEAD_UUID, "wake up")
    assert (ok, short) == (True, "7c5dcf5d")
    assert calls == []  # every acquire now lives inside the create path
