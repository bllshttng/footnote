"""`fno agents king shape`: declaring a reign's shape is a verb, not prose.

The Stop nudge reads the manifest's shape field to learn whether live spawned
workers are an answered court or an unshaped pass. These tests pin the CLI
half: the holder declares, on its own crowned manifest, idempotently. The
write itself lives in Rust (`fno-agents reign-shape`, pinned by loop_reign.rs
tests); the two write tests run the real binary and skip when it is not built.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.registry import AgentEntry, update_registry
from fno.king.state import king_manifest_path, parse_manifest, write_manifest
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "5d4c3b2a-1111-4000-8000-000000000001"
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
    built = Path(__file__).parents[3] / (
        "crates/fno-agents/target/debug/fno-agents"
    )
    if built.is_file():
        monkeypatch.setenv("FNO_AGENTS_BIN", str(built))
    return tmp_path


def _binary_available() -> bool:
    import os

    if os.environ.get("FNO_AGENTS_BIN"):
        return Path(os.environ["FNO_AGENTS_BIN"]).is_file()
    return (Path(__file__).parents[3] / "crates/fno-agents/target/debug/fno-agents").is_file()


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


def _shape(*args: str):
    from fno.king.cli import agents_king_app

    return CliRunner().invoke(agents_king_app, ["shape", *args])


@pytest.mark.skipif(not _binary_available(), reason="fno-agents binary not built")
def test_shape_court_lands_on_the_manifest_and_echoes(court) -> None:
    _seat("reigning-king", CALLER_SESSION)
    manifest = _manifest(court)

    result = _shape("court")

    assert result.exit_code == 0, result.output
    assert "shape declared: court" in result.output
    assert parse_manifest(manifest)["shape"] == "court"


@pytest.mark.skipif(not _binary_available(), reason="fno-agents binary not built")
def test_shape_is_idempotent_on_a_second_call(court) -> None:
    _seat("reigning-king", CALLER_SESSION)
    manifest = _manifest(court)

    assert _shape("court").exit_code == 0
    second = _shape("court")

    assert second.exit_code == 0, second.output
    assert parse_manifest(manifest)["shape"] == "court"


def test_shape_refuses_a_value_outside_the_vocabulary(court) -> None:
    _seat("reigning-king", CALLER_SESSION)
    _manifest(court)

    result = _shape("siege")

    assert result.exit_code == 2, result.output
    assert "'pass' or 'court'" in result.output


def test_shape_refuses_a_session_without_a_crown(court) -> None:
    _seat("uncrowned-worker", CALLER_SESSION, scope=None)

    result = _shape("court")

    assert result.exit_code == 2, result.output
    assert "no crown" in result.output


def test_shape_refuses_a_foreign_scope(court) -> None:
    _seat("reigning-king", CALLER_SESSION)

    result = _shape("court", "--scope", "epic-other")

    assert result.exit_code == 2, result.output
    assert "only its own reign" in result.output
