"""The surviving review-artifact skill contracts after the sigma retirement.

The six-agent panel and its selection matrix are gone
(skills/review/references/sigma.md and agent-selection.md, deleted with the
retirement). What survives and stays pinned here: the /pr check flow that
consumes the durable review artifacts, the report template that still carries
the RECOMMEND RESTART contract target's failure-recovery.md cites, and the
retirement itself - the router no longer instructs anyone to run a panel, and
the lane reference carries the rehomed scope rules.
"""
from pathlib import Path

import re


ROOT = Path(__file__).parents[3]


def test_pr_check_inspects_before_zero_reviewer_return() -> None:
    check = (ROOT / "skills/pr/references/check.md").read_text(encoding="utf-8")
    step_zero = check.index("## Step 0: Determine Reviewers")
    step_two = check.index("### 2. Wait for Review")
    window = check[step_zero:step_two]
    assert "    exit 0" not in window
    assert "--inspect-sigma" in window
    assert "EXTERNAL_REVIEW_ENABLED=0" in window
    assert 'if [[ "$EXTERNAL_REVIEW_ENABLED" == "1" ]]' in window
    assert "REPO_SLUG=$(git config --get remote.origin.url" in window
    assert "OWNER=${REPO_SLUG%%/*}" in window
    assert "REPO=${REPO_SLUG#*/}" in window


def test_pr_check_reuses_badge_and_response_sequence() -> None:
    check = (ROOT / "skills/pr/references/check.md").read_text(encoding="utf-8")
    assert "Apply this same badge parser to sigma artifact lines" in check
    assert "existing verify, decide, implement, push, and response sequence" in check
    assert "rather than inventing a thread" in check
    assert "fno-sigma-disposition id=$STABLE_ID" in check
    assert "exclude that stable id" in check


def test_report_template_records_scope() -> None:
    template = (ROOT / "skills/review/references/report-template.md").read_text(
        encoding="utf-8"
    )
    assert "**Review Scope:**" in template
    for reason in ("first-round", "rules-changed", "history-rewritten", "incremental"):
        assert reason in template, reason


def test_report_template_carries_the_restart_contract() -> None:
    template = (ROOT / "skills/review/references/report-template.md").read_text(
        encoding="utf-8"
    )
    assert "Effective model" in template
    assert "- **critical** -" in template
    assert "- **high** -" in template


def test_the_panel_is_retired_and_the_lane_replaced_it() -> None:
    """Positive and negative markers together: a deleted file cannot pass,
    and asserting the panel did not run would pass on a run where nothing
    ran at all."""
    router = (ROOT / "skills/review/SKILL.md").read_text(encoding="utf-8")
    lane = (ROOT / "skills/review/references/single-lane.md").read_text(
        encoding="utf-8"
    )
    # The retired references are gone.
    for gone in (
        "references/sigma.md",
        "references/agent-selection.md",
    ):
        assert not (ROOT / "skills/review" / gone).exists(), gone
    # The router refuses the retired token and names the replacement.
    assert "refused: sigma is retired" in router
    assert "the default review lane replaced it" in router
    # The lane, not a dispatch, is the default path.
    assert "dispatches ZERO review subagents" in router
    assert "do not spawn subagents" in lane
    assert "Task/Agent" not in lane
    # No panel instruction survives in the router's default step.
    step2 = router[router.index("## Step 2: the default mode") : router.index("## Step 3:")]
    assert "six-agent" not in step2


def test_the_lane_keeps_the_scope_rules_that_made_narrowing_safe() -> None:
    lane = (ROOT / "skills/review/references/single-lane.md").read_text(
        encoding="utf-8"
    )
    match = re.search(r"grep -qE '([^']+)'", lane)
    assert match is not None, "scope-narrowing grep pattern not found in single-lane.md"
    pattern = re.compile(match.group(1))
    for path in ("CLAUDE.md", "AGENTS.md", ".claude/rules/style.md", "docs/CLAUDE.md"):
        assert pattern.search(path), path
    for path in ("cli/src/fno/cli.py", "docs/style-rules.md", "claude-rules.txt"):
        assert not pattern.search(path), path
    assert "git merge-base --is-ancestor" in lane
    assert "can never pass vacuously" in lane
