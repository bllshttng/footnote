"""The architect agent body points the planner at drafting lenses, never at
the grading lenses. A grader read during drafting turns into a checklist
the author writes to, so the body may name neither the lens files nor the
pm-plan-review skill."""

from __future__ import annotations

from fno.paths import resolve_repo_root

FORBIDDEN = ("lenses.md", "lenses/", "pm-plan-review", "pm-plan-draft")


def _split_architect() -> tuple[str, str] | None:
    path = resolve_repo_root() / "agents" / "architect.md"
    if not path.is_file():
        return None
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
    split = _split_architect()
    if split is None:
        # The architect agent ships with the fno-pm pack work; the guard
        # activates the moment that file lands.
        return
    _, body = split
    hits = [token for token in FORBIDDEN if token in body]
    assert not hits, f"architect body names grading lenses: {hits}"


def test_architect_pins_an_allowed_model_tier():
    split = _split_architect()
    if split is None:
        return
    frontmatter, _ = split
    assert "model: opus" in frontmatter or "model: fable" in frontmatter, (
        "the architect must pin an allowed blueprint model tier (opus or fable)"
    )


def test_architect_keeps_write_access():
    split = _split_architect()
    if split is None:
        return
    frontmatter, _ = split
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


def test_architect_has_no_skills_key():
    split = _split_architect()
    if split is None:
        return
    frontmatter, _ = split
    assert "skills:" not in frontmatter, (
        "a skills list loads nothing under claude --agent, drops unresolved"
        " entries silently in a subagent, and would preload; ship no skills key"
    )


def test_architect_links_the_skill_step():
    split = _split_architect()
    if split is None:
        return
    _, body = split
    assert "fno:blueprint" in body
    assert "2a-bis" in body
