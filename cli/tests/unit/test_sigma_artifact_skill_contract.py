from pathlib import Path


ROOT = Path(__file__).parents[3]


def test_sigma_publishes_before_comment_eligibility() -> None:
    sigma = (ROOT / "skills/review/references/sigma.md").read_text(encoding="utf-8")
    publish = sigma.index("### Step 6d: Persist the report")
    comment = sigma.index("### Step 6e: Project the durable report")
    assert publish < comment
    assert "--publish-sigma" in sigma[publish:comment]
    assert "fno-sigma head=$REVIEWED_HEAD round=$ROUND_ID" in sigma[comment:]


def test_pr_check_inspects_sigma_before_zero_reviewer_return() -> None:
    check = (ROOT / "skills/pr/references/check.md").read_text(encoding="utf-8")
    step_zero = check.index("## Step 0: Determine Reviewers")
    step_two = check.index("### 2. Wait for Review")
    window = check[step_zero:step_two]
    assert "    exit 0" not in window
    assert "--inspect-sigma" in window
    assert "EXTERNAL_REVIEW_ENABLED=0" in window


def test_pr_check_reuses_badge_and_response_sequence() -> None:
    check = (ROOT / "skills/pr/references/check.md").read_text(encoding="utf-8")
    assert "Apply this same badge parser to sigma artifact lines" in check
    assert "existing verify, decide, implement, push, and response sequence" in check
    assert "rather than inventing a thread" in check
