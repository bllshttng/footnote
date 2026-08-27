"""`fno agents king manifest-path` resolves a live crown to its manifest.

The stop hook calls the deprecated `fno king manifest-path` spelling, which
verb_moves forwards onto the agents app. The verb missed the agents fold, so
the resolver exited 2 and every stop on an active kings dir burned its
unavailable-retries before allowing exit. These tests pin the verb onto the
agents app and pin the deprecated spelling onto the same command.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.registry import AgentEntry, update_registry
from fno.king.state import king_manifest_path, write_manifest
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "0c1f2f9a-2222-4000-8000-000000000002"
SCOPE = "epic-y"


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
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _seat_crown():
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="crowned-king",
                cwd="/tmp",
                log_path="",
                harness="claude",
                harness_session_id=CALLER_SESSION,
                status="busy",
                crown_level=2,
                crown_scope=SCOPE,
                crown_grantor="human",
            )
        ]
    )
    return king_manifest_path(SCOPE, state_root=Path(".fno"))


def _manifest_path(*args: str):
    from fno.king.cli import agents_king_app

    return CliRunner().invoke(
        agents_king_app, ["manifest-path", *args]
    )


def test_manifest_path_resolves_a_live_crown(court) -> None:
    manifest = _seat_crown()
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)

    result = _manifest_path(
        "--harness-session-id", CALLER_SESSION,
        "--state-root", str(court / ".fno"),
    )

    assert result.exit_code == 0, result.output
    assert str(manifest) in result.output


def test_manifest_path_frees_a_stranger(court) -> None:
    """No registry row names this session: exit 1, the hook's "stranger goes
    free" contract, never the exit-2 parse failure the missing verb produced."""
    result = _manifest_path(
        "--harness-session-id", CALLER_SESSION,
        "--state-root", str(court / ".fno"),
    )

    assert result.exit_code == 1, result.output


def test_deprecated_king_spelling_forwards_onto_the_agents_app(court) -> None:
    """The stop hook's literal argv: `fno king manifest-path ...`. The banner
    must stay on stderr so the hook's stdout capture reads a clean path."""
    from fno.cli import app

    manifest = _seat_crown()
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)

    result = CliRunner().invoke(
        app,
        [
            "king", "manifest-path",
            "--harness-session-id", CALLER_SESSION,
            "--state-root", str(court / ".fno"),
        ],
    )

    assert result.exit_code == 0, result.output
    assert str(manifest) in result.output
    assert "is now" in (result.stderr or ""), "the rename banner names the new spelling"
