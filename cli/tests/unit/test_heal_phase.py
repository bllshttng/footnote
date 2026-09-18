"""The pr-watch tick's heal phase gate: armed, unarmed, and no-binary paths.

Every assertion names a positive marker: the argv one drive loop per root
carries, the unarmed short-circuit that never resolves the binary. "Nothing
ran" is only ever asserted beside a same-run positive that proves the phase
was reached.
"""
from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

from fno.pr_watch._heal_phase import run_heal_phase


class Recorder:
    def __init__(self) -> None:
        self.runs: list[list[str]] = []

    def resolve(self):
        return Path("/bin/fno-agents")

    def run(self, argv, **kwargs):
        self.runs.append(argv)
        assert kwargs.get("check") is False, "the tick never fails on a red PR"


def _settings(*, armed: bool) -> SimpleNamespace:
    return SimpleNamespace(
        auto_heal=SimpleNamespace(enabled=armed),
    )


def test_an_armed_tick_runs_one_drive_loop_per_root(tmp_path):
    rec = Recorder()
    roots = [tmp_path / "a", tmp_path / "b"]

    outcome = run_heal_phase(
        _settings(armed=True), roots, resolve_binary=rec.resolve, run=rec.run
    )

    assert outcome == "ran"
    assert len(rec.runs) == 2, f"one per root: {rec.runs}"
    first = rec.runs[0]
    assert first[:3] == ["/bin/fno-agents", "pr-heal", "--all"], f"{first}"
    assert "--apply" in first, f"{first}"
    assert first[first.index("--cwd") + 1] == str(roots[0]), f"{first}"


def test_an_unarmed_tick_resolves_nothing_and_runs_nothing():
    rec = Recorder()
    # The positive control: the same call with the flag on DOES run, so a
    # zero below means the gate, not a phase that never executed.
    armed = run_heal_phase(
        _settings(armed=True), [Path("/tmp")], resolve_binary=rec.resolve, run=rec.run
    )
    assert armed == "ran" and rec.runs, "control: the armed path runs"

    unarmed = run_heal_phase(
        _settings(armed=False), [Path("/tmp")], resolve_binary=rec.resolve, run=rec.run
    )

    assert unarmed == "unarmed"
    assert len(rec.runs) == 1, "the unarmed tick added no run"


def test_a_settings_stub_with_no_auto_heal_block_reads_unarmed(tmp_path):
    # The tick's own test harnesses pass settings stubs with no auto_heal
    # attribute at all; that must read as unarmed, never raise.
    rec = Recorder()
    outcome = run_heal_phase(
        SimpleNamespace(), [tmp_path], resolve_binary=rec.resolve, run=rec.run
    )
    assert outcome == "unarmed"
    assert rec.runs == []


def test_a_missing_binary_is_reported_and_runs_nothing(tmp_path):
    rec = Recorder()
    outcome = run_heal_phase(
        _settings(armed=True),
        [tmp_path],
        resolve_binary=lambda: None,
        run=rec.run,
    )
    assert outcome == "no-binary"
    assert rec.runs == []


def test_one_failing_root_never_stops_the_rest(tmp_path):
    from subprocess import TimeoutExpired

    rec = Recorder()
    roots = [tmp_path / "a", tmp_path / "b"]

    def flaky(argv, **kwargs):
        rec.runs.append(argv)
        if len(rec.runs) == 1:
            raise TimeoutExpired(argv, 600)

    outcome = run_heal_phase(
        _settings(armed=True), roots, resolve_binary=rec.resolve, run=flaky
    )

    assert outcome == "ran"
    assert len(rec.runs) == 2, "the second root still ran"


def test_an_armed_tick_with_no_roots_never_claims_a_run(tmp_path):
    # "ran" without a run is the false receipt this phase exists not to
    # print: no root means no pr_heal_tick row, and the log must say so.
    rec = Recorder()
    outcome = run_heal_phase(
        _settings(armed=True), [], resolve_binary=rec.resolve, run=rec.run
    )
    assert outcome == "no-roots"
    assert rec.runs == []


def test_the_armed_tick_passes_detach_and_a_30s_spawn_bound(tmp_path):
    # The drive loop runs detached from the tick: the phase only
    # ever pays the spawn, so --detach rides the argv and the timeout only
    # bounds a wedged spawn, not the loop's remedies.
    captured: dict = {}
    runs: list = []

    def run(argv, **kwargs):
        runs.append(argv)
        captured["timeout"] = kwargs.get("timeout")

    outcome = run_heal_phase(
        _settings(armed=True), [tmp_path], resolve_binary=Recorder().resolve, run=run
    )

    assert outcome == "ran"
    assert "--detach" in runs[0], f"{runs}"
    assert "--cwd" in runs[0] and runs[0][-1] == str(tmp_path), f"{runs}"
    assert captured["timeout"] == 30, captured


def test_a_drive_loop_that_fails_on_every_root_never_reports_ran(tmp_path):
    # A stale binary exits 4 on every root: nothing ran, no pr_heal_tick row
    # will land, and the status line must not keep showing a stale
    # "last run". The gate row comes from cli.py for any non-"ran" answer.
    import types

    def failing(argv, **kwargs):
        return types.SimpleNamespace(returncode=4)

    outcome = run_heal_phase(
        _settings(armed=True),
        [tmp_path / "a", tmp_path / "b"],
        resolve_binary=Recorder().resolve,
        run=failing,
    )

    assert outcome == "failed", "an all-failed drive loop is not a run"


def test_a_drive_loop_that_fails_on_one_root_still_reports_ran(tmp_path):
    import types

    def half_failing(argv, **kwargs):
        # First call (root a) fails, second (root b) runs.
        if argv[-1].endswith("a"):
            return types.SimpleNamespace(returncode=4)
        return types.SimpleNamespace(returncode=0)

    outcome = run_heal_phase(
        _settings(armed=True),
        [tmp_path / "a", tmp_path / "b"],
        resolve_binary=Recorder().resolve,
        run=half_failing,
    )

    assert outcome == "ran", "one good root still counts as a run"
