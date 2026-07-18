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
"""
from __future__ import annotations

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
