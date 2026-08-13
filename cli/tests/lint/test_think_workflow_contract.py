from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
SKILL = ROOT / "skills" / "think" / "SKILL.md"
ARCH = ROOT / "docs" / "architecture" / "lean-think.md"


def test_think_is_research_not_planning() -> None:
    """Think investigates and cites; blueprint owns the plan and its approval."""
    text = SKILL.read_text(encoding="utf-8")
    assert "Research, not planning" in text
    assert "/fno:blueprint" in text


def test_think_runs_a_fixed_three_step_process() -> None:
    """The process is identical every run; only the brief varies."""
    text = SKILL.read_text(encoding="utf-8")
    assert "fno think inspect" in text
    assert "Investigate primary sources" in text
    assert "Write one Markdown file" in text
    assert "fno plan path" in text


def test_think_offers_substrate_tokens_and_briefs() -> None:
    """Substrate is an explicit token (D1); the briefs change the question only."""
    text = SKILL.read_text(encoding="utf-8")
    for token in ("bg", "subagent"):
        assert token in text
    for brief in ("what-if", "panel", "class"):
        assert brief in text


def test_think_demands_cited_claims() -> None:
    text = SKILL.read_text(encoding="utf-8")
    assert "file:line" in text


def test_native_plan_boundary_survives_in_architecture() -> None:
    """The Footnote-specific plan boundary is documented, not copied from Superpowers."""
    architecture = ARCH.read_text(encoding="utf-8")
    assert "Native Plan Mode" in architecture
    assert "does not copy Superpowers" in architecture
