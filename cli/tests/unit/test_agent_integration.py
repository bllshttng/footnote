"""End-to-end integration tests for `fno whoami` / `fno status` against fixtures.

Builds tmp workspaces with realistic fleet/walker/target/session combos,
runs each command through subprocess (the actual `fno` binary path), and
asserts output matches expected patterns. Plus the read-only invariant
verified across repeated invocations. These were formerly `fno agent whoami`
/ `fno agent status`; the `fno agent` (singular) namespace was retired in
ab-12dd2a5d.
"""
from __future__ import annotations

import hashlib
import json
import os
import subprocess
from pathlib import Path

import pytest

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


@pytest.mark.parametrize("verb", ["whoami", "status"])
def test_command_runs_against_full_fixture(tmp_path, verb):
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    result = _fno([verb], cwd=project, env=env)
    assert result.returncode == 0, (
        f"fno {verb} failed: rc={result.returncode}\n"
        f"stdout: {result.stdout}\nstderr: {result.stderr}"
    )
    assert result.stdout, f"fno {verb} produced no stdout"


@pytest.mark.parametrize("verb", ["whoami", "status"])
def test_command_json_mode_emits_valid_json(tmp_path, verb):
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    result = _fno([verb, "--json"], cwd=project, env=env)
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload is not None


def test_whoami_full_fixture_shows_all_layers(tmp_path):
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    result = _fno(["whoami"], cwd=project, env=env)
    assert result.returncode == 0
    out = result.stdout
    assert "fleet-fixture-001" in out
    assert "20260512T010101Z-99999-fixaaa" in out  # target session id
    assert "claude" in out


def test_fno_agent_namespace_absent(tmp_path):
    """AC2-EDGE: the retired `fno agent` (singular) namespace is gone end-to-end
    - a clean usage error (non-zero), not a silent wrong result."""
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    result = _fno(["agent", "whoami"], cwd=project, env=env)
    assert result.returncode != 0
    assert "No such command 'agent'" in (result.stdout + result.stderr)


def test_read_only_invariant_repeated_invocations(tmp_path):
    """Repeated invocations must not change any read input's md5.

    Covers target-state.md, megawalk-state.md, events.jsonl, and the fleet
    INDEX - every file the commands open for reading. A regression that opens
    any of these in `r+` or rewrites them in place would surface here.
    """
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    # Seed events.jsonl so status has something to tail; this file is
    # read-only but the invariant must cover it.
    events = project / ".fno" / "events.jsonl"
    events.write_text('{"ts":"T1","type":"phase_init","data":{}}\n')
    fleet_index = (
        fake_home / ".fno" / "fleet" / "fleet-fixture-001" / "00-INDEX.md"
    )
    monitored = [
        project / ".fno" / "target-state.md",
        project / ".fno" / "megawalk-state.md",
        events,
        fleet_index,
    ]

    def _hashes() -> dict:
        return {
            str(p.relative_to(tmp_path)): hashlib.md5(p.read_bytes()).hexdigest()
            for p in monitored
            if p.exists()
        }

    before = _hashes()
    for _ in range(25):
        for verb in ("whoami", "status"):
            result = _fno([verb], cwd=project, env=env)
            assert result.returncode == 0, (
                f"verb {verb} failed: {result.stderr}"
            )
    after = _hashes()
    assert before == after, f"state mutated: {before} -> {after}"


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


@pytest.mark.parametrize("verb", ["whoami", "status"])
def test_command_help_resolves(tmp_path, verb):
    """Both commands respond to --help with rc=0."""
    result = _fno([verb, "--help"], cwd=REPO_ROOT)
    assert result.returncode == 0
    assert "Usage:" in result.stdout


def test_no_walker_flag_drops_walker_layer(tmp_path):
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    result = _fno(["whoami", "--no-walker"], cwd=project, env=env)
    assert result.returncode == 0
    assert "walker:" not in result.stdout


def test_no_fleet_flag_drops_fleet_layer(tmp_path):
    project, fake_home = _build_full_fixture(tmp_path)
    env = {"HOME": str(fake_home)}
    result = _fno(["whoami", "--no-fleet"], cwd=project, env=env)
    assert result.returncode == 0
    assert "fleet:" not in result.stdout


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


def test_malformed_session_state_exits_2(tmp_path):
    project = tmp_path / "p"
    fno = project / ".fno"
    fno.mkdir(parents=True)
    (fno / "target-state.md").write_text(
        (FIXTURES / "malformed-state.md").read_text()
    )
    result = _fno(["whoami"], cwd=project)
    assert result.returncode == 2
    assert "malformed" in result.stderr.lower()
