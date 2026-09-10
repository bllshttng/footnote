"""A thread receipt says live only once claude's job state records the prompt.

Claude writes a non-empty ``intent`` before ``claude --bg`` returns. A session
started with no prompt reads ``intent: ""`` and holds a slot waiting for input.
"""
from __future__ import annotations

import functools
import json

import pytest
from typer.testing import CliRunner

from fno.agents.harnesses import _claude_session_registry as registry
from fno.agents.harnesses._claude_session_registry import seed_unverified_reason
from fno.paths_testing import use_tmpdir

SHORT_ID = "7c5dcf5d"


def _write_state(config, intent):
    jobs = config / "jobs" / SHORT_ID
    jobs.mkdir(parents=True)
    (jobs / "state.json").write_text(json.dumps({"state": "running", "intent": intent}))


def test_a_recorded_prompt_is_verified(tmp_path):
    _write_state(tmp_path, "/fno:target x-1")
    env = {"CLAUDE_CONFIG_DIR": str(tmp_path)}
    assert seed_unverified_reason(SHORT_ID, env, timeout_s=0) is None


@pytest.mark.parametrize("write", [False, True], ids=["no-job-state", "empty-intent"])
def test_an_unrecorded_prompt_names_the_path_it_read(tmp_path, write):
    if write:
        _write_state(tmp_path, "")
    env = {"CLAUDE_CONFIG_DIR": str(tmp_path)}
    reason = seed_unverified_reason(SHORT_ID, env, timeout_s=0)
    assert reason is not None
    assert str(tmp_path / "jobs" / SHORT_ID / "state.json") in reason


@pytest.fixture
def claude_config(tmp_path, monkeypatch):
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.chdir(tmp_path)
    install_fake_claude(tmp_path / "bin")
    monkeypatch.setenv("PATH", str(tmp_path / "bin"))
    # A spawn with no account reads ~/.claude, like the Rust reader.
    monkeypatch.setenv("HOME", str(tmp_path))
    config = tmp_path / ".claude"
    # The suite stubs the check to verified; these tests run the real reader.
    monkeypatch.setattr(
        registry, "seed_unverified_reason", functools.partial(seed_unverified_reason, timeout_s=0)
    )
    return config


def _spawn_first_line():
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "seeded", "-H", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output
    return result.output.split("\n")[0].strip()


def test_the_receipt_reads_live_once_claude_records_the_prompt(claude_config):
    _write_state(claude_config, "hello")
    assert _spawn_first_line() == (
        '{"name": "seeded", "short_id": "7c5dcf5d", "harness": "claude", "status": "live"}'
    )


def test_an_unrecorded_prompt_reads_spawning_and_names_the_state_path(claude_config):
    receipt = json.loads(_spawn_first_line())
    assert receipt["status"] == "spawning"
    assert receipt["seed"] == "unverified"
    assert str(claude_config / "jobs" / SHORT_ID / "state.json") in receipt["seed_unverified"]
