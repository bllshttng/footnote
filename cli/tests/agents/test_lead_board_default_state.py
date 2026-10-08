"""A bare `fno inbox board` from a promoted session defaults to that role.

The --state flag existed but nothing passed it, so every interactive lead read
was fleet-wide while the term goal text promised "no actionable rows for the
role scope". These tests pin the default: the caller's registry row plus an
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

import fno.agents.role as role_mod
from fno.agents.registry import AgentEntry, update_registry
from fno.lead.state import lead_manifest_path, lead_state_root, write_manifest
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "5d2b9c1a-3333-4000-8000-000000000017"
SCOPE = "epic-board"

BOARD_PAYLOAD = json.dumps({"exit_code": 0, "queues": []})


@pytest.fixture
def team(tmp_path, monkeypatch):
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
    real_run = subprocess.run
    import fno.rust_binary as rust_binary

    real_binary = rust_binary.resolve_binary()

    def fake_run(cmd, *args, **kwargs):
        if list(cmd[1:2]) == ["registry-commit"]:
            return real_run([str(real_binary), *cmd[1:]], *args, **kwargs)
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


def _seat_role(team):
    # The resolver keys the manifest root on the seated row's cwd, so the row
    # must carry the dir the fixture writes under.
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="promoted-lead",
                cwd=str(team),
                log_path="",
                harness="claude",
                harness_session_id=CALLER_SESSION,
                status="busy",
                role_level=2,
                role_scope=SCOPE,
                role_grantor="human",
            )
        ]
    )
    return lead_manifest_path(SCOPE, state_root=lead_state_root(team))


def _board(monkeypatch, *args: str, caller=None):
    monkeypatch.setattr(role_mod, "calling_agent_row", lambda: caller)
    from fno.lead.cli import lead_app

    return CliRunner().invoke(lead_app, ["board", "--json", *args])


def _promoted_caller():
    return types.SimpleNamespace(
        harness_session_id=CALLER_SESSION,
        cc_session_id=None,
        harness="claude",
    )


def test_promoted_caller_defaults_to_its_own_manifest(
    team, captured_argv, monkeypatch
) -> None:
    manifest = _seat_role(team)
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)

    result = _board(monkeypatch, caller=_promoted_caller())

    assert result.exit_code == 0, result.output
    argv = captured_argv()
    i = argv.index("--state")
    assert Path(argv[i + 1]) == manifest, argv


def test_plain_caller_stays_fleet_wide(team, captured_argv, monkeypatch) -> None:
    result = _board(monkeypatch, caller=None)

    assert result.exit_code == 0, result.output
    assert "--state" not in captured_argv(), captured_argv()


def test_unreadable_registry_caller_stays_fleet_wide(
    team, captured_argv, monkeypatch
) -> None:
    result = _board(monkeypatch, caller=role_mod.REGISTRY_UNREADABLE)

    assert result.exit_code == 0, result.output
    assert "--state" not in captured_argv(), captured_argv()


def test_role_without_manifest_file_stays_fleet_wide(
    team, captured_argv, monkeypatch
) -> None:
    """Row promoted, file absent: presence alone was never the authority."""
    _seat_role(team)

    result = _board(monkeypatch, caller=_promoted_caller())

    assert result.exit_code == 0, result.output
    assert "--state" not in captured_argv(), captured_argv()


def test_explicit_state_wins_over_the_default(
    team, captured_argv, monkeypatch
) -> None:
    manifest = _seat_role(team)
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)
    explicit = team / "other-lead.md"

    result = _board(monkeypatch, "--state", str(explicit), caller=_promoted_caller())

    assert result.exit_code == 0, result.output
    argv = captured_argv()
    assert argv[argv.index("--state") + 1] == str(explicit), argv
