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

import pytest

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


# A parallel wave at task grain: 1.1 and 1.2 are disjoint, 1.3 shares 1.1's
# file, and 1.4 states no file list at all (unevaluated).
PARALLEL_PLAN_MD = """---
title: parallel fixture
status: ready
difficulty: medium
---

# Parallel fixture

## Execution Strategy

```yaml
execution_mode: parallel
waves:
  - wave: 1
    mode: parallel
    tasks: ['1.1', '1.2', '1.3', '1.4']
tasks:
  - id: '1.1'
    title: Touch a
  - id: '1.2'
    title: Touch b
  - id: '1.3'
    title: Also touches a
  - id: '1.4'
    title: No file list
```

### Task 1.1: Touch a
**Files:**
- Modify: `src/a.py`

### Task 1.2: Touch b
**Files:**
- Modify: `src/b.py`

### Task 1.3: Also touches a
**Files:**
- Modify: `src/a.py`
"""


def _write_parallel_fixture(tmp_path: Path, plan_md: str = PARALLEL_PLAN_MD):
    plan = tmp_path / "plan.md"
    plan.write_text(plan_md)
    state = tmp_path / "STATE.md"
    state.write_text("# Progress\n")
    return plan, state


# Three banded waves, all three tasks ready at once: the later waves declare
# explicit empty blockers so nothing holds them back (the band axis, not the
# wave order, must decide the split).
BANDED_PLAN_MD = """---
title: banded fixture
status: ready
---

# Banded fixture

## Execution Strategy

```yaml
execution_mode: parallel
waves:
  - wave: 1
    mode: sequential
    difficulty: high
    tasks: ['1.1']
  - wave: 2
    mode: sequential
    difficulty: low
    tasks: ['1.2']
  - wave: 3
    mode: sequential
    difficulty: medium
    tasks: ['1.3']
tasks:
  - id: '1.1'
    title: Heavy
  - id: '1.2'
    title: Light
    blocked_by: []
  - id: '1.3'
    title: Middle
    blocked_by: []
```
"""


def _write_banded_fixture(tmp_path: Path):
    return _write_parallel_fixture(tmp_path, BANDED_PLAN_MD)


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

    @pytest.mark.parametrize("payload", ["[]", "null", '{"tasks": null}'])
    def test_non_object_node_read_degrades_to_state(self, tmp_path, payload):
        plan, state = _write_fixture(tmp_path)
        bin_dir = tmp_path / "bin"
        bin_dir.mkdir()
        fake_fno = bin_dir / "fno"
        fake_fno.write_text(f"#!/bin/sh\nprintf '%s\\n' {payload!r}\n")
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
        assert "unreadable task rows" in proc.stderr

    def test_invalid_edges_refuse_the_run(self, tmp_path):
        bad_plan = PLAN_MD.replace("blocked_by: ['1.1']", "blocked_by: ['9.9']")
        plan, state = _write_fixture(tmp_path, bad_plan)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode != 0
        assert "blocked_reason=plan_task_edges_invalid" in proc.stderr
        assert "task 2.1 blocked_by unknown task 9.9" in proc.stderr


