"""Per-task readiness in the do-phase orchestrator.

``ready_tasks`` is the work-stealing query behind ``--ready`` and the
``get_next_wave`` compatibility shim; the ``--ready`` CLI joins STATE.md
completions with cross-session ``done`` task rows from a bound node.
"""
from __future__ import annotations

import importlib.util
import json
import os
import stat
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]


def _load_orch():
    spec = importlib.util.spec_from_file_location(
        "do_orchestrator", REPO / "skills/execute/orchestrator.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _strategy(mod, waves, blocked_by=None):
    return mod.ExecutionStrategy(
        execution_mode="parallel",
        waves=[mod.Wave(number=n, mode="sequential", tasks=tasks, reason="")
               for n, tasks in waves],
        blocked_by=dict(blocked_by or {}),
    )


PLAN_MD = """---
title: example spec
status: ready
---

# Example

## Execution Strategy

```yaml
execution_mode: parallel
waves:
  - wave: 1
    mode: sequential
    tasks: ['1.1', '1.2']
  - wave: 2
    mode: sequential
    tasks: ['2.1']
tasks:
  - id: '1.1'
    title: Setup
  - id: '1.2'
    title: Peer setup
  - id: '2.1'
    title: Follow-up
    blocked_by: ['1.1']
```
"""


class TestReadyTasks:
    def test_declared_edge_floats_ready_task(self):
        # AC2-HP: 1.1 done, sibling 1.2 open, 2.1 blocks only on 1.1 ->
        # 2.1 is ready beside the straggler and the shim still answers
        # "wave 1", the wave of the first ready entry.
        orch = _load_orch()
        strategy = _strategy(orch, [(1, ["1.1", "1.2"]), (2, ["2.1"])],
                             {"2.1": ["1.1"]})
        assert orch.ready_tasks(strategy, {"1.1"}, set()) == ["1.2", "2.1"]
        assert orch.get_next_wave(strategy, ["1.1"]).number == 1

    def test_edgeless_plan_matches_legacy_scheduling(self):
        # AC2-ERR: no declared edges; after wave 1 completes the ready set
        # is exactly wave 2 in declared order, and a claimed task is absent.
        orch = _load_orch()
        strategy = _strategy(orch, [(1, ["1.1", "1.2"]), (2, ["2.1", "2.2"])])
        assert orch.ready_tasks(strategy, [], set()) == ["1.1", "1.2"]
        assert orch.ready_tasks(strategy, ["1.1", "1.2"], []) == ["2.1", "2.2"]
        assert orch.ready_tasks(strategy, ["1.1", "1.2"], {"2.1"}) == ["2.2"]

    def test_derived_blockers_reach_one_wave_back_only(self):
        orch = _load_orch()
        strategy = _strategy(orch, [(1, ["1.1"]), (2, ["2.1"]), (3, ["3.1"])])
        assert orch.effective_blockers(strategy, "1.1") == set()
        assert orch.effective_blockers(strategy, "2.1") == {"1.1"}
        assert orch.effective_blockers(strategy, "3.1") == {"2.1"}

    def test_explicit_empty_blockers_override_wave_derivation(self):
        orch = _load_orch()
        strategy = _strategy(
            orch,
            [(1, ["1.1"]), (2, ["2.1"])],
            {"2.1": []},
        )
        assert orch.effective_blockers(strategy, "2.1") == set()
        assert orch.ready_tasks(strategy, set(), set()) == ["1.1", "2.1"]

    def test_shim_returns_none_when_all_complete(self):
        orch = _load_orch()
        strategy = _strategy(orch, [(1, ["1.1"]), (2, ["2.1"])])
        assert orch.get_next_wave(strategy, ["1.1", "2.1"]) is None


def _run_cli(args, env_path, cwd):
    env = dict(os.environ)
    env["PYTHONPATH"] = f"{REPO / 'cli' / 'src'}{os.pathsep}{env.get('PYTHONPATH', '')}"
    env["PATH"] = f"{env_path}{os.pathsep}{env['PATH']}"
    return subprocess.run(
        [sys.executable, str(REPO / "skills/execute/orchestrator.py"), *args],
        capture_output=True, text=True, env=env, cwd=cwd,
    )


def _write_fixture(tmp_path: Path, plan_md: str = PLAN_MD):
    plan = tmp_path / "plan.md"
    plan.write_text(plan_md)
    state = tmp_path / "STATE.md"
    state.write_text("# Progress\n\n- [x] 1.1: Setup\n- [ ] 1.2: Peer setup\n")
    return plan, state


class TestReadyCli:
    def test_state_only_read(self, tmp_path):
        # AC3-HP: 2.1 blocks only on 1.1 and STATE.md marks 1.1 [x]; the
        # stdout JSON lists both the freed 2.1 and the open sibling 1.2.
        plan, state = _write_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.2", "2.1"]
        assert out["completed"] == ["1.1"]
        assert out["claimed"] == []

    def test_node_rows_join_completed_and_claimed(self, tmp_path):
        # AC3-ERR: a peer holds 1.2 in_progress per its task row -> it lands
        # under claimed, never under ready, even though STATE.md stays quiet.
        plan, state = _write_fixture(tmp_path)
        bin_dir = tmp_path / "bin"
        bin_dir.mkdir()
        fake_fno = bin_dir / "fno"
        fake_fno.write_text(
            "#!/bin/sh\ncat <<'EOF'\n"
            '{"node": "x-demo", "tasks": ['
            '{"id": "1.2", "status": "in_progress", "owner": "peer"},'
            '{"id": "2.1", "status": "pending", "owner": null}'
            "]}\nEOF\n"
        )
        fake_fno.chmod(fake_fno.stat().st_mode | stat.S_IEXEC)
        proc = _run_cli([str(plan), "--ready", "--state", str(state),
                         "--node", "x-demo"],
                        env_path=str(bin_dir), cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["claimed"] == ["1.2"]
        assert out["completed"] == ["1.1"]
        # The claimed sibling never dispatches; the freed dependent still does.
        assert out["ready"] == ["2.1"]

    def test_stale_task_claim_is_reoffered(self, tmp_path):
        plan, state = _write_fixture(tmp_path)
        bin_dir = tmp_path / "bin"
        bin_dir.mkdir()
        fake_fno = bin_dir / "fno"
        fake_fno.write_text(
            "#!/bin/sh\n"
            "if [ \"$1\" = backlog ]; then\n"
            "  cat <<'EOF'\n"
            '{"node": "x-demo", "tasks": ['
            '{"id": "1.2", "status": "in_progress", "owner": "peer"},'
            '{"id": "2.1", "status": "pending", "owner": null}'
            "]}\nEOF\n"
            "elif [ \"$1\" = agents ]; then\n"
            "  echo '{\"key\":\"task:x-demo:1.2\",\"state\":\"stale\"}'\n"
            "fi\n"
        )
        fake_fno.chmod(fake_fno.stat().st_mode | stat.S_IEXEC)
        proc = _run_cli(
            [str(plan), "--ready", "--state", str(state), "--node", "x-demo"],
            env_path=str(bin_dir),
            cwd=tmp_path,
        )
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.2", "2.1"]
        assert out["claimed"] == []

    def test_unknown_task_status_is_blocked(self, tmp_path):
        plan, state = _write_fixture(tmp_path)
        bin_dir = tmp_path / "bin"
        bin_dir.mkdir()
        fake_fno = bin_dir / "fno"
        fake_fno.write_text(
            "#!/bin/sh\ncat <<'EOF'\n"
            '{"node": "x-demo", "tasks": ['
            '{"id": "1.2", "status": "cancelled", "owner": null},'
            '{"id": "2.1", "status": "pending", "owner": null}'
            "]}\nEOF\n"
        )
        fake_fno.chmod(fake_fno.stat().st_mode | stat.S_IEXEC)
        proc = _run_cli(
            [str(plan), "--ready", "--state", str(state), "--node", "x-demo"],
            env_path=str(bin_dir),
            cwd=tmp_path,
        )
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["2.1"]
        assert out["claimed"] == []
        assert out["blocked"] == ["1.2"]

    def test_failed_node_read_degrades_to_state(self, tmp_path):
        plan, state = _write_fixture(tmp_path)
        bin_dir = tmp_path / "bin"
        bin_dir.mkdir()
        failing = bin_dir / "fno"
        failing.write_text("#!/bin/sh\necho boom >&2\nexit 1\n")
        failing.chmod(failing.stat().st_mode | stat.S_IEXEC)
        proc = _run_cli([str(plan), "--ready", "--state", str(state),
                         "--node", "x-demo"],
                        env_path=str(bin_dir), cwd=tmp_path)
        assert proc.returncode == 0
        assert "rejected (non-fatal)" in proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.2", "2.1"]
        assert out["claimed"] == []

    def test_invalid_edges_refuse_the_run(self, tmp_path):
        bad_plan = PLAN_MD.replace("blocked_by: ['1.1']", "blocked_by: ['9.9']")
        plan, state = _write_fixture(tmp_path, bad_plan)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode != 0
        assert "blocked_reason=plan_task_edges_invalid" in proc.stderr
        assert "task 2.1 blocked_by unknown task 9.9" in proc.stderr
