"""The expire half of the crown lifecycle: `fno agents king done`.

A crown that nothing expires makes every later king pay `--force`, so the
abdication path must clear both halves the crown owns: the registry row (the
authority) and the scope manifest (the loop arm). The ordering is the
succession ordering - vacate under the registry lock first, then clean the
file - so a scope that moved to an heir mid-call is never disarmed.
"""
from __future__ import annotations

from dataclasses import replace

import pytest
from typer.testing import CliRunner

from fno.agents.registry import AgentEntry, load_registry, update_registry
from fno.king.state import king_manifest_path, parse_manifest, write_manifest
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "0c1f2f9a-1111-4000-8000-000000000001"
SCOPE = "epic-x"


@pytest.fixture(autouse=True)
def _clear_parent_markers(monkeypatch):
    for marker in (
        "FNO_SESSION",
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
    ):
        monkeypatch.delenv(marker, raising=False)


@pytest.fixture
def court(tmp_path, monkeypatch):
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CALLER_SESSION)
    # The verb resolves the manifest state root from the caller's cwd; seat
    # the court so that resolution lands on the tmp state root too.
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _seat(name: str, session: str, *, scope: str | None = SCOPE, status: str = "busy"):
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name=name,
                cwd="/tmp",
                log_path="",
                harness="claude",
                harness_session_id=session,
                status=status,
                crown_level=2 if scope else None,
                crown_scope=scope,
                crown_grantor="human" if scope else None,
            )
        ]
    )


def _manifest(court, scope: str = SCOPE, session: str = CALLER_SESSION):
    path = king_manifest_path(scope, state_root=court / ".fno")
    write_manifest(path, scope=scope, harness_session_id=session)
    return path


def _row(name: str):
    return next((e for e in load_registry() if e.name == name), None)


def _done(*args: str):
    from fno.king.cli import agents_king_app

    return CliRunner().invoke(agents_king_app, ["done", *args])


def test_done_expires_the_manifest_and_arms_a_successor_without_force(court) -> None:
    """The abdication contract: both crown halves clear, and the next init
    over the same scope writes without --force."""
    _seat("sitting-king", CALLER_SESSION)
    manifest = _manifest(court)

    result = _done()

    assert result.exit_code == 0, result.output
    king = _row("sitting-king")
    assert (king.crown_level, king.crown_scope, king.crown_grantor) == (None, None, None)
    assert not manifest.exists(), "the scope manifest must be cleared"
    write_manifest(manifest, scope=SCOPE, harness_session_id="successor-session")


def test_done_refuses_a_scope_the_caller_does_not_hold(court) -> None:
    _seat("sitting-king", CALLER_SESSION, scope="epic-own")
    other = _manifest(court, scope="epic-other")

    result = _done("--scope", "epic-other")

    assert result.exit_code == 2, result.output
    assert "only its own crown" in result.output
    assert other.exists(), "a refused expire must leave the manifest alone"
    assert _row("sitting-king").crown_scope == "epic-own"


def test_done_refuses_when_the_crown_moved_before_the_write(court, monkeypatch) -> None:
    """The vacate closure re-reads the row under the registry lock, so a scope
    that already moved to an heir is refused rather than disarming the heir's
    manifest. Simulated by moving the crown between the CLI's identity read
    and its registry write."""
    _seat("sitting-king", CALLER_SESSION)
    manifest = _manifest(court)
    from fno.agents import registry as registry_mod

    real_update = registry_mod.update_registry

    def _move_then_update(fn):
        # The heir is crowned by a concurrent succession between the CLI's
        # caller read and its vacate write.
        real_update(
            lambda rows: [
                (
                    replace(row, crown_scope=None, crown_level=None, crown_grantor=None)
                    if row.name == "sitting-king"
                    else row
                )
                for row in rows
            ]
            + [
                AgentEntry(
                    name="heir",
                    cwd="/tmp",
                    log_path="",
                    harness="claude",
                    harness_session_id="heir-session",
                    status="busy",
                    crown_level=2,
                    crown_scope=SCOPE,
                    crown_grantor="sitting-king",
                )
            ]
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _move_then_update)

    result = _done()

    assert result.exit_code == 1, result.output
    assert "no longer holds" in result.output
    assert manifest.exists(), "the heir's manifest must survive the refusal"
    assert _row("heir").crown_scope == SCOPE


def test_done_leaves_a_successor_manifest_crowned_in_the_vacate_window(
    court, monkeypatch
) -> None:
    """A successor init --force that lands between the row vacate and the
    manifest unlink must survive it: the unlink compares against the session
    id snapshotted before the vacate, so the file this expiry deletes can
    only ever be the one it decided to expire."""
    _seat("sitting-king", CALLER_SESSION)
    manifest = _manifest(court)
    from fno.agents import registry as registry_mod

    real_update = registry_mod.update_registry

    def _crown_successor_then_update(fn):
        write_manifest(
            manifest, scope=SCOPE, harness_session_id="successor-session", force=True
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _crown_successor_then_update)

    result = _done()

    assert result.exit_code == 1, result.output
    assert "no longer names the session" in result.output
    assert manifest.exists(), "the successor's manifest must survive"
    assert parse_manifest(manifest)["harness_session_id"] == "successor-session"
    assert _row("sitting-king").crown_scope is None, "the row still vacated"


def test_an_attended_human_expires_a_named_scope(court, monkeypatch) -> None:
    """No agent identity means a human at the keyboard: any scope may expire,
    but it must be named - a human holds no crown to default to."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    manifest = _manifest(court, scope="orphaned-scope", session="long-gone")

    result = _done()

    assert result.exit_code == 2
    assert "--scope" in result.output

    result = _done("--scope", "orphaned-scope")

    assert result.exit_code == 0, result.output
    assert not manifest.exists()


def test_an_agent_with_no_crown_has_nothing_to_expire(court) -> None:
    _seat("plain-worker", CALLER_SESSION, scope=None)

    result = _done()

    assert result.exit_code == 2
    assert "no crown" in result.output
