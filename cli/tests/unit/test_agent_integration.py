"""End-to-end tests for `fno whoami` / `fno status` through the console script.

Builds tmp workspaces with realistic fleet/walker/target/session combos and
runs each command through subprocess (the installed `fno-py` entry point).
The in-process CliRunner suite (test_agent_cli.py) owns the per-branch cases.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
FIXTURES = Path(__file__).parent / "fixtures" / "agent"


def _fno(args, cwd: Path, env: dict = None) -> subprocess.CompletedProcess:
    """Invoke `fno` via uv run --project cli."""
    full_env = os.environ.copy()
    if env:
        full_env.update(env)
    return subprocess.run(
        # Absolute --project so uv resolves cli/'s env (which has the `fno-py`
        # console script) regardless of `cwd`. A relative "cli" resolves against
        # the tmpdir cwd, misses, and falls back to the ambient PATH - which used
        # to accidentally find an installed `fno`, but there is no ambient `fno-py`.
        ["uv", "run", "--project", str(REPO_ROOT / "cli"), "fno-py", *args],
        capture_output=True,
        text=True,
        cwd=cwd,
        env=full_env,
        timeout=60,
    )


def _build_full_fixture(tmp_path: Path) -> tuple[Path, Path]:
    """Build (project, fake_home) with fleet+walker+target state."""
    project = tmp_path / "project"
    fno = project / ".fno"
    fno.mkdir(parents=True)
    (fno / "target-state.md").write_text(
        (FIXTURES / "target-state.md").read_text()
    )
    (fno / "megawalk-state.md").write_text(
        (FIXTURES / "megawalk-state.md").read_text()
    )
    fake_home = tmp_path / "fake_home"
    fleet_root = fake_home / ".fno" / "fleet" / "fleet-fixture-001"
    fleet_root.mkdir(parents=True)
    body = (FIXTURES / "fleet-mission.md").read_text().replace(
        "__PROJECT_ROOT__", str(project.resolve())
    )
    (fleet_root / "00-INDEX.md").write_text(body)
    return project, fake_home


def test_end_to_end_journey_consistent_session_id(tmp_path):
    """Run both commands in sequence; each reports the same session_id.

    Catches cross-verb state contamination: if either command left the
    state-loader cache in a bad shape, the next would differ.
    """
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    expected_sid = "20260512T010101Z-99999-fixaaa"

    # whoami: session id visible bare
    r1 = _fno(["whoami"], cwd=project, env=env)
    assert r1.returncode == 0
    assert expected_sid in r1.stdout

    # status: session id in the session: line
    r2 = _fno(["status"], cwd=project, env=env)
    assert r2.returncode == 0
    assert expected_sid in r2.stdout

    # status JSON mode -> dict with events_tail key
    r3 = _fno(["status", "--json"], cwd=project, env=env)
    assert r3.returncode == 0
    payload = json.loads(r3.stdout)
    assert "events_tail" in payload


def test_state_file_override(tmp_path):
    project, fake_home = _build_full_fixture(tmp_path)
    override = tmp_path / "custom.md"
    override.write_text((FIXTURES / "session-state-think.md").read_text())
    env = {"HOME": str(fake_home)}
    result = _fno(
        ["whoami", "--state-file", str(override)],
        cwd=project, env=env,
    )
    assert result.returncode == 0
    assert "phase=think" in result.stdout
