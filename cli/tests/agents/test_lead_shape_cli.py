"""`fno agents lead shape`: declaring a term's shape is a verb, not prose.

The Stop nudge reads the manifest's shape field to learn whether live spawned
workers are an answered team or an unshaped pass. These tests pin the CLI
half: the holder declares, on its own promoted manifest, idempotently. The
write itself lives in Rust (`fno-agents lead-shape`, pinned by lead_state.rs
tests); the two write tests run the real binary and skip when it is not built.
"""
from __future__ import annotations

import types
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.registry import AgentEntry, update_registry
from fno.lead.state import lead_manifest_path, lead_state_root, parse_manifest, write_manifest
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
def team(tmp_path, monkeypatch):
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CALLER_SESSION)
    # The verb keys the manifest root on the caller row's cwd; the seated row
    # carries that cwd (see _seat's cwd argument).
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


def _seat(
    name: str,
    session: str,
    *,
    scope: str | None = SCOPE,
    status: str = "busy",
    cwd: str = "/tmp",
):
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name=name,
                cwd=cwd,
                log_path="",
                harness="claude",
                harness_session_id=session,
                status=status,
                role_level=2 if scope else None,
                role_scope=scope,
                role_grantor="human" if scope else None,
            )
        ]
    )


def _manifest(team, scope: str = SCOPE, session: str = CALLER_SESSION):
    # The verb keys the manifest root on the caller row's cwd, so the fixture
    # writes through the same call the resolver makes.
    path = lead_manifest_path(scope, state_root=lead_state_root(Path(team)))
    write_manifest(path, scope=scope, harness_session_id=session)
    return path


def _shape(*args: str):
    from fno.lead.cli import agents_lead_app

    return CliRunner().invoke(agents_lead_app, ["shape", *args])


@pytest.mark.skipif(not _binary_available(), reason="fno-agents binary not built")
def test_shape_team_lands_on_the_manifest_and_echoes(team) -> None:
    _seat("serving-lead", CALLER_SESSION, cwd=str(team))
    manifest = _manifest(team)

    result = _shape("team")

    assert result.exit_code == 0, result.output
    assert "shape declared: team" in result.output
    assert parse_manifest(manifest)["shape"] == "team"


@pytest.mark.skipif(not _binary_available(), reason="fno-agents binary not built")
def test_shape_is_idempotent_on_a_second_call(team) -> None:
    _seat("serving-lead", CALLER_SESSION, cwd=str(team))
    manifest = _manifest(team)

    _first = _shape("team")
    assert _first.exit_code == 0, f"DBG={_first.output!r}"
    second = _shape("team")

    assert second.exit_code == 0, second.output
    assert parse_manifest(manifest)["shape"] == "team"


def test_shape_refuses_a_value_outside_the_vocabulary(team) -> None:
    _seat("serving-lead", CALLER_SESSION)
    _manifest(team)

    result = _shape("siege")

    assert result.exit_code == 2, result.output
    assert "'pass' or 'team'" in result.output


def test_shape_refuses_a_session_without_a_role(team) -> None:
    _seat("unpromoted-worker", CALLER_SESSION, scope=None)

    result = _shape("team")

    assert result.exit_code == 2, result.output
    assert "no role" in result.output


def test_shape_refuses_a_foreign_scope(team) -> None:
    _seat("serving-lead", CALLER_SESSION)

    result = _shape("team", "--scope", "epic-other")

    assert result.exit_code == 2, result.output
    assert "only its own term" in result.output


def test_own_role_argv_keys_the_root_on_the_caller_row_cwd(
    team, monkeypatch
) -> None:
    """--root must name the caller row's space, so a lead whose shell sits
    outside the repo still declares on its own manifest (x-8387)."""
    import fno.agents.role as role_mod
    from fno.lead.cli import _own_role_argv
    from fno.lead.state import lead_state_root as _ksr

    kingrepo = team / "kingrepo"
    kingrepo.mkdir()
    row = types.SimpleNamespace(
        harness_session_id=CALLER_SESSION,
        cc_session_id=None,
        harness="claude",
        role_scope=SCOPE,
        cwd=str(kingrepo),
    )
    monkeypatch.setattr(role_mod, "calling_agent_row", lambda: row)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary", lambda: Path("/fake/fno-agents")
    )
    elsewhere = team / "elsewhere"
    elsewhere.mkdir()
    monkeypatch.chdir(elsewhere)

    argv, own = _own_role_argv("lead-shape", "")

    assert own == SCOPE
    i = argv.index("--root")
    assert argv[i + 1] == str(_ksr(kingrepo)), argv
