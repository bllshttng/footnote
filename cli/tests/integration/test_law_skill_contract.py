"""The /fno:law skill is a chat surface, not a shell-authority shortcut."""

from __future__ import annotations

from pathlib import Path


REPO_ROOT = Path(__file__).parents[3]
SKILL = REPO_ROOT / "skills" / "law" / "SKILL.md"


def test_law_skill_is_discoverable_and_permission_bound() -> None:
    text = SKILL.read_text(encoding="utf-8")

    assert text.startswith("---\n")
    assert "name: law" in text
    assert "requires:" in text
    assert "- \"fno >= 0.3.1\"" in text
    assert "/fno:law" in text
    assert "fno inbox decisions" in text
    assert "fno inbox law prepare" in text
    assert "fno inbox law enact" in text
    assert "/fno:law resume <proposal-id>" in text
    assert "--authority operator" not in text
    assert "environment" in text.lower()


def test_beginner_example_has_no_shell_syntax() -> None:
    text = SKILL.read_text(encoding="utf-8")
    example = text.split("## Beginner example", 1)[1].split("##", 1)[0]

    assert "/fno:law Merges belong to the operator" in example
    assert "bash" not in example.lower()
    assert "fno law" not in example
