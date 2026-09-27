"""The drafting lens table links every drafting lens in the blueprint skill by
a named condition, and nothing the planner reads links or names the judge's
grading lenses. A grader read during drafting turns into a checklist the
author writes to."""

from __future__ import annotations

import re
from pathlib import Path

from fno.paths import resolve_repo_root

LINK = re.compile(r"\]\(([^)]+)\)")
JUDGE_PATH = "lenses/judge"


def _blueprint() -> Path:
    return resolve_repo_root() / "skills" / "blueprint"


def _table() -> Path:
    return _blueprint() / "references" / "product-lenses.md"


def _draft() -> Path:
    return _blueprint() / "references" / "lenses" / "draft"


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
    table = _table()
    draft = _draft().resolve()
    rows = _rows(table)
    linked: set[Path] = set()
    for condition, cell in rows:
        assert condition, "a lens row has an empty Read when cell"
        match = LINK.search(cell)
        assert match, f"lens row {condition!r} carries no lens link"
        target = (table.parent / match.group(1)).resolve()
        assert target.parent == draft, f"row {condition!r} links outside {draft}: {target}"
        assert target.is_file(), f"lens row {condition!r} links a missing file: {target}"
        linked.add(target)
    assert len(linked) == len(rows), (
        f"{len(rows)} rows link only {len(linked)} distinct lens files; a row is duplicated"
    )
    for lens in sorted(draft.glob("*.md")):
        assert lens.resolve() in linked, f"orphan lens file with no table row: {lens.name}"


def test_drafting_lenses_never_link_a_grader():
    for match in LINK.finditer(_table().read_text(encoding="utf-8")):
        assert JUDGE_PATH not in match.group(1), (
            f"the drafting table links a grader path: {match.group(1)}"
        )
    for path in sorted(_draft().rglob("*.md")):
        for i, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            assert "VERDICT" not in line, f"{path.name}:{i} holds a VERDICT line"
            assert "EVIDENCE:" not in line, f"{path.name}:{i} holds an EVIDENCE: line"
            assert not line.startswith(("Fail", "Pass")), (
                f"{path.name}:{i} starts with Fail/Pass: grading prose in a drafting lens"
            )


def test_planner_files_never_name_the_judge_lenses():
    blueprint = _blueprint()
    files = [blueprint / "SKILL.md"]
    files += sorted(p for p in (blueprint / "references").rglob("*") if p.is_file())
    files += sorted((resolve_repo_root() / "agents").glob("*.md"))
    for path in files:
        text = path.read_text(encoding="utf-8")
        assert JUDGE_PATH not in text, f"{path} names the judge's lenses ({JUDGE_PATH})"


def test_lens_folders_are_not_skills():
    blueprint = _blueprint()
    for base in (blueprint / "lenses", blueprint / "references" / "lenses"):
        stray = sorted(base.rglob("SKILL.md"))
        assert not stray, f"a lens folder is a skill again, so it can load on its own: {stray}"
    for path in (resolve_repo_root() / "skills").rglob("*.md"):
        text = path.read_text(encoding="utf-8")
        assert "pm-node" not in text, f"{path} still names pm-node"
        assert "pm-epic" not in text, f"{path} still names pm-epic"


def test_drafting_lenses_are_attributed():
    for path in sorted(_draft().glob("*.md")):
        text = path.read_text(encoding="utf-8")
        assert "Source:" in text, f"{path.name} names no source"
        size = len(text.encode("utf-8"))
        assert size <= 800, f"{path.name} is {size} bytes; the budget is 800"
    notice = (resolve_repo_root() / "NOTICE").read_text(encoding="utf-8")
    for token in (
        "skills/blueprint/references/lenses/draft/",
        "skills/blueprint/lenses/judge/",
        "Pawel Huryn",
        "Every",
        "JimmySadek",
        "Permission is hereby granted",
    ):
        assert token in notice, f"NOTICE is missing {token}"
