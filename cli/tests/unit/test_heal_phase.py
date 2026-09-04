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
