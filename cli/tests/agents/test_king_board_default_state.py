"""A bare `fno inbox board` from a crowned session defaults to that crown.

The --state flag existed but nothing passed it, so every interactive king read
was fleet-wide while the reign goal text promised "no actionable rows for the
crown scope". These tests pin the default: the caller's registry row plus an
existing manifest file scopes the board; every other caller stays fleet-wide;
an explicit --state still wins.
"""
from __future__ import annotations

import json
import subprocess
import types
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.agents.crown as crown_mod
from fno.agents.registry import AgentEntry, update_registry
from fno.king.state import king_manifest_path, write_manifest
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "5d2b9c1a-3333-4000-8000-000000000017"
SCOPE = "epic-board"

BOARD_PAYLOAD = json.dumps({"exit_code": 0, "queues": []})


@pytest.fixture
def court(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.chdir(tmp_path)
    return tmp_path


@pytest.fixture
def captured_argv(monkeypatch):
    """Stub the collector binary: record the argv, answer a green payload.

    Other subprocess.run calls (path resolution shells out to git) land in
    `seen` too; the board call is the one whose argv starts with the fake
    binary, which `board_argv()` extracts.
    """
    seen: list[list[str]] = []

    def fake_run(cmd, *args, **kwargs):
        seen.append(cmd)
        if str(cmd[0]) == "/fake/fno-agents":
            return types.SimpleNamespace(
                returncode=0, stdout=BOARD_PAYLOAD, stderr=""
            )
        return types.SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary", lambda: Path("/fake/fno-agents")
    )

    def board_argv() -> list[str]:
        return next(a for a in seen if str(a[0]) == "/fake/fno-agents")

    return board_argv


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
    return king_manifest_path(SCOPE)


def _board(monkeypatch, *args: str, caller=None):
    monkeypatch.setattr(crown_mod, "calling_agent_row", lambda: caller)
    from fno.king.cli import king_app

    return CliRunner().invoke(king_app, ["board", "--json", *args])


def _crowned_caller():
    return types.SimpleNamespace(
        harness_session_id=CALLER_SESSION,
        cc_session_id=None,
        harness="claude",
    )


def test_crowned_caller_defaults_to_its_own_manifest(
    court, captured_argv, monkeypatch
) -> None:
    manifest = _seat_crown()
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)

    result = _board(monkeypatch, caller=_crowned_caller())

    assert result.exit_code == 0, result.output
    argv = captured_argv()
    i = argv.index("--state")
    assert Path(argv[i + 1]) == manifest, argv


def test_plain_caller_stays_fleet_wide(court, captured_argv, monkeypatch) -> None:
    result = _board(monkeypatch, caller=None)

    assert result.exit_code == 0, result.output
    assert "--state" not in captured_argv(), captured_argv()


def test_unreadable_registry_caller_stays_fleet_wide(
    court, captured_argv, monkeypatch
) -> None:
    result = _board(monkeypatch, caller=crown_mod.REGISTRY_UNREADABLE)

    assert result.exit_code == 0, result.output
    assert "--state" not in captured_argv(), captured_argv()


def test_crown_without_manifest_file_stays_fleet_wide(
    court, captured_argv, monkeypatch
) -> None:
    """Row crowned, file absent: presence alone was never the authority."""
    _seat_crown()

    result = _board(monkeypatch, caller=_crowned_caller())

    assert result.exit_code == 0, result.output
    assert "--state" not in captured_argv(), captured_argv()


def test_explicit_state_wins_over_the_default(
    court, captured_argv, monkeypatch
) -> None:
    manifest = _seat_crown()
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)
    explicit = court / "other-king.md"

    result = _board(monkeypatch, "--state", str(explicit), caller=_crowned_caller())

    assert result.exit_code == 0, result.output
    argv = captured_argv()
    assert argv[argv.index("--state") + 1] == str(explicit), argv
