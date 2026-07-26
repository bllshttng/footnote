from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
THINK = ROOT / "skills" / "think" / "references" / "think.md"
SKILL = ROOT / "skills" / "think" / "SKILL.md"
ARCH = ROOT / "docs" / "architecture" / "lean-think.md"


def test_default_think_is_lean_and_receipt_driven() -> None:
    text = THINK.read_text()
    assert len(text.splitlines()) <= 450
    assert "fno think inspect" in text
    assert all(depth in text for depth in ("light", "standard", "deep"))
    assert "one material question at a time" in text
    assert "2>/dev/null || true" not in text


def test_blueprint_handoff_contract_survives_prompt_diet() -> None:
    text = THINK.read_text()
    assert "## Failure Modes" in text
    assert all(label in text for label in ("**Boundaries**", "**Errors**", "**Invariants**", "**Concurrency**"))
    assert "fno plan path" in text
    assert "fno backlog update" in text
    assert "skills/tdd/references/bdd-acceptance-criteria.md" in text
    assert "Load the `/bdd-acceptance-criteria` skill" not in text


def test_acceptance_criteria_require_behavior_not_format() -> None:
    """AC10-DOC: Think requires observable behavior, not one Markdown spelling.

    Explicit AC identifiers are optional author input; Blueprint supplies
    deterministic identifiers when absent. A future prompt edit must not
    silently reintroduce mandatory AC formatting.
    """
    text = THINK.read_text()
    # Observable, verifiable behavior stays required.
    assert "observable" in text
    # Explicit AC labels are framed as optional, not mandatory.
    assert "optional author input" in text
    # At least one compiler-accepted shape beyond bold AC is named, so the
    # guidance does not collapse back to "write a bold AC token".
    assert "Given/When/Then" in text


def test_router_and_architecture_define_native_plan_boundary() -> None:
    router = SKILL.read_text()
    architecture = ARCH.read_text()
    assert "adaptive" in router.lower()
    assert "Native Plan Mode" in architecture
    assert "/fno:blueprint" in architecture
    assert "does not copy Superpowers" in architecture
