"""Keep recording scripts aligned with the live Footnote command surface."""
from __future__ import annotations

import importlib.util
import re
import shlex
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
RECORDING_DIR = REPO_ROOT / "docs" / "recording"
SCRIPT = REPO_ROOT / "scripts" / "diagnostics" / "verb-callers.py"
TABLE_ROW = re.compile(
    r"^\|\s*(L\d+)\s*\|\s*([^|]+?)\s*\|\s*(cast|video)\s*\|"
    r"\s*`([^`]+)`\s*\|\s*(planned|scripted)\s*\|$"
)
SLASH_COMMAND = re.compile(r"(?<![\w-])/fno:([a-z][a-z0-9-]*)")


def _load_verb_callers():
    spec = importlib.util.spec_from_file_location("recording_verb_callers", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


vc = _load_verb_callers()


def _markdown_files(directory: Path = RECORDING_DIR) -> list[Path]:
    return sorted(directory.glob("*.md"))


def _live_typing_surface(leaves: set[str]) -> set[str]:
    allocation = REPO_ROOT / "scripts" / "ci" / "verb-collapse-map.tsv"
    current = {
        row.split("\t", 1)[0]
        for row in allocation.read_text().splitlines()[1:]
        if row.strip()
    }
    return leaves | current


def _run_fences(path: Path):
    lines = path.read_text().splitlines()
    index = 0
    while index < len(lines):
        if lines[index] != "```run":
            index += 1
            continue
        start = index + 1
        index += 1
        commands = []
        while index < len(lines) and lines[index] != "```":
            commands.append((index + 1, lines[index]))
            index += 1
        assert index < len(lines), f"{path}:{start}: unclosed run fence"
        index += 1
        next_index = index
        while next_index < len(lines) and not lines[next_index].strip():
            next_index += 1
        next_line = lines[next_index] if next_index < len(lines) else ""
        yield start, commands, next_index + 1, next_line


def _fno_resolution_errors(paths: list[Path], leaves: set[str]) -> list[str]:
    surface = _live_typing_surface(leaves)
    dispatchers = {
        leaf for leaf in leaves if any(item.startswith(f"{leaf} ") for item in surface)
    }
    errors = []
    for path in paths:
        for _, commands, _, _ in _run_fences(path):
            for line_number, command in commands:
                try:
                    words = shlex.split(command, comments=True)
                except ValueError as exc:
                    errors.append(f"{path}:{line_number}: cannot parse run line: {exc}")
                    continue
                if not words or words[0] not in {"fno", "fno-py", "fno-agents"}:
                    continue
                if any(
                    word in {"fno", "fno-py", "fno-agents"}
                    and words[index - 1] in {"&&", "||", ";", "|"}
                    for index, word in enumerate(words[1:], start=1)
                ):
                    errors.append(
                        f"{path}:{line_number}: multiple fno commands on one run line"
                    )
                    continue
                verb_words = words[1:]
                while verb_words and verb_words[0] in {"--json", "-J"}:
                    verb_words.pop(0)
                if words[0] == "fno-agents":
                    verb_words = ["agents", *verb_words]
                match = next(
                    (
                        " ".join(verb_words[:length])
                        for length in range(len(verb_words), 0, -1)
                        if " ".join(verb_words[:length]) in surface
                    ),
                    None,
                )
                consumed = len(match.split()) if match else 0
                has_unresolved_action = (
                    match in dispatchers
                    and len(verb_words) > consumed
                    and not verb_words[consumed].startswith("-")
                )
                if match is None or has_unresolved_action:
                    unresolved = " ".join(verb_words) or "<missing>"
                    errors.append(
                        f"{path}:{line_number}: unresolved fno leaf {unresolved!r}"
                    )
    return errors


def _slash_resolution_errors(paths: list[Path], root: Path = REPO_ROOT) -> list[str]:
    errors = []
    for path in paths:
        for _, commands, _, _ in _run_fences(path):
            for line_number, command in commands:
                for verb in SLASH_COMMAND.findall(command):
                    skill = root / "skills" / verb / "SKILL.md"
                    command_file = root / "commands" / f"{verb}.md"
                    if not skill.is_file() and not command_file.is_file():
                        errors.append(
                            f"{path}:{line_number}: unresolved slash command /fno:{verb}"
                        )
                        continue
                    try:
                        words = shlex.split(command)
                    except ValueError:
                        continue
                    if not words or words[0] != f"/fno:{verb}" or len(words) < 2:
                        continue
                    mode = words[1]
                    if not re.fullmatch(r"[a-z][a-z0-9-]*", mode):
                        continue
                    source = skill if skill.is_file() else command_file
                    hint = next(
                        (
                            line.split(":", 1)[1]
                            for line in source.read_text().splitlines()[:20]
                            if line.startswith("argument-hint:")
                        ),
                        "",
                    )
                    if not re.search(
                        rf"(?<![a-z0-9-]){re.escape(mode)}(?![a-z0-9-])", hint
                    ):
                        errors.append(
                            f"{path}:{line_number}: unresolved /fno:{verb} mode {mode!r}"
                        )
    return errors


def _output_contract_errors(paths: list[Path]) -> list[str]:
    errors = []
    for path in paths:
        for start, _, next_line_number, next_line in _run_fences(path):
            if next_line not in {"```expected", "[capture-at-record]"}:
                errors.append(
                    f"{path}:{start}: run fence has no expected output or "
                    f"[capture-at-record] at line {next_line_number}"
                )
                continue
            if next_line == "```expected":
                lines = path.read_text().splitlines()
                opening = next_line_number - 1
                closing = next(
                    (
                        index
                        for index in range(opening + 1, len(lines))
                        if lines[index] == "```"
                    ),
                    None,
                )
                if closing is None:
                    errors.append(
                        f"{path}:{next_line_number}: expected fence is unclosed"
                    )
                elif not any(line.strip() for line in lines[opening + 1 : closing]):
                    errors.append(f"{path}:{next_line_number}: expected fence is empty")
    return errors


def _coverage_errors(readme: Path, directory: Path) -> list[str]:
    rows = []
    for line_number, line in enumerate(readme.read_text().splitlines(), start=1):
        match = TABLE_ROW.match(line)
        if match:
            rows.append((line_number, *match.groups()))
    errors = []
    if len(rows) != 12:
        errors.append(f"{readme}: medium table has {len(rows)} rows; expected 12")
    listed = {file_name for _, _, _, _, file_name, _ in rows}
    for line_number, lesson, _, _, file_name, status in rows:
        if status == "scripted" and not (directory / file_name).is_file():
            errors.append(
                f"{readme}:{line_number}: {lesson} is scripted but {file_name} is missing"
            )
    for path in sorted(directory.glob("L*.md")):
        if path.name not in listed:
            errors.append(f"{path}: recording file has no medium-table row")
    return errors


def test_fno_verbs_resolve_against_live_surface(tmp_path):
    leaves = set(vc.load_leaves(REPO_ROOT))
    assert not _fno_resolution_errors(_markdown_files(), leaves)
    bad = tmp_path / "bad.md"
    bad.write_text("```run\nfno backlog no-such-leaf\n```\n\n[capture-at-record]\n")
    errors = _fno_resolution_errors([bad], leaves)
    assert errors == [f"{bad}:2: unresolved fno leaf 'backlog no-such-leaf'"]
    agents = tmp_path / "agents.md"
    agents.write_text("```run\nfno-agents loop-check\n```\n\n[capture-at-record]\n")
    assert not _fno_resolution_errors([agents], leaves)
    root_flag = tmp_path / "root-flag.md"
    root_flag.write_text(
        "```run\nfno --json backlog get demo-1234\n```\n\n[capture-at-record]\n"
    )
    assert not _fno_resolution_errors([root_flag], leaves)
    compound = tmp_path / "compound.md"
    compound.write_text(
        "```run\nfno status && fno backlog no-such-leaf\n```\n\n[capture-at-record]\n"
    )
    assert _fno_resolution_errors([compound], leaves) == [
        f"{compound}:2: multiple fno commands on one run line"
    ]
    foreign = tmp_path / "foreign.md"
    foreign.write_text("```run\ngit status\ngh pr view\n```\n\n```expected\nclean\n```\n")
    assert not _fno_resolution_errors([foreign], leaves)


def test_slash_commands_resolve_to_shipped_surface(tmp_path):
    assert not _slash_resolution_errors(_markdown_files())
    bad = tmp_path / "bad.md"
    bad.write_text("```run\n/fno:no-such-skill\n```\n\n[capture-at-record]\n")
    errors = _slash_resolution_errors([bad])
    assert errors == [f"{bad}:2: unresolved slash command /fno:no-such-skill"]
    bad_mode = tmp_path / "bad-mode.md"
    bad_mode.write_text("```run\n/fno:review no-such-mode\n```\n\n[capture-at-record]\n")
    errors = _slash_resolution_errors([bad_mode])
    assert errors == [
        f"{bad_mode}:2: unresolved /fno:review mode 'no-such-mode'"
    ]


def test_run_fences_have_captured_or_deferred_output(tmp_path):
    assert not _output_contract_errors(_markdown_files())
    bad = tmp_path / "bad.md"
    bad.write_text("```run\nfno status\n```\n\nprose instead of output\n")
    errors = _output_contract_errors([bad])
    assert errors == [
        f"{bad}:1: run fence has no expected output or [capture-at-record] at line 5"
    ]
    empty = tmp_path / "empty.md"
    empty.write_text("```run\nfno status\n```\n\n```expected\n```\n")
    assert _output_contract_errors([empty]) == [
        f"{empty}:5: expected fence is empty"
    ]
    unclosed = tmp_path / "unclosed.md"
    unclosed.write_text("```run\nfno status\n```\n\n```expected\nstatus\n")
    assert _output_contract_errors([unclosed]) == [
        f"{unclosed}:5: expected fence is unclosed"
    ]


def test_medium_table_covers_exactly_twelve_lessons(tmp_path):
    readme = RECORDING_DIR / "README.md"
    assert not _coverage_errors(readme, RECORDING_DIR)
    copied = tmp_path / "README.md"
    copied.write_text(readme.read_text())
    copied.write_text(
        copied.read_text() + "\n| L99 | Extra | cast | `L99-extra.md` | planned |\n"
    )
    assert "expected 12" in _coverage_errors(copied, tmp_path)[0]
    copied.write_text(readme.read_text().replace("| planned |", "| scripted |", 1))
    assert "is missing" in _coverage_errors(copied, tmp_path)[0]
    copied.write_text(readme.read_text())
    (tmp_path / "L99-extra.md").write_text("# Extra\n")
    assert any(
        "has no medium-table row" in error
        for error in _coverage_errors(copied, tmp_path)
    )
