"""The /fno:law skill is a one-step chat surface, and says what that trades."""

from __future__ import annotations

from pathlib import Path


REPO_ROOT = Path(__file__).parents[3]
SKILL = REPO_ROOT / "skills" / "law" / "SKILL.md"
LIMITATIONS = REPO_ROOT / "skills" / "law" / "LIMITATIONS.md"


def test_law_skill_is_discoverable_and_one_step() -> None:
    text = SKILL.read_text(encoding="utf-8")

    assert text.startswith("---\n")
    assert "name: law" in text
    assert "requires:" in text
    assert '- "fno >= 0.3.1"' in text
    assert "/fno:law" in text
    assert "fno inbox decisions" in text
    assert "fno inbox law set" in text
    assert "--authority operator" not in text


def test_law_skill_teaches_no_retired_ceremony() -> None:
    """The five-step workflow is gone; leaving it would teach a dead path."""
    text = SKILL.read_text(encoding="utf-8")

    for retired in (
        "law prepare",
        "law enact",
        "law resume",
        "law inspect",
        "--receipt",
        "--proposal",
        "<proposal-id>",
    ):
        assert retired not in text, retired


def test_beginner_example_has_no_shell_syntax() -> None:
    text = SKILL.read_text(encoding="utf-8")
    example = text.split("## Beginner example", 1)[1].split("##", 1)[0]

    assert "/fno:law Merges belong to the operator" in example
    assert "bash" not in example.lower()
    assert "fno law" not in example


def test_limitations_state_the_traded_property_with_its_measurement() -> None:
    """A property given up silently is the failure mode this file prevents."""
    text = LIMITATIONS.read_text(encoding="utf-8")

    assert "chat_attested" in text
    assert "promptSource" in text
    assert "--raw" in text
    assert "not distinguishable" in text.lower() or "no field to refuse" in text
