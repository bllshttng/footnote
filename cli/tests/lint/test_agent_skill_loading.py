from __future__ import annotations

from pathlib import Path

from fno.paths import resolve_repo_root

ROOT = resolve_repo_root()


def _split_agent(path: Path) -> tuple[str, str]:
    _, frontmatter, body = path.read_text(encoding="utf-8").split("---", 2)
    return frontmatter, body


def test_no_agent_ships_a_skills_key():
    reason = (
        "a skills list loads nothing under claude --agent "
        "(what fno agents spawn --agent runs) and drops unresolved "
        "entries silently in a subagent. Load the skill from the body "
        "with the Skill tool."
    )
    offending = []
    for path in sorted((ROOT / "agents").glob("*.md")):
        frontmatter, _ = _split_agent(path)
        if any(line.startswith("skills:") for line in frontmatter.splitlines()):
            offending.append(f"{path.relative_to(ROOT)}: {reason}")

    assert not offending, "\n".join(offending)


def test_archer_loads_tdd_from_its_body():
    frontmatter, body = _split_agent(ROOT / "agents" / "archer.md")
    tools_line = next(
        (line for line in frontmatter.splitlines() if line.startswith("tools:")),
        "",
    )

    assert '"Skill"' in tools_line
    assert "fno:tdd" in body
    assert "Skill tool" in body
