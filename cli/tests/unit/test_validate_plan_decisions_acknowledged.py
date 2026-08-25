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
    data = {
        "decision_id": decision_id,
        "decision": "VERDICT",
        "subject": subject,
        "authority_source": "beastmode",
    }
    if supersedes:
        data["supersedes"] = supersedes
    row = {"ts": "2026-08-01T00:00:00.000000Z", "type": "operator_decision", "source": "target", "data": data}
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
    # A grandfathered WARN must not read as a pass on the entry it warned
    # about - the ok line's own count must say 0, not 1.
    assert "0/1 decision(s) acknowledged" in result.stdout


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
    assert "1/1 decision(s) acknowledged" in result.stdout


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
    assert "1/1 decision(s) acknowledged" in result.stdout


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
    assert "0/0 decision(s) acknowledged" in result.stdout


def test_node_field_is_also_resolved_not_only_claims(tmp_path):
    """node: is the canonical link key; claims: alone missed it (finding #2)."""
    state_dir = tmp_path / "fno-home"
    _seed_decision(state_dir, decision_id="d-abcd1234", subject="x-abcd")

    plan = tmp_path / "clean.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nnode: x-abcd\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
        "  decisions_acknowledged:\n"
        "    - decision_id: d-abcd1234\n"
        "      reason: routine, not a verdict on this work\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 0, result.stdout
    assert "1/1 decision(s) acknowledged" in result.stdout


def test_decision_id_case_is_not_significant(tmp_path):
    """d-ABCD1234 in the plan must match d-abcd1234 in the index (finding #4)."""
    state_dir = tmp_path / "fno-home"
    _seed_decision(state_dir, decision_id="d-abcd1234", subject="x-abcd")

    plan = tmp_path / "clean.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-abcd\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
        "  decisions_acknowledged:\n"
        "    - decision_id: d-ABCD1234\n"
        "      reason: routine, not a verdict on this work\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 0, result.stdout
    assert "1/1 decision(s) acknowledged" in result.stdout


def test_ruling_recorded_after_the_plan_was_written_only_warns(tmp_path):
    """An approved, in-execution plan must not be blocked by a decision
    recorded mid-flight - it could not possibly have acknowledged a ruling
    that did not exist yet when it was written (finding #5)."""
    state_dir = tmp_path / "fno-home"
    state_dir.mkdir(parents=True, exist_ok=True)
    index = state_dir / "decisions.jsonl"
    row = {
        "ts": "2026-08-24T00:00:00.000000Z",  # after the plan's created: below
        "type": "operator_decision",
        "source": "target",
        "data": {
            "decision_id": "d-abcd1234",
            "decision": "VERDICT",
            "subject": "x-abcd",
            "authority_source": "operator",
        },
    }
    with index.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(row) + "\n")

    plan = tmp_path / "approved.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-abcd\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 0, result.stdout
    assert "decisions_acknowledged is missing d-abcd1234" in result.stdout
    assert "after this plan's created: 2026-08-23 date" in result.stdout


def test_unreadable_decision_index_fails_closed(tmp_path):
    """codex finding: an unreadable index must not read as a clean pass - it
    could be hiding the closing verdict this whole gate exists to catch."""
    state_dir = tmp_path / "fno-home"
    state_dir.mkdir(parents=True, exist_ok=True)
    # A directory where a file is expected: _read_index's os.stat() succeeds
    # (not FileNotFoundError), then path.open() raises IsADirectoryError -
    # genuinely unreadable, distinct from "no index written yet".
    (state_dir / "decisions.jsonl").mkdir()

    plan = tmp_path / "plan.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\nclaims: x-abcd\ncreated: 2026-08-23\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against: []\n"
    ))
    result = _run(plan, state_dir)
    assert result.returncode == 1, result.stdout
    assert "decisions_acknowledged could not be checked" in result.stdout


def test_damaged_index_rows_fail_closed_not_reported_as_clean(tmp_path):
    """codex finding: a damaged row's decision might be the closing verdict;
    reading the surviving rows as complete would silently pass a plan whose
    node has an unreadable ruling on file."""
    state_dir = tmp_path / "fno-home"
    _seed_decision(state_dir, decision_id="d-abcd1234", subject="x-abcd")
    index = state_dir / "decisions.jsonl"
    with index.open("a", encoding="utf-8") as fh:
        fh.write("{not valid json, a torn append\n")

    plan = tmp_path / "plan.md"
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
    assert result.returncode == 1, result.stdout
    assert "decisions_acknowledged could not be checked" in result.stdout
    assert "damaged row" in result.stdout
