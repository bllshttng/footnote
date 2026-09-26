"""The architect agent body points the planner at drafting lenses, never at
the grading lenses. A grader read during drafting turns into a checklist
the author writes to, so the body may name neither the lens files nor their
folders."""

from __future__ import annotations

from fno.paths import resolve_repo_root

FORBIDDEN = ("lenses.md", "lenses/")


def _split_architect() -> tuple[str, str]:
    path = resolve_repo_root() / "agents" / "architect.md"
    # Plain string split on the first two `---` lines; the frontmatter is
    # small and the strict YAML parsers that read every other file live in
    # their own gates.
    _, frontmatter, body = path.read_text(encoding="utf-8").split("---", 2)
    return frontmatter, body


def _disallowed_names(frontmatter: str) -> list[str]:
    for line in frontmatter.splitlines():
        if line.startswith("disallowedTools:"):
            inner = line.split("[", 1)[1].rsplit("]", 1)[0]
            return [item.strip().strip("\"'") for item in inner.split(",") if item.strip()]
    return []


def _description_value(frontmatter: str) -> str:
    for line in frontmatter.splitlines():
        if line.startswith("description:"):
            return line.split(":", 1)[1].strip().strip("\"'")
    return ""


def test_architect_body_never_names_the_grading_lenses():
    _, body = _split_architect()
    hits = [token for token in FORBIDDEN if token in body]
    assert not hits, f"architect body names grading lenses: {hits}"


def test_architect_pins_an_allowed_model_tier():
    frontmatter, _ = _split_architect()
    assert "model: opus" in frontmatter or "model: fable" in frontmatter, (
        "the architect must pin an allowed blueprint model tier (opus or fable)"
    )


def test_architect_keeps_write_access():
    frontmatter, _ = _split_architect()
    assert "tools:" not in frontmatter, (
        "no tools: allowlist; a code-index MCP tool the user registered stays reachable"
    )
    for tool in ("Write", "Edit"):
        assert tool not in _disallowed_names(frontmatter), (
            f"disallowedTools names {tool}; the architect writes its own plan file"
        )
    assert "sandbox_mode: workspace-write" in frontmatter
    description = _description_value(frontmatter)
    size = len(description.encode("utf-8"))
    assert size <= 220, f"description is {size} bytes; the budget is 220"


def test_architect_links_the_skill_step():
    _, body = _split_architect()
    assert "fno:blueprint" in body
    assert "2a-bis" in body


def test_blueprint_substrate_launches_architect():
    text = (resolve_repo_root() / "skills" / "blueprint" / "SKILL.md").read_text(encoding="utf-8")
    for token in ("subagent_type: fno:architect", "--agent fno:architect", "agent_type: architect"):
        assert token in text, f"blueprint Substrate step 3 does not name the architect: {token}"
    assert "Use the Skill tool to run fno:blueprint with args" in text, (
        "the one-line subagent prompt changed"
    )
