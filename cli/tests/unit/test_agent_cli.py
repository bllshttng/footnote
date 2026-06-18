"""Tests for the relocated self-introspection commands: fno whoami / fno status.

Covers the operating-stack summary (whoami) and gate/events view (status),
plus the read-only invariant (paired-state hash diff). These were formerly
`fno agent whoami` / `fno agent status`; the `fno agent` (singular) namespace
was retired in ab-12dd2a5d (suggest/capabilities trimmed). Commands are invoked
through the real top-level `fno` app so the lazy registration is exercised too.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import List

import pytest
from typer.testing import CliRunner

from fno.agent.cli import _tail_events
from fno.cli import app

FIXTURES = Path(__file__).parent / "fixtures" / "agent"


def _file_hash(path: Path) -> str:
    return hashlib.md5(path.read_bytes()).hexdigest() if path.exists() else "(absent)"


def _make_workspace(tmp_path: Path, *, target: bool = False, walker: bool = False,
                    fleet: bool = False, malformed: bool = False) -> Path:
    project = tmp_path / "project"
    fno = project / ".fno"
    fno.mkdir(parents=True)
    if target and not malformed:
        (fno / "target-state.md").write_text(
            (FIXTURES / "target-state.md").read_text()
        )
    if malformed:
        (fno / "target-state.md").write_text(
            (FIXTURES / "malformed-state.md").read_text()
        )
    if walker:
        (fno / "megawalk-state.md").write_text(
            (FIXTURES / "megawalk-state.md").read_text()
        )
    if fleet:
        fleet_root = tmp_path / "fake_home" / ".fno" / "fleet" / "fleet-fixture-001"
        fleet_root.mkdir(parents=True)
        body = (FIXTURES / "fleet-mission.md").read_text().replace(
            "__PROJECT_ROOT__", str(project.resolve())
        )
        (fleet_root / "00-INDEX.md").write_text(body)
    return project


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _state_files(project: Path) -> List[Path]:
    return [
        project / ".fno" / "target-state.md",
        project / ".fno" / "megawalk-state.md",
        project / ".fno" / "session-state.md",
    ]


def _state_hashes(project: Path) -> dict:
    return {p.name: _file_hash(p) for p in _state_files(project)}


def _invoke(runner: CliRunner, project: Path, monkeypatch, *args, env_home: Path = None):
    """Invoke the real top-level `fno` app. `args` is the verb followed by any
    command-level flags (flags come AFTER the verb now that the options live on
    the command, not on a group callback)."""
    monkeypatch.chdir(project)
    if env_home is not None:
        monkeypatch.setenv("HOME", str(env_home))
    return runner.invoke(app, list(args))


# --- whoami --------------------------------------------------------------


class TestWhoami:
    def test_ac1_hp_full_stack_visible(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True, walker=True, fleet=True)
        result = _invoke(
            runner, project, monkeypatch, "whoami",
            env_home=tmp_path / "fake_home",
        )
        assert result.exit_code == 0, result.stdout + result.stderr
        out = result.stdout
        assert "project:" in out
        assert "fleet-fixture-001" in out
        assert "walker:" in out
        assert "20260512T010101Z-99999-fixaaa" in out  # target session id
        assert "provider:" in out

    def test_ac2_err_malformed_target_state_fails_fast(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, malformed=True)
        result = _invoke(runner, project, monkeypatch, "whoami")
        assert result.exit_code == 2
        assert "target-state.md" in result.stderr
        assert "malformed" in result.stderr.lower()

    def test_ac3_ui_json_mode_is_structured(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True, walker=True)
        result = _invoke(runner, project, monkeypatch, "whoami", "--json")
        assert result.exit_code == 0, result.stdout + result.stderr
        payload = json.loads(result.stdout)
        # Absent fleet serializes as null, not as a missing key.
        assert "fleet" in payload and payload["fleet"] is None
        assert payload["walker"] is not None
        assert payload["session"] is not None
        assert payload["provider"]

    @pytest.mark.parametrize("global_flag", ["--json", "-J"])
    def test_global_json_flag_honored(self, tmp_path, runner, monkeypatch, global_flag):
        """Regression (codex P2 on PR #500): the root callback's global -J/--json
        (passed BEFORE the verb: `fno -J whoami`) must also produce JSON, not
        just the command-local `fno whoami --json`. Mirrors the `fno review`
        merge convention."""
        project = _make_workspace(tmp_path, target=True, walker=True)
        result = _invoke(runner, project, monkeypatch, global_flag, "whoami")
        assert result.exit_code == 0, result.stdout + result.stderr
        payload = json.loads(result.stdout)
        assert payload["session"] is not None
        # status honors it too
        result2 = _invoke(runner, project, monkeypatch, global_flag, "status")
        assert result2.exit_code == 0, result2.stdout + result2.stderr
        assert "events_tail" in json.loads(result2.stdout)

    def test_json_mode_serializes_datetime_frontmatter(self, tmp_path, runner, monkeypatch):
        """Regression: yaml.safe_load turns an unquoted ISO timestamp in the
        manifest into a datetime, which json.dumps cannot serialize. --json
        must emit isoformat strings, not crash. (Surfaced making whoami a
        top-level command run against real manifests; ab-12dd2a5d.)"""
        project = tmp_path / "project"
        fno = project / ".fno"
        fno.mkdir(parents=True)
        (fno / "target-state.md").write_text(
            "---\n"
            "session_id: 20260611T000000Z-00000-dtfix\n"
            "created_at: 2026-06-11T13:27:56Z\n"   # unquoted -> parsed to datetime
            "input: \"demo\"\n"
            "---\n# manifest\n"
        )
        result = _invoke(runner, project, monkeypatch, "whoami", "--json")
        assert result.exit_code == 0, result.stdout + result.stderr
        payload = json.loads(result.stdout)
        created = payload["session"]["raw"]["created_at"]
        assert isinstance(created, str)
        assert created.startswith("2026-06-11T13:27:56")

    def test_ac4_edge_no_state_at_all(self, tmp_path, runner, monkeypatch):
        project = tmp_path / "empty"
        project.mkdir()
        result = _invoke(runner, project, monkeypatch, "whoami")
        assert result.exit_code == 0
        assert "project:" in result.stdout
        assert "provider:" in result.stdout
        assert "session:" not in result.stdout
        assert "walker:" not in result.stdout
        assert "fleet:" not in result.stdout

    def test_ac5_fr_dual_states_picks_target_warns(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True)
        # Add session-state.md alongside target-state.md
        (project / ".fno" / "session-state.md").write_text(
            (FIXTURES / "session-state-think.md").read_text()
        )
        result = _invoke(runner, project, monkeypatch, "whoami")
        assert result.exit_code == 0
        assert "(target)" in result.stdout
        assert "warn:" in result.stderr
        assert "both" in result.stderr

    def test_no_walker_flag_suppresses_walker_layer(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True, walker=True)
        result = _invoke(runner, project, monkeypatch, "whoami", "--no-walker")
        assert result.exit_code == 0
        assert "walker:" not in result.stdout

    def test_no_fleet_flag_suppresses_fleet_layer(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True, walker=True, fleet=True)
        result = _invoke(
            runner, project, monkeypatch, "whoami", "--no-fleet",
            env_home=tmp_path / "fake_home",
        )
        assert result.exit_code == 0
        assert "fleet:" not in result.stdout
        assert "walker:" in result.stdout


# --- status --------------------------------------------------------------


class TestStatus:
    def test_ac1_hp_gate_satisfaction_reported(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True)
        result = _invoke(runner, project, monkeypatch, "status")
        assert result.exit_code == 0, result.stdout + result.stderr
        assert "gates:" in result.stdout
        assert "quality_check_passed" in result.stdout

    def test_ac2_err_events_jsonl_unreadable_degrades(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True)
        # Create a directory where events.jsonl should be a file -> read fails open()
        (project / ".fno" / "events.jsonl").mkdir()
        result = _invoke(runner, project, monkeypatch, "status")
        # rc=0 (degraded), warning emitted to stderr, events block omitted.
        assert result.exit_code == 0
        assert "events (last" not in result.stdout

    def test_ac3_ui_inconsistencies_flagged_inline(self, tmp_path, runner, monkeypatch):
        # target fixture has pr_number=1234 + external_review_passed missing/false.
        project = _make_workspace(tmp_path, target=True)
        # Force external_review_passed: false explicitly to trigger the rule.
        state_path = project / ".fno" / "target-state.md"
        state_path.write_text(
            "---\n"
            "status: IN_PROGRESS\n"
            "session_id: 20260512T010101Z-99999-fixaaa\n"
            "current_phase: external\n"
            "pr_number: 1234\n"
            "external_review_passed: false\n"
            "---\n# fixture\n"
        )
        result = _invoke(runner, project, monkeypatch, "status")
        assert result.exit_code == 0
        assert "inconsistencies:" in result.stdout
        assert "WARNING" in result.stdout
        assert "/pr check" in result.stdout

    def test_ac4_edge_events_tail_bounded_constant_time(self, tmp_path, runner, monkeypatch):
        """Tail is constant-time wrt file size: time for 50MB <= 3x time for 5MB.

        Time-bound assertions on shared CI runners are flaky; instead this test
        asserts the structural property the bounded tail is meant to provide.
        """
        project = _make_workspace(tmp_path, target=True)
        events = project / ".fno" / "events.jsonl"
        line = json.dumps({"ts": "2026-05-12T00:00:00Z", "type": "noise",
                          "data": {"x": "padding" * 50}}) + "\n"
        import time

        def _write_and_time(target_bytes: int) -> float:
            events.write_text("")
            with events.open("w") as f:
                written = 0
                while written < target_bytes:
                    f.write(line)
                    written += len(line)
            start = time.perf_counter()
            tail = _tail_events(events)
            elapsed = time.perf_counter() - start
            assert len(tail) <= 10
            return elapsed

        small = _write_and_time(5 * 1024 * 1024)   # 5MB
        large = _write_and_time(50 * 1024 * 1024)  # 50MB
        # Ratio assertion: if _tail_events read the whole file, large/small
        # would be ~10x. A truly bounded seek-from-end read is O(1) wrt file
        # size, so the ratio should be near 1. Allow 3x to absorb FS jitter.
        assert large < small * 3 + 0.1, (
            f"tail not constant-time: 5MB={small*1000:.0f}ms 50MB={large*1000:.0f}ms"
        )

    def test_ac5_fr_mid_append_event_skipped_silently(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True)
        events = project / ".fno" / "events.jsonl"
        events.write_text(
            json.dumps({"ts": "T1", "type": "phase_init"}) + "\n"
            + json.dumps({"ts": "T2", "type": "phase_transition"}) + "\n"
            + '{"ts":"T3","type":"phase_truncated_marker_xyz'  # partial line
        )
        result = _invoke(runner, project, monkeypatch, "status")
        assert result.exit_code == 0
        assert "phase_init" in result.stdout
        assert "phase_transition" in result.stdout
        # Critical: the partial line must NOT appear in any form. The
        # marker is distinctive enough that an accidental render would
        # surface it.
        assert "phase_truncated_marker_xyz" not in result.stdout

    def test_json_mode_includes_events_and_inconsistencies(self, tmp_path, runner, monkeypatch):
        project = _make_workspace(tmp_path, target=True)
        result = _invoke(runner, project, monkeypatch, "status", "--json")
        assert result.exit_code == 0
        payload = json.loads(result.stdout)
        assert "events_tail" in payload
        assert "inconsistencies" in payload


# --- retired namespace ---------------------------------------------------


class TestAgentNamespaceRetired:
    """The `fno agent` (singular) namespace is gone (AC2-EDGE): the verbs are
    absent (clean usage error), not a silent wrong result."""

    @pytest.mark.parametrize("verb", ["whoami", "status", "suggest", "capabilities"])
    def test_abi_agent_verb_is_absent(self, tmp_path, runner, monkeypatch, verb):
        project = _make_workspace(tmp_path, target=True)
        result = _invoke(runner, project, monkeypatch, "agent", verb)
        assert result.exit_code != 0
        # `agents` (plural mesh) still exists, so the suggestion may point there;
        # the point is `fno agent <verb>` does not silently succeed.
        combined = result.stdout + result.stderr
        assert "No such command 'agent'" in combined or "Usage:" in combined


# --- read-only invariant -------------------------------------------------


class TestReadOnlyInvariant:
    """Tests that neither command mutates state files (the design's hard rule)."""

    @pytest.mark.parametrize("verb", ["whoami", "status"])
    def test_paired_state_hash_unchanged(
        self, tmp_path, runner, monkeypatch, verb
    ):
        project = _make_workspace(tmp_path, target=True, walker=True)
        before = _state_hashes(project)
        for _ in range(3):  # repeated invocation should still not mutate
            result = _invoke(runner, project, monkeypatch, verb)
            assert result.exit_code in (0, 2), result.stdout + result.stderr
        after = _state_hashes(project)
        assert before == after, f"{verb} mutated state: {before} -> {after}"
