"""CLI-level tests for `fno agents list` and `fno agents logs`.

These tests exercise the Typer entry points, not the underlying
read.py / providers.claude module. The latter have their own unit tests
in test_read.py / test_providers_claude_read.py. Here we verify flag
wiring, exit codes, TTY behavior, and the AC3-ERR allowed-values list.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir
from fno.agents.registry import AgentEntry, write_registry


def _claude(**kw) -> AgentEntry:
    base = dict(
        name="worker-frontend",
        harness="claude",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-frontend/output.jsonl",
        short_id="abc12345",
        created_at="2026-05-20T17:00:00Z",
        status="live",
        last_message_at="2026-05-20T17:30:12Z",
    )
    base.update(kw)
    return AgentEntry(**base)


def _codex(**kw) -> AgentEntry:
    base = dict(
        name="worker-migration",
        harness="codex",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-migration/output.jsonl",
        harness_session_id="codex-xyz",
        created_at="2026-05-20T17:15:00Z",
        status="live",
        last_message_at="2026-05-20T17:15:43Z",
    )
    base.update(kw)
    return AgentEntry(**base)


def _fake_completed(stdout: str = "", stderr: str = "", returncode: int = 0):
    return SimpleNamespace(stdout=stdout, stderr=stderr, returncode=returncode)


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def _patch_claude_subprocess(monkeypatch):
    """Default: claude_agents_json returns ({}, []) silently — no live data, no WARN.

    Tests that exercise the shellout-failure WARN path patch _subprocess_run
    directly instead of leaning on this default; that keeps the assertions
    here focused on CLI plumbing rather than provider plumbing.
    """
    from fno.agents.providers import claude as claude_mod

    def _fake(timeout=3.0):  # noqa: ARG001
        return {}, []

    monkeypatch.setattr(claude_mod, "claude_agents_json", _fake)
    return claude_mod


# --- `fno agents list` -------------------------------------------------------


def test_list_empty_registry_emits_json_with_zero_count(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """AC1-EDGE — empty registry returns valid empty shape."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["list", "--json"])

    assert result.exit_code == 0, result.output
    parsed = json.loads(result.output)
    assert parsed["count"] == 0
    assert parsed["agents"] == []


def test_list_corrupt_registry_exits_1(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """AC1-ERR — corrupt JSON exits 1, stderr names file path + parser error."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths

    target = paths.agents_registry_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("{garbage", encoding="utf-8")

    from fno.agents.cli import agents_app

    # CliRunner separates stdout/stderr only when mix_stderr=False is used;
    # by default they're combined. Either way, the parser error surfaces.
    result = runner.invoke(agents_app, ["list", "--json"])

    assert result.exit_code == 1
    assert str(target) in result.output


def test_list_populated_table_renders_in_tty(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """AC1-HP — 3 entries render under a TTY."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="alpha"),
            _codex(name="bravo"),
            _claude(name="charlie", status="orphaned"),
        ]
    )

    from fno.agents.cli import agents_app

    # CliRunner's stdout is not a real TTY, so the JSON default would
    # kick in. Force the table path via monkeypatching isatty.
    monkeypatch.setattr("sys.stdout.isatty", lambda: True, raising=False)
    result = runner.invoke(agents_app, ["list"])

    assert result.exit_code == 0, result.output
    assert "alpha" in result.output
    assert "bravo" in result.output
    assert "charlie" in result.output


