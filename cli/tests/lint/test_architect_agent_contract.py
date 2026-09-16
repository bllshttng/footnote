"""The architect agent body points the planner at drafting lenses, never at
the grading lenses. A grader read during drafting turns into a checklist
the author writes to, so the body may name neither the lens files nor the
pm-plan-review skill."""

from __future__ import annotations

from fno.paths import resolve_repo_root

FORBIDDEN = ("lenses.md", "pm-plan-review")


def test_architect_body_never_names_the_grading_lenses():
    architect = resolve_repo_root() / "agents" / "architect.md"
    if not architect.is_file():
        # The architect agent ships with the fno-pm pack work; the guard
        # activates the moment that file lands.
        return
    body = architect.read_text(encoding="utf-8")
    hits = [token for token in FORBIDDEN if token in body]
    assert not hits, f"architect body names grading lenses: {hits}"
