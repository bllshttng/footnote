"""A cross-checkout event type must reach the global log, not just the project one.

The project log is written wherever the emitter runs, which for a review is a
worktree. ``fno pr merge`` runs from canonical and reads canonical, so a
worktree-local ``review_attestation`` is a satisfied gate that reads as an
unsatisfiable one. ``loopcheck.rs`` already fixed this for ``review_coverage``
by emitting to both logs; ``review_attestation`` goes through ``fno event emit``
and kept the defect.

Measured 2026-08-13 on a real PR: canonical held 4 attestations and zero
mentions of the reviewed head, while the worktree held that head 4 times. The
merge refused a review that had actually happened.
"""

from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.events.cli import GLOBAL_MIRROR_TYPES, cli as events_app

runner = CliRunner()


def _emit(tmp_path: Path, monkeypatch, type_: str, data: str = "{}"):
    """Emit into a project log with the global log redirected under tmp_path."""
    project = tmp_path / "project" / "events.jsonl"
    project.parent.mkdir(parents=True, exist_ok=True)
    state = tmp_path / "state"
    state.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr("fno.paths.state_dir", lambda: state)
    result = runner.invoke(
        events_app,
        ["emit", "--type", type_, "--data", data, "--events", str(project), "--source", "test"],
    )
    return result, project, state / "events.jsonl"


def _types(path: Path) -> list[str]:
    if not path.exists():
        return []
    out = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            out.append(json.loads(line).get("type"))
    return out


def test_review_attestation_reaches_both_logs(tmp_path, monkeypatch):
    result, project, global_log = _emit(
        tmp_path, monkeypatch, "review_attestation",
        '{"reviewer": "code-review", "head_sha": "deadbeef", "verdict": "pass",'
        ' "session_id": "s1"}',
    )
    assert result.exit_code == 0, result.output
    assert _types(project) == ["review_attestation"]
    # The whole point: a reader in another checkout can see it.
    assert _types(global_log) == ["review_attestation"], (
        "the attestation stayed worktree-local, so a merge from canonical "
        "cannot see it"
    )


def test_an_ordinary_event_stays_project_local(tmp_path, monkeypatch):
    """The positive control for the test above.

    Without it, "attestation reaches global" would pass just as happily against
    an implementation that mirrored every event, which would double the global
    log for no reason and prove nothing about the mirror set.
    """
    result, project, global_log = _emit(tmp_path, monkeypatch, "daemon_started")
    assert result.exit_code == 0, result.output
    assert _types(project) == ["daemon_started"]
    assert _types(global_log) == []


def test_the_mirror_set_is_small_and_named():
    # Membership is earned by having a cross-checkout reader. review_coverage
    # is absent on purpose: loopcheck.rs emits it to both logs itself, and a
    # second writer here would double every row the merge gate counts.
    assert "review_attestation" in GLOBAL_MIRROR_TYPES
    assert "review_coverage" not in GLOBAL_MIRROR_TYPES
