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
