"""Contract tests for the review skill's active-instruction preflight."""

from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
REVIEW_SKILL = ROOT / "skills" / "review" / "SKILL.md"


def test_active_skill_freshness_runs_before_review_router() -> None:
    text = REVIEW_SKILL.read_text(encoding="utf-8")
    probe = 'fno doctor plugin-file "$SKILL_DIR/SKILL.md"'
    router = "## Step 1: Resolve the mode"

    assert probe in text
    assert "PLUGIN_FILE_STALE" in text
    assert text.index(probe) < text.index(router)


def test_stale_active_skill_preflight_stops_before_lane_markers() -> None:
    text = REVIEW_SKILL.read_text(encoding="utf-8")
    preflight = text.index("## Active skill freshness preflight")
    router = text.index("## Step 1: Resolve the mode")
    stale = text.index("PLUGIN_FILE_STALE", preflight, router)

    assert 'exit "$FRESHNESS_EXIT"' in text[stale:router]
    assert "running fno review lane" not in text[preflight:router]


def test_unavailable_freshness_diagnostic_warns_and_continues() -> None:
    """A deployed fno older than the verb exits non-zero with no marker; the
    staleness doc's contract is warn-and-continue, never a refused review."""
    text = REVIEW_SKILL.read_text(encoding="utf-8")
    preflight = text.index("## Active skill freshness preflight")
    router = text.index("## Step 1: Resolve the mode")
    preflight_text = text[preflight:router]

    assert 'if [ "$FRESHNESS_EXIT" -ne 0 ]' in preflight_text
    assert "review refused" in preflight_text
    assert preflight_text.index("FRESHNESS_EXIT") < preflight_text.index("review refused")


def test_review_cap_gate_names_the_budget_and_the_one_checked_path() -> None:
    text = REVIEW_SKILL.read_text(encoding="utf-8")
    start = text.index("## Review-cap gate")
    end = text.index("## Active skill freshness preflight", start)
    gate = text[start:end]

    for phrase in (
        "rounds_exhausted",
        "fno do pr status",
        "fno-agents review-coverage --cwd . --pr <n>",
        "request-self-review",
        "hooks/review-hold.sh",
        "--verify-fixes",
    ):
        assert phrase in gate, f"## Review-cap gate omits {phrase!r}"
    for path in (REVIEW_SKILL, ROOT / "skills" / "review" / "references" / "peer.md"):
        content = path.read_text(encoding="utf-8")
        assert "IMPOSSIBLE" not in content, f"{path} retains IMPOSSIBLE"
        assert "non-author GitHub approval" not in content, f"{path} retains retired remedy"
