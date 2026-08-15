from pathlib import Path

import re


ROOT = Path(__file__).parents[3]


def test_sigma_resolves_scope_before_dispatch() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    capture = sigma.index("REVIEWED_HEAD=$(git rev-parse HEAD)")
    resolution = sigma.index("### Step 1b: Resolve Review Scope")
    dispatch = sigma.index("### Step 2: Run Base Agents")
    assert capture < resolution < dispatch
    window = sigma[resolution:dispatch]
    assert "--sigma-last-head" in window
    assert "git merge-base --is-ancestor" in window
    assert "SCOPE_BASE=" in window and "SCOPE_REASON=" in window
    assert "CHANGED_FILES=" in window and "FULL_DIFF_FILES=" in window


def test_scope_fallback_reasons_all_present() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    for reason in ("first-round", "incremental", "rules-changed", "history-rewritten"):
        assert f'"{reason}"' in sigma, reason


def test_rules_changed_pattern_matches_real_path_shapes() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    match = re.search(r"grep -qE '([^']+)'", sigma)
    assert match is not None, "scope-resolution grep pattern not found in sigma.md"
    pattern = re.compile(match.group(1))
    # Positive control: every rules path shape must escalate to full scope.
    for path in ("CLAUDE.md", "AGENTS.md", ".claude/rules/style.md", "docs/CLAUDE.md"):
        assert pattern.search(path), path
    # Negative control: ordinary code paths must not.
    for path in ("cli/src/fno/cli.py", "docs/style-rules.md", "claude-rules.txt"):
        assert not pattern.search(path), path


def test_changed_files_have_a_single_producer() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    selection = (
        ROOT / "skills/review/references/agent-selection.md"
    ).read_text(encoding="utf-8")
    both = sigma + selection
    # No diff read may name a bare branch as its base: that is a second
    # producer scanning a different (possibly stale) base than Step 1b.
    stale = re.findall(r"git diff --(?:name-only|stat) +(?:origin/)?main\.\.\.HEAD", both)
    assert stale == [], stale
    # Positive control: both files consume the Step 1b variables.
    assert '$SCOPE_BASE..HEAD' in sigma
    assert '"$SCOPE_BASE..HEAD"' in selection or "$SCOPE_BASE..HEAD" in selection
    assert 'CHANGED="${CHANGED_FILES:-' in selection


def test_dispatch_prompts_carry_increment_plus_full_context() -> None:
    selection = (
        ROOT / "skills/review/references/agent-selection.md"
    ).read_text(encoding="utf-8")
    base = selection.index("## Base Agents (Always Run)")
    conditional = selection.index("## Conditional Agents")
    window = selection[base:conditional]
    assert "since the last reviewed head: ${changedFiles}" in window
    assert "${fullDiffFiles}" in window
    assert window.count("context only") >= 2
    assert "trace the error handling across the boundary" in window


def test_carry_forward_revalidates_prior_findings_before_verdict() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    scoring = sigma.index("### Step 3b: Confidence Scoring")
    carry = sigma.index("### Step 3c: Carry Forward Unresolved Prior Findings")
    automated = sigma.index("### Step 4: Run Automated Checks")
    assert scoring < carry < automated
    window = sigma[carry:automated]
    assert "--inspect-sigma" in window
    assert "abstain band" in window
    assert "cannot be `ready-to-merge`" in window
    attestation = sigma.index("### Step 6c: Emit the reviewers-gate attestation")
    assert "cumulative across rounds up to this head" in sigma[attestation:]


def test_report_and_artifact_record_scope() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    template = (ROOT / "skills/review/references/report-template.md").read_text(
        encoding="utf-8"
    )
    assert "**Review Scope:**" in template
    for reason in ("first-round", "rules-changed", "history-rewritten", "incremental"):
        assert reason in template, reason
    publish = sigma.index("### Step 6d: Persist the report")
    comment = sigma.index("### Step 6e: Project the durable report")
    window = sigma[publish:comment]
    assert '--sigma-scope-base "$SCOPE_BASE"' in window
    assert '--sigma-scope-reason "$SCOPE_REASON"' in window


def test_sigma_publishes_before_comment_eligibility() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    publish = sigma.index("### Step 6d: Persist the report")
    comment = sigma.index("### Step 6e: Project the durable report")
    assert publish < comment
    assert "--publish-sigma" in sigma[publish:comment]
    assert "fno-sigma head=$REVIEWED_HEAD round=$ROUND_ID" in sigma[comment:]


def test_sigma_captures_reviewed_head_before_panel_dispatch() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    capture = sigma.index("REVIEWED_HEAD=$(git rev-parse HEAD)")
    dispatch = sigma.index("### Step 2: Run Base Agents")
    assert capture < dispatch


def test_pr_check_inspects_sigma_before_zero_reviewer_return() -> None:
    check = (ROOT / "skills/pr/references/check.md").read_text(encoding="utf-8")
    step_zero = check.index("## Step 0: Determine Reviewers")
    step_two = check.index("### 2. Wait for Review")
    window = check[step_zero:step_two]
    assert "    exit 0" not in window
    assert "--inspect-sigma" in window
    assert "EXTERNAL_REVIEW_ENABLED=0" in window
    assert 'if [[ "$EXTERNAL_REVIEW_ENABLED" == "1" ]]' in window
    assert "OWNER=$(gh repo view" in window
    assert "REPO=$(gh repo view" in window


def test_pr_check_reuses_badge_and_response_sequence() -> None:
    check = (ROOT / "skills/pr/references/check.md").read_text(encoding="utf-8")
    assert "Apply this same badge parser to sigma artifact lines" in check
    assert "existing verify, decide, implement, push, and response sequence" in check
    assert "rather than inventing a thread" in check
    assert "fno-sigma-disposition id=$STABLE_ID" in check
    assert "exclude that stable id" in check


def test_sigma_route_contract_prices_named_sessions_and_records_model() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    template = (ROOT / "skills/review/references/report-template.md").read_text(
        encoding="utf-8"
    )
    assert "--agent \"fno:$AGENT_HYPHEN\"" in sigma
    assert "--route \"$ROUTE_PROVIDER/$MODEL\"" in sigma
    assert "300–360K preamble tokens" in sigma
    assert "Effective model" in template
    assert "- **critical** -" in template
    assert "- **high** -" in template


def test_sigma_reports_observed_runtime_not_requested_route() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    router = (ROOT / "skills/review/SKILL.md").read_text(encoding="utf-8")
    selection = (ROOT / "skills/review/references/agent-selection.md").read_text(
        encoding="utf-8"
    )
    template = (ROOT / "skills/review/references/report-template.md").read_text(
        encoding="utf-8"
    )

    assert "INVOKING_HARNESS=" in sigma
    assert "differs from `INVOKING_HARNESS`" in sigma
    assert "Never copy the requested provider into the observed runtime" in sigma
    assert 'if [ "$PROVIDER" = "claude" ]; then' in sigma
    assert '--harness claude --substrate headless' in sigma
    assert "Requested route" in template
    assert "Observed runtime" in template
    assert "six-agent Claude review panel" not in router
    assert "claude -> `Task()`" not in selection
    assert "all-Claude run" not in selection
