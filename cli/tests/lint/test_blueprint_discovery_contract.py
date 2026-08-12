from pathlib import Path


ROOT = Path(__file__).parents[3]
SKILL = ROOT / "skills" / "blueprint" / "SKILL.md"
DISCOVERY = ROOT / "skills" / "blueprint" / "references" / "discovery-gate.md"


def test_supplied_design_docs_do_not_repeat_discovery() -> None:
    text = SKILL.read_text(encoding="utf-8")

    assert "compiles it without re-running discovery" in text
    assert "that doc has a `## Discovery` or `## Assumptions` section" not in text


def test_fresh_plan_paths_keep_discovery() -> None:
    text = SKILL.read_text(encoding="utf-8")

    assert "raw prose or a node-seeded path" in text


def test_discovery_reference_names_blueprint_as_owner() -> None:
    text = DISCOVERY.read_text(encoding="utf-8")

    assert "grounds itself on the receipt" in text
    assert "owns discovery for a supplied design artifact" not in text
