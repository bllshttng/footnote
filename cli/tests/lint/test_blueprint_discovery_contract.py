from pathlib import Path


ROOT = Path(__file__).parents[3]
SKILL = ROOT / "skills" / "blueprint" / "SKILL.md"
DISCOVERY = ROOT / "skills" / "blueprint" / "references" / "discovery-gate.md"


def test_supplied_design_docs_do_not_repeat_discovery() -> None:
    text = SKILL.read_text(encoding="utf-8")

    assert "compiles it without re-running discovery" in text
    assert "that doc has a `## Discovery` or `## Assumptions` section" not in text
