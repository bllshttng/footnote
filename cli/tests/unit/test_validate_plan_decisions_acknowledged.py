"""Validator's decisions_acknowledged gate: a live ruling with no matching entry.

Change 3 of x-953b. `check_consolidation_file` cross-checks the plan's
`decisions_acknowledged` list against the node's LIVE decision-index rulings
(read directly, not through the shape model - `ConsolidationBlock` has no
access to the index). Each fixture points a fresh subprocess at a hermetic
decision index via `$FNO_CONFIG` (the only candidate when set, per
`fno.config._candidate_paths`) naming a `config.state_dir` override, then
seeds `decisions.jsonl` directly under it, in the envelope
`fno.events.operator_decision` writes - so no graph or carveout root needs to
exist for the node named in `claims:`.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = REPO_ROOT / "scripts" / "validate-plan.sh"


def _run(plan: Path, state_dir: Path) -> subprocess.CompletedProcess[str]:
    settings = state_dir.parent / "settings.yaml"
    settings.write_text(f"config:\n  state_dir: {state_dir}\n")
    env = dict(os.environ)
    env["FNO_CONFIG"] = str(settings)
    return subprocess.run(
        ["bash", str(VALIDATOR), str(plan)],
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
        env=env,
    )


def _plan(frontmatter: str) -> str:
    return (
        f"---\n{frontmatter}---\n\n# T\n\n"
        "## Context\n\nReal context here.\n\n"
        "## Changes\n\nAdd the guard in foo.py.\n\n"
        "## Files to Modify\n\n- foo.py\n\n"
        "## Verification\n\npytest cli/tests\n"
    )


def _seed_decision(state_dir: Path, *, decision_id: str, subject: str, supersedes: str | None = None) -> None:
    """Append one decision-index row directly, bypassing record_decision.

    `record_decision` also writes a durable events journal and projects onto
    a graph node - both real side effects this fixture does not want. The
    index is the only store `list_decisions` reads, so writing it directly is
    the smaller, correct fixture.
    """
    state_dir.mkdir(parents=True, exist_ok=True)
    index = state_dir / "decisions.jsonl"
    data = {"decision_id": decision_id, "decision": "VERDICT", "subject": subject}
    if supersedes:
        data["supersedes"] = supersedes
    row = {"ts": "2026-08-22T00:00:00.000000Z", "type": "operator_decision", "source": "target", "data": data}
    with index.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(row) + "\n")


def test_missing_acknowledgment_on_a_post_gate_plan_errors(tmp_path):
    state_dir = tmp_path / "fno-home"
    _seed_decision(state_dir, decision_id="d-abcd1234", subject="x-abcd")

    plan = tmp_path / "new.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-abcd\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 1, result.stdout
    assert "decisions_acknowledged is missing d-abcd1234" in result.stdout


def test_pre_gate_plan_warns_instead_of_erroring(tmp_path):
    state_dir = tmp_path / "fno-home"
    _seed_decision(state_dir, decision_id="d-abcd1234", subject="x-abcd")

    plan = tmp_path / "old.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-abcd\ncreated: 2026-08-18\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 0, result.stdout
    assert "decisions_acknowledged is missing d-abcd1234" in result.stdout
    assert "backfill before the next blueprint" in result.stdout


def test_clean_plan_names_how_many_it_checked(tmp_path):
    state_dir = tmp_path / "fno-home"
    _seed_decision(state_dir, decision_id="d-abcd1234", subject="x-abcd")

    plan = tmp_path / "clean.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-abcd\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
        "  decisions_acknowledged:\n"
        "    - decision_id: d-abcd1234\n"
        "      reason: routine, not a verdict on this work\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 0, result.stdout
    assert "1 decision(s) acknowledged" in result.stdout


def test_superseded_ruling_needs_no_acknowledgment(tmp_path):
    state_dir = tmp_path / "fno-home"
    _seed_decision(state_dir, decision_id="d-0ad00001", subject="x-abcd")
    _seed_decision(state_dir, decision_id="d-0be00001", subject="x-abcd", supersedes="d-0ad00001")

    plan = tmp_path / "clean.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-abcd\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
        "  decisions_acknowledged:\n"
        "    - decision_id: d-0be00001\n"
        "      reason: routine, not a verdict on this work\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 0, result.stdout
    assert "d-0ad00001" not in result.stdout
    assert "1 decision(s) acknowledged" in result.stdout


def test_node_with_no_decisions_passes_with_zero_count(tmp_path):
    state_dir = tmp_path / "fno-home"

    plan = tmp_path / "clean.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-953b\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 0, result.stdout
    assert "0 decision(s) acknowledged" in result.stdout