def test_list_invalid_status_value_exits_2_with_allowed_values(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """AC3-ERR — invalid --status exits 2 with allowed-values list."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["list", "--status", "invalid-value"])

    assert result.exit_code == 2
    # Typer puts the allowed-values list in the usage error.
    assert "live" in result.output.lower()
    assert "orphaned" in result.output.lower()


def test_list_filter_by_status_orphaned(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="alive", status="live"),
            _claude(
                name="dead",
                status="orphaned",
                short_id="def67890",
            ),
        ]
    )

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["list", "--status", "orphaned", "--json"])

    assert result.exit_code == 0, result.output
    parsed = json.loads(result.output)
    assert parsed["count"] == 1
    assert parsed["agents"][0]["name"] == "dead"


def test_list_non_tty_defaults_to_json(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """Locked Decision 4 — non-TTY stdout defaults to JSON."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])

    from fno.agents.cli import agents_app

    monkeypatch.setattr("sys.stdout.isatty", lambda: False, raising=False)
    result = runner.invoke(agents_app, ["list"])

    assert result.exit_code == 0, result.output
    parsed = json.loads(result.output)
    assert parsed["count"] == 1


# --- `fno agents logs <name>` -----------------------------------------------


def test_logs_unknown_name_exits_13(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """AC2-ERR — unknown name exits 13 with clear message."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "ghost-agent"])

    assert result.exit_code == 13
    assert "agent not found: ghost-agent" in result.output


def test_logs_codex_without_tee_exits_13_with_honest_message(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """AC2-EDGE — a codex agent whose log file is absent exits 13 with an honest
    "no log file" message (ab-65c3e60d), not the stale "ships in Phase 3 US4"
    stub that made codex log retrieval look unimplemented (it is implemented)."""
    use_tmpdir(monkeypatch, tmp_path)
    absent = tmp_path / "nonexistent" / "output.jsonl"
    write_registry([_codex(name="worker-Y", log_path=str(absent))])

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "worker-Y"])

    assert result.exit_code == 13
    assert "us4" not in result.output.lower()
    assert "no logs for codex agent worker-Y" in result.output
    assert f"no log file at {absent}" in result.output


def test_logs_claude_raw_passthrough(
    tmp_path, monkeypatch, runner
):
    """AC2-HP — claude logs passes raw output through with claude's exit code."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])

    from fno.agents.providers import claude as claude_mod

    def _fake(argv, **kwargs):  # noqa: ARG001
        assert argv[:2] == ["claude", "logs"]
        assert "abc12345" in argv
        return _fake_completed(stdout="raw line 1\nraw line 2\n", returncode=0)

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    from fno.agents.cli import agents_app

    # Default --tail=100 → effective_tail=100; with only 2 lines we still
    # see both.
    result = runner.invoke(agents_app, ["logs", "alpha"])

    assert result.exit_code == 0, result.output
    assert "raw line 1" in result.output
    assert "raw line 2" in result.output


def test_logs_claude_tail_n_slices_output(tmp_path, monkeypatch, runner):
    """AC2-UI — --tail N slices the last N lines."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])

    from fno.agents.providers import claude as claude_mod

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(
            stdout="\n".join(f"line{i}" for i in range(1, 21)) + "\n",
            returncode=0,
        )

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "alpha", "--tail", "3"])

    assert result.exit_code == 0
    lines = [ln for ln in result.output.splitlines() if ln.startswith("line")]
    assert lines == ["line18", "line19", "line20"]


def test_logs_negative_tail_exits_2(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """Boundary — --tail -5 rejected with exit 2."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "alpha", "--tail", "-5"])

    assert result.exit_code == 2
    assert "tail" in result.output.lower()


def test_logs_tail_zero_emits_nothing(
    tmp_path, monkeypatch, runner, _patch_claude_subprocess
):
    """Boundary — --tail 0 emits empty output and exit 0."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])

    from fno.agents.providers import claude as claude_mod

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout="line1\nline2\n", returncode=0)

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "alpha", "--tail", "0"])

    assert result.exit_code == 0
    # Should not contain "line1" / "line2" from stub.
    assert "line1" not in result.output
    assert "line2" not in result.output


def test_logs_claude_propagates_non_zero_exit(tmp_path, monkeypatch, runner):
    """AC2-HP variant — claude's non-zero exit propagates."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])

    from fno.agents.providers import claude as claude_mod

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout="", stderr="claude: not found\n", returncode=17)

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "alpha"])

    assert result.exit_code == 17


def test_logs_claude_with_json_flag_emits_warn_and_raw_passthrough(
    tmp_path, monkeypatch, runner
):
    """The `--json` gap for Claude logs surfaces as a WARN-prefixed line on stderr.

    This is the integration-test-analyzer's Finding 1 — without this
    test the stderr message at read.py could silently mutate or be
    deleted and every other test would still pass.
    """
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])

    from fno.agents.providers import claude as claude_mod

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout="claude raw output\n", returncode=0)

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "alpha", "--json"])

    assert result.exit_code == 0
    # The CLI's "WARN: " prefix from the result.warnings loop applies.
    assert "warn:" in result.output.lower()
    assert "json output for claude logs not implemented" in result.output.lower()
    # Raw passthrough still happens.
    assert "claude raw output" in result.output


def test_logs_claude_entry_missing_short_id_exits_1(tmp_path, monkeypatch, runner):
    """Missing short_id is a data-integrity error (exit 1), not name-not-found (exit 13).

    This is the code-reviewer Finding 3 — exit 13 was overloaded.
    """
    use_tmpdir(monkeypatch, tmp_path)
    # Construct a registry entry with no short_id (data drift).
    write_registry([_claude(name="alpha", short_id="")])

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["logs", "alpha"])

    assert result.exit_code == 1
    assert "short id" in result.output.lower() or "short_id" in result.output.lower()


def test_agent_status_filter_in_sync_with_known_statuses():
    """The cli.py import-time assertion has already run; this test guards the symmetry.

    If KNOWN_STATUSES gains a value without AgentStatusFilter following,
    cli.py fails to import and this test (along with everything else)
    breaks. Asserting the symmetry from a unit test makes the failure
    mode obvious in pytest output.
    """
    from fno.agents.cli import AgentStatusFilter
    from fno.agents.registry import KNOWN_STATUSES

    assert {m.value for m in AgentStatusFilter} == set(KNOWN_STATUSES)
