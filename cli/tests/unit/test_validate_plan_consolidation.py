"""Validator consolidation-block gate: negative and grandfather paths.

The happy path lives in the stub-marker and project-warning fixtures. These
tests pin the failure modes a silent gate would miss: a new plan with no
block, a legacy plan grandfathered to a warn, flow-style lists, comment
noise, and an outcome with no recorded decision.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = REPO_ROOT / "scripts" / "validate-plan.sh"


def _run(plan: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(VALIDATOR), str(plan)],
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
    )


def _plan(frontmatter: str) -> str:
    # The full-body shape the execution-contract leg accepts (mirrors the
    # stub-marker suite's _FILLED fixture), so these tests isolate the
    # consolidation gate rather than re-tripping the execution contract.
    return (
        f"---\n{frontmatter}---\n\n# T\n\n"
        "## Context\n\nReal context here.\n\n"
        "## Changes\n\nAdd the guard in foo.py.\n\n"
        "## Files to Modify\n\n- foo.py\n\n"
        "## Verification\n\npytest cli/tests\n"
    )


def test_new_plan_without_block_errors(tmp_path):
    plan = tmp_path / "new.md"
    plan.write_text(_plan("title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"))
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "no consolidation: block" in result.stdout
    # F2: the missing block must not abort the validator - later checks and
    # the summary still run.
    assert "Errors:" in result.stdout


def test_legacy_plan_without_block_warns_not_errors(tmp_path):
    plan = tmp_path / "old.md"
    plan.write_text(_plan("title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-15\n"))
    result = _run(plan)
    assert result.returncode == 0, result.stdout
    assert "backfill one before the next blueprint" in result.stdout


def test_flow_style_id_list_errors_with_fix(tmp_path):
    plan = tmp_path / "flow.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed: [{id: x-ab12, reason: same lock}]\n"
    ))
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "block style" in result.stdout


def test_inline_comment_on_outcome_line_is_stripped(tmp_path):
    plan = tmp_path / "comment.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"
        "consolidation:\n"
        "  outcome: proceed_alone  # no siblings found\n"
        "  proceed_alone_against: []\n"
    ))
    result = _run(plan)
    assert result.returncode == 0, result.stdout
    assert "consolidation outcome is proceed_alone" in result.stdout


def test_absorb_with_empty_absorbed_list_errors(tmp_path):
    plan = tmp_path / "empty-absorb.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed: []\n"
    ))
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "absorbed: list is empty" in result.stdout


def test_plan_created_on_the_gate_date_is_grandfathered(tmp_path):
    # Strictly-after boundary: a plan created ON the gate date predates the
    # gate reaching its author. Nine live plans carry that date, and erroring
    # on them would refuse every one at /do time.
    plan = tmp_path / "on-gate.md"
    plan.write_text(_plan("title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-17\n"))
    result = _run(plan)
    assert result.returncode == 0, result.stdout
    assert "not after the 2026-08-17 gate" in result.stdout


def test_quoted_and_compact_created_dates_are_read_not_guessed(tmp_path):
    # A quoted date used to keep its quote, sort before the gate, and warn
    # forever; a compact YYYYMMDD stamp sorted after it and hard-errored.
    quoted = tmp_path / "quoted.md"
    quoted.write_text(_plan('title: T\nstatus: ready\nkind: quick-plan\ncreated: "2026-08-25"\n'))
    assert _run(quoted).returncode == 1

    compact = tmp_path / "compact.md"
    compact.write_text(_plan("title: T\nstatus: ready\nkind: quick-plan\ncreated: 20260806\n"))
    out = _run(compact)
    assert out.returncode == 0, out.stdout
    assert "created 2026-08-06" in out.stdout


def test_unreadable_created_grandfathers_and_says_so(tmp_path):
    # An unparsable date must not be compared lexicographically against the
    # gate. Warn (never block in-flight work) and name the real reason.
    plan = tmp_path / "junk-date.md"
    plan.write_text(_plan("title: T\nstatus: ready\nkind: quick-plan\ncreated: last tuesday\n"))
    out = _run(plan)
    assert out.returncode == 0, out.stdout
    assert "is not a readable date" in out.stdout


def test_entry_keys_may_come_in_any_order(tmp_path):
    # YAML lets a mapping's keys come in any order. Keying the walk on `- id:`
    # read a reason-first entry as no entry and blamed the list for being empty.
    plan = tmp_path / "reason-first.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-20\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed:\n"
        "    - reason: same lock, wave B\n"
        "      id: x-ab12\n"
    ))
    out = _run(plan)
    assert out.returncode == 0, out.stdout
    assert "consolidation outcome is absorb" in out.stdout


def test_entry_with_no_id_is_reported(tmp_path):
    plan = tmp_path / "no-id.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-20\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed:\n"
        "    - reason: only a reason\n"
    ))
    out = _run(plan)
    assert out.returncode == 1
    assert "entry with no id" in out.stdout


def test_positive_marker_survives_an_unrelated_error(tmp_path):
    # The gate reports through its own counter: a valid block must still print
    # its marker when some other check has already failed.
    plan = tmp_path / "unrelated.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-20\n"
        "kill_criteria:\n  - name: x\n"
        "consolidation:\n  outcome: proceed_alone\n  proceed_alone_against: []\n"
    ))
    out = _run(plan)
    assert out.returncode == 1  # the unrelated kill_criteria error
    assert "consolidation outcome is proceed_alone" in out.stdout


def test_a_hyphenated_next_key_ends_the_block(tmp_path):
    # The block extractor stopped only at an underscore-or-alnum key, so a
    # real `depends-on:` sitting after the block was swallowed INTO it and its
    # list items were read as entries with no id. Twelve live plans carry a
    # hyphenated key.
    plan = tmp_path / "hyphen.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-20\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against:\n"
        "    - id: x-aaaa\n"
        "      reason: different subsystem\n"
        "depends-on:\n"
        "  - x-bbbb\n"
        "  - x-cccc\n"
    ))
    out = _run(plan)
    assert out.returncode == 0, out.stdout
    assert "entry with no id" not in out.stdout
    assert "consolidation outcome is proceed_alone" in out.stdout


def test_a_list_key_after_the_entries_closes_the_section(tmp_path):
    # The walk set the section on its header and cleared it nowhere, so a
    # later list-valued key had its items read as entries of that list.
    plan = tmp_path / "tail-list.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-20\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed:\n"
        "    - id: x-ab12\n"
        "      reason: same lock\n"
        "  reversal:\n"
        "    - fno backlog unsupersede x-ab12\n"
    ))
    out = _run(plan)
    assert out.returncode == 0, out.stdout
    assert "entry with no id" not in out.stdout


def test_entry_id_must_look_like_a_node_id(tmp_path):
    # A bare number is legal YAML that parses to an int. The awk walk reads it
    # as text and the model rejects it as a non-string, and that divergence
    # surfaces downstream as "plan is unreadable or invalid".
    plan = tmp_path / "int-id.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-20\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed:\n"
        "    - id: 12345\n"
        "      reason: numeric id\n"
    ))
    out = _run(plan)
    assert out.returncode == 1
    assert "is not a node id" in out.stdout


def test_append_outcome_in_a_written_plan_is_a_contradiction(tmp_path):
    # append records that the content went onto the other node and no second
    # plan was written. This file existing says otherwise.
    plan = tmp_path / "append.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-20\n"
        "consolidation:\n"
        "  outcome: append\n"
        "  appended_to:\n"
        "    - id: x-ab12\n"
        "      reason: belongs on that node\n"
    ))
    out = _run(plan)
    assert out.returncode == 1
    assert "a plan file was written" in out.stdout


def test_decompose_child_scaffold_carries_created(tmp_path):
    # Without created: every decompose child is grandfathered forever, so the
    # gate is permanently inert on that path.
    from fno.graph._decompose import scaffold_separate_plan, validate_groups

    group = validate_groups([{"slug": "1", "title": "G1", "waves": "1-2"}], None)[0]
    text = scaffold_separate_plan(group, "ab-epic0001", "big.md", created="2026-08-20")
    assert "created: 2026-08-20" in text


def test_blueprint_owns_the_consolidation_frontmatter_key():
    # The validator requires the block, so the ownership model must permit the
    # write that satisfies it, or blueprint raises OwnershipViolation instead.
    from fno.plan._ownership import check_blueprint_can_write

    assert check_blueprint_can_write("consolidation")


# -- the Pydantic shape authority (fno plan validate must agree with the gate) --


def test_pydantic_model_rejects_malformed_block():
    from pydantic import ValidationError

    from fno.plan.schema import PlanFrontmatter

    base = {"node": "x-1", "status": "ready", "created": "2026-08-18"}

    # Out-of-enum outcome fails the model, not just the bash gate.
    bad = dict(base, consolidation={"outcome": "maybe"})
    with pytest.raises(ValidationError):
        PlanFrontmatter.model_validate(bad)

    # An entry with an empty reason fails on both surfaces.
    bad = dict(base, consolidation={
        "outcome": "absorb",
        "absorbed": [{"id": "x-ab12", "reason": ""}],
    })
    with pytest.raises(ValidationError):
        PlanFrontmatter.model_validate(bad)

    # A well-formed block validates.
    good = dict(base, consolidation={
        "outcome": "absorb",
        "absorbed": [{"id": "x-ab12", "reason": "same lock"}],
        "reversal": "fno backlog unsupersede x-ab12",
    })
    assert PlanFrontmatter.model_validate(good).consolidation.outcome == "absorb"

    # Absorb recording no decision is rejected here too (the empty-block rule).
    bad = dict(base, consolidation={"outcome": "absorb", "absorbed": []})
    with pytest.raises(ValidationError):
        PlanFrontmatter.model_validate(bad)