class TestPartitionEdgesReady:
    def test_overlapping_task_waits_for_its_group_mate(self, tmp_path):
        # AC2-HP: 1.1 and 1.2 are disjoint and dispatch now; 1.3 shares
        # 1.1's file and is held with 1.1 as its blocker.
        plan, state = _write_parallel_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.1", "1.2"]
        assert out["blocked_on"] == {"1.3": ["1.1"], "1.4": ["1.1", "1.2", "1.3"]}

    def test_unevaluated_task_waits_for_every_evaluated_sibling(self, tmp_path):
        # AC2-ERR: 1.4 states no file list, so it runs only after every
        # evaluated task in the wave is complete.
        plan, state = _write_parallel_fixture(tmp_path)
        state.write_text("# Progress\n\n- [x] 1.1: a\n- [x] 1.2: b\n- [x] 1.3: a again\n")
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.4"]
        assert out["blocked_on"] == {}

    def test_wave_decision_records_unevaluated(self, tmp_path):
        plan, state = _write_parallel_fixture(tmp_path)
        proc = _run_cli([str(plan), "--wave-decision", "1"],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        decision = json.loads(proc.stdout)
        assert decision["effective_mode"] == "parallel"
        assert decision["conflicts"]["unevaluated"] == ["1.4"]
        assert ["1.1", "1.3"] in decision["conflicts"]["groups"]

    def test_declared_edges_union_with_derived_never_replace(self, tmp_path):
        # 1.3 declares blocked_by [1.2] AND shares 1.1's file: both edges hold.
        plan_md = PARALLEL_PLAN_MD.replace(
            "  - id: '1.3'\n    title: Also touches a",
            "  - id: '1.3'\n    title: Also touches a\n    blocked_by: ['1.2']",
        )
        plan, state = _write_parallel_fixture(tmp_path, plan_md)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.1", "1.2"]
        assert out["blocked_on"]["1.3"] == ["1.1", "1.2"]

    def test_derived_edge_cannot_float_task_past_earlier_wave(self, tmp_path):
        # Wave 2 tasks 2.1/2.2 share a file, so 2.2 derives an edge on 2.1.
        # That derived edge must UNION with the previous-wave inheritance:
        # both still wait for all of wave 1. If the union replaced the
        # inheritance, 2.1 would float ready with no blockers at all.
        wave2_plan = PARALLEL_PLAN_MD.replace(
            "    tasks: ['1.1', '1.2', '1.3', '1.4']\ntasks:",
            "    tasks: ['1.1', '1.2', '1.3', '1.4']\n"
            "  - wave: 2\n    mode: parallel\n    tasks: ['2.1', '2.2']\ntasks:",
        ).replace(
            "  - id: '1.4'\n    title: No file list",
            "  - id: '1.4'\n    title: No file list\n"
            "  - id: '2.1'\n    title: Wave two, first\n"
            "  - id: '2.2'\n    title: Wave two, second",
        ).replace(
            "### Task 1.3: Also touches a\n**Files:**\n- Modify: `src/a.py`",
            "### Task 1.3: Also touches a\n**Files:**\n- Modify: `src/a.py`\n\n"
            "### Task 2.1: Wave two, first\n**Files:**\n- Modify: `src/two.py`\n\n"
            "### Task 2.2: Wave two, second\n**Files:**\n- Modify: `src/two.py`",
        )
        plan, state = _write_parallel_fixture(tmp_path, wave2_plan)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.1", "1.2"]
        assert out["blocked_on"] == {
            "1.3": ["1.1"],
            "1.4": ["1.1", "1.2", "1.3"],
            "2.1": ["1.1", "1.2", "1.3", "1.4"],
            "2.2": ["1.1", "1.2", "1.3", "1.4", "2.1"],
        }

    def test_ready_reports_bands(self, tmp_path):
        # The plan's frontmatter band seeds every wave; an explicit per-wave
        # band wins (AC4-HP's --ready half).
        plan_md = PARALLEL_PLAN_MD.replace(
            "  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2', '1.3', '1.4']",
            "  - wave: 1\n    mode: parallel\n    difficulty: high\n"
            "    tasks: ['1.1', '1.2', '1.3', '1.4']",
        )
        plan, state = _write_parallel_fixture(tmp_path, plan_md)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["bands"] == {
            "1.1": "high", "1.2": "high", "1.3": "high", "1.4": "high",
        }

    def test_bands_fall_back_to_frontmatter_band(self, tmp_path):
        plan, state = _write_parallel_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["bands"] == {
            "1.1": "medium", "1.2": "medium", "1.3": "medium", "1.4": "medium",
        }

    def test_bands_normalize_case(self, tmp_path):
        # The validator accepts any casing; the band axis is lowercase, so a
        # plan stamped "Medium" must report "medium", not pass "Medium" on to
        # band-resolving consumers.
        plan_md = PARALLEL_PLAN_MD.replace("difficulty: medium", "difficulty: Medium")
        plan, state = _write_parallel_fixture(tmp_path, plan_md)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["bands"]["1.1"] == "medium"


class TestReadyBandFilter:
    def test_band_filters_and_orders_the_pull(self, tmp_path):
        # AC1-HP: a medium worker takes the medium task first, then the low
        # one; the high task surfaces under above_band, shown not offered.
        plan, state = _write_banded_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state),
                         "--band", "medium"],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.3", "1.2"]
        assert out["above_band"] == ["1.1"]

    def test_high_band_takes_everything(self, tmp_path):
        plan, state = _write_banded_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state),
                         "--band", "high"],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.1", "1.3", "1.2"]
        assert out["above_band"] == []

    def test_no_band_keeps_payload_unchanged(self, tmp_path):
        # AC1-ERR: without --band the receipt is today's shape - wave order,
        # and no above_band key a lone-target consumer could trip on.
        plan, state = _write_banded_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.1", "1.2", "1.3"]
        assert "above_band" not in out

    def test_band_normalizes_case(self, tmp_path):
        # The band axis is lowercase; "Medium" rides like "medium" (same
        # split as AC1-HP, spelled with a capital).
        plan, state = _write_banded_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state),
                         "--band", "Medium"],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.3", "1.2"]
        assert out["above_band"] == ["1.1"]

    def test_illegal_band_refuses_the_run(self, tmp_path):
        # A typo reading as unfiltered would silently undo the partition.
        plan, state = _write_banded_fixture(tmp_path)
        proc = _run_cli([str(plan), "--ready", "--state", str(state),
                         "--band", "huge"],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode != 0
        assert "huge" in (proc.stderr + proc.stdout)

    def test_canonical_surface_lists_drive_the_partition(self, tmp_path):
        # A plan that declares ownership only through Execution Strategy
        # `surface:` lists has no `### Task` prose; the partition must read
        # its file targets from the parsed task blocks, not find them all
        # unevaluated and add no edges.
        canonical = """---
title: canonical surfaces
status: ready
---

# Canonical surfaces

## Execution Strategy

```yaml
execution_mode: parallel
waves:
  - wave: 1
    mode: parallel
    tasks: ['1.1', '1.2', '1.3']
tasks:
  - id: '1.1'
    title: Alpha
    surface: [src/a.py]
  - id: '1.2'
    title: Beta
    surface: [src/b.py]
  - id: '1.3'
    title: Alpha again
    surface: [src/a.py]
```
"""
        plan, state = _write_parallel_fixture(tmp_path, canonical)
        proc = _run_cli([str(plan), "--ready", "--state", str(state)],
                        env_path="/usr/bin:/bin", cwd=tmp_path)
        assert proc.returncode == 0, proc.stderr
        out = json.loads(proc.stdout)
        assert out["ready"] == ["1.1", "1.2"]
        assert out["blocked_on"] == {"1.3": ["1.1"]}
