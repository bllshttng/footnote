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
    assert "fno do think inspect" in text
    assert "Investigate primary sources" in text
    assert "Write one Markdown file" in text
    assert "fno do plan path" in text
