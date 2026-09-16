"""The drafting lens table links every lens in the pm-plan-draft pack skill
by a named condition, and nothing in the drafting home grades a plan or
preloads a lens skill."""

from __future__ import annotations

import re
from pathlib import Path

from fno.paths import resolve_repo_root

LINK = re.compile(r"\]\(([^)]+)\)")


def _repo() -> Path:
    return resolve_repo_root()


def _rows(path: Path) -> list[tuple[str, str]]:
    rows: list[tuple[str, str]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not (line.startswith("|") and line.endswith("|")):
            continue
        cells = [cell.strip() for cell in line[1:-1].split("|")]
        if len(cells) != 2 or cells[0] == "Read when" or set(cells[0]) <= set("-: "):
            continue
        rows.append((cells[0], cells[1]))
    return rows


def test_every_lens_row_resolves_and_names_a_condition():
    table = _repo() / "skills" / "blueprint" / "references" / "product-lenses.md"
    lenses = _repo() / "skills" / "pm-plan-draft" / "lenses"
    rows = _rows(table)
    linked: set[Path] = set()
    for condition, cell in rows:
        assert condition, "a lens row has an empty Read when cell"
        match = LINK.search(cell)
        assert match, f"lens row {condition!r} carries no lens link"
        target = (table.parent / match.group(1)).resolve()
        assert "pm-plan-draft" in target.parts, (
            f"row {condition!r} links outside pm-plan-draft: {target}"
        )
        assert target.is_file(), f"lens row {condition!r} links a missing file: {target}"
        linked.add(target)
    assert len(linked) == len(rows), (
        f"{len(rows)} rows link only {len(linked)} distinct lens files; a row is duplicated"
    )
    for lens in sorted(lenses.glob("*.md")):
        assert lens.resolve() in linked, f"orphan lens file with no table row: {lens.name}"


def test_drafting_lenses_never_link_a_grader():
    table = _repo() / "skills" / "blueprint" / "references" / "product-lenses.md"
    for match in LINK.finditer(table.read_text(encoding="utf-8")):
        assert "pm-plan-review" not in match.group(1), (
            f"the drafting table links a grader path: {match.group(1)}"
        )
    draft = _repo() / "plugins" / "fno-pm" / "skills" / "pm-plan-draft"
    for path in sorted(draft.rglob("*.md")):
        for i, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            assert "VERDICT" not in line, f"{path.name}:{i} holds a VERDICT line"
            assert "EVIDENCE:" not in line, f"{path.name}:{i} holds an EVIDENCE: line"
            assert not line.startswith(("Fail", "Pass")), (
                f"{path.name}:{i} starts with Fail/Pass: grading prose in a drafting lens"
            )


def test_lens_skills_are_never_preloaded():
    for skill in ("pm-plan-draft", "pm-plan-review"):
        text = (_repo() / "plugins" / "fno-pm" / "skills" / skill / "SKILL.md").read_text(
            encoding="utf-8"
        )
        front = text.split("---", 2)[1]
        assert "disable-model-invocation: true" in front, f"{skill} is not non-invocable"
        assert "pack: fno-pm" in front, f"{skill} does not declare pack fno-pm"
    for agent in sorted((_repo() / "agents").glob("*.md")):
        front = agent.read_text(encoding="utf-8").split("---", 2)[1]
        for line in front.splitlines():
            assert not ("skills:" in line and "pm-plan" in line), (
                f"{agent.name} pins a pm-plan skill in its skills list"
            )
    for base in ("skills", "plugins/fno-pm"):
        for path in (_repo() / base).rglob("*.md"):
            text = path.read_text(encoding="utf-8")
            assert "pm-node" not in text, f"{path} still names pm-node"
            assert "pm-epic" not in text, f"{path} still names pm-epic"


def test_drafting_lenses_are_attributed():
    draft = _repo() / "plugins" / "fno-pm" / "skills" / "pm-plan-draft"
    for path in sorted((draft / "lenses").glob("*.md")):
        text = path.read_text(encoding="utf-8")
        assert "Source:" in text, f"{path.name} names no source"
        size = len(text.encode("utf-8"))
        assert size <= 800, f"{path.name} is {size} bytes; the budget is 800"
    notice = (_repo() / "NOTICE").read_text(encoding="utf-8")
    for token in (
        "pm-plan-draft",
        "Pawel Huryn",
        "Every",
        "JimmySadek",
        "Permission is hereby granted",
    ):
        assert token in notice, f"NOTICE is missing {token}"
