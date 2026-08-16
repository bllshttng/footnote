from __future__ import annotations

import csv
import importlib
import importlib.util
import re
from collections import Counter
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
BASELINE = REPO_ROOT / "scripts" / "ci" / "verb-baseline.txt"
MAP = REPO_ROOT / "scripts" / "ci" / "verb-collapse-map.tsv"
CALLERS = REPO_ROOT / "scripts" / "diagnostics" / "verb-callers.py"
PREAMBLE = (
    "AGENTS.md",
    "skills/using-fno/SKILL.md",
    ".claude/rules/worktrees.md",
    ".claude/rules/oss-fix-not-memory.md",
    "CLAUDE.md",
)


def _leaves() -> set[str]:
    return {
        line.split(" !", 1)[0].strip()
        for line in BASELINE.read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }


def _rows() -> list[dict[str, str]]:
    with MAP.open(newline="") as handle:
        return list(csv.DictReader(handle, delimiter="\t"))


def _mapped_leaves() -> set[str]:
    return {row["current-leaf"] for row in _rows()}


def _load_callers():
    spec = importlib.util.spec_from_file_location("collapse_map_callers", CALLERS)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_map_covers_current_surface_once():
    rows = _rows()
    mapped = [row["current-leaf"] for row in rows]
    assert len(mapped) == len(set(mapped))
    # Adding a CLI action means allocating its row in verb-collapse-map.tsv
    # BEFORE scripts/ci/verb-baseline.txt can regenerate, then bumping this
    # count. Nothing else states that order, and the regeneration refuses
    # without the row.
    #
    # This number is a two-writer collision waiting to happen: main and a
    # feature branch each bump it to the same value from the same base, so the
    # texts conflict while both are individually right. Resolve by COUNTING the
    # merged rows, never by taking either side. `worktree reapable` and
    # `agents orphans` both landed at 323 and the truth is 324.
    assert len(mapped) == 329, (
        f"{len(mapped)} rows in verb-collapse-map.tsv; bump this count when a "
        "new CLI action is deliberately allocated a row"
    )


def test_map_matches_the_uncollapsed_click_action_inventory():
    import click
    import typer

    from fno.cli import COLLAPSE_KEEP, LAZY_SUBCOMMANDS
    from fno.lint_verb_ratchet import _iter_group_leaves

    live: set[str] = set()
    for group in COLLAPSE_KEEP:
        import_path = LAZY_SUBCOMMANDS[group][0]
        module_name, _, attr_name = import_path.rpartition(":")
        obj = getattr(importlib.import_module(module_name), attr_name)
        command = typer.main.get_command(obj)
        context = click.Context(command, info_name=group)
        live.update(path for path, _sub in _iter_group_leaves(command, context, group))

    mapped = {leaf for leaf in _mapped_leaves() if leaf.split()[0] in COLLAPSE_KEEP}
    assert live == mapped


def test_t1_preserves_the_pre_collapse_typing_string():
    for row in _rows():
        if row["tier"] == "T1":
            assert row["post-collapse-typing"] == row["current-leaf"]


def test_every_non_t1_row_carries_a_reason_and_reference_cost():
    for row in _rows():
        assert row["tier"] in {"T1", "T2", "T3", "KEEP"}
        assert row["refs"].isdigit()
        if row["tier"] != "T1":
            assert row["reason-if-not-T1"].strip()


def test_all_preamble_named_leaves_are_t1_or_keep():
    callers = _load_callers()
    leaves = _mapped_leaves()
    counts = callers.sweep(
        REPO_ROOT,
        leaves,
        binary_form=True,
        pipe_fan=True,
        paths=(REPO_ROOT / path for path in PREAMBLE),
    )
    tiers = {row["current-leaf"]: row["tier"] for row in _rows()}
    named = {leaf for leaf, count in counts.items() if count}
    assert named
    assert {tiers[leaf] for leaf in named} <= {"T1", "KEEP"}


def test_allocation_projects_no_more_than_99_registered_leaves():
    rows = _rows()
    groups_with_dispatch = {row["current-leaf"].split()[0] for row in rows if row["tier"] == "T1"}
    kept = Counter(row["current-leaf"].split()[0] for row in rows if row["tier"] == "KEEP")
    projected = len(groups_with_dispatch) + sum(kept.values())
    assert projected == 84
    assert projected <= 99


def test_live_baseline_matches_the_projected_allocation():
    leaves = _leaves()
    rows = _rows()
    mapped_groups = {row["current-leaf"].split()[0] for row in rows}
    projected = {
        row["current-leaf"] if row["tier"] == "KEEP" else row["current-leaf"].split()[0]
        for row in rows
    }
    assert {leaf for leaf in leaves if leaf.split()[0] in mapped_groups} == projected
    assert len(leaves) <= 99
    assert "fno-agents" in leaves


def test_runtime_keep_registry_matches_the_checked_in_allocation():
    from fno.cli import COLLAPSE_KEEP

    expected: dict[str, set[str]] = {group: set() for group in COLLAPSE_KEEP}
    group_sizes = Counter(row["current-leaf"].split()[0] for row in _rows())
    for row in _rows():
        tokens = row["current-leaf"].split()
        if row["tier"] == "KEEP" and group_sizes[tokens[0]] > 1:
            expected[tokens[0]].add(tokens[1])
    assert COLLAPSE_KEEP == expected


def test_each_python_group_dispatcher_reaches_the_original_action_command():
    import click
    import typer

    from fno._lazy_group import collapse_click_group
    from fno.cli import COLLAPSE_KEEP, LAZY_SUBCOMMANDS

    rows = _rows()
    for group, keep in COLLAPSE_KEEP.items():
        first_t1 = next(
            row for row in rows if row["tier"] == "T1" and row["current-leaf"].split()[0] == group
        )
        action_name = first_t1["current-leaf"].split()[1]
        import_path = LAZY_SUBCOMMANDS[group][0]
        module_name, _, attr_name = import_path.rpartition(":")
        obj = getattr(importlib.import_module(module_name), attr_name)
        original = typer.main.get_command(obj)
        original_ctx = click.Context(original, info_name=group)
        destination = original.get_command(original_ctx, action_name)
        assert destination is not None, first_t1["current-leaf"]

        collapsed = collapse_click_group(original, keep=set(keep))
        collapsed_ctx = click.Context(collapsed, info_name=group)
        _, adapter, residual = collapsed.resolve_command(
            collapsed_ctx, first_t1["current-leaf"].split()[1:]
        )

        assert adapter is not None
        assert adapter._action.name == destination.name
        assert type(adapter._action) is type(destination)
        assert [parameter.name for parameter in adapter._action.params] == [
            parameter.name for parameter in destination.params
        ]
        if destination.callback is None:
            assert adapter._action.callback is None
        else:
            assert adapter._action.callback.__code__ is destination.callback.__code__
        assert residual == first_t1["current-leaf"].split()[2:]


FLAGS = REPO_ROOT / "scripts" / "ci" / "verb-collapse-flags.txt"
LEGAL_TIERS = {"T1", "T2", "T3", "KEEP"}


def _fake_root(tmp_path: Path, *, map_text: str, flags_text: str) -> Path:
    """A repo root whose ci/ files are ours and whose fno source is the real one.

    The enumerator refuses when the imported package is not the checkout's
    source, so the symlink is load-bearing: it makes ``root/cli/src/fno``
    resolve to the package already imported here.
    """
    (tmp_path / "cli" / "src").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno").symlink_to(REPO_ROOT / "cli" / "src" / "fno")
    ci = tmp_path / "scripts" / "ci"
    ci.mkdir(parents=True)
    (ci / "verb-collapse-map.tsv").write_text(map_text)
    (ci / "verb-collapse-flags.txt").write_text(flags_text)
    return tmp_path


def _raise_message(tmp_path, monkeypatch, *, map_text: str, flags_text: str) -> str:
    from fno import lint_verb_ratchet as lint

    root = _fake_root(tmp_path, map_text=map_text, flags_text=flags_text)
    monkeypatch.setattr(lint, "_repo_root", lambda: root)
    with pytest.raises(lint.VerbRatchetError) as excinfo:
        list(lint.iter_python_leaves())
    return str(excinfo.value)


def test_map_refusal_carries_a_parseable_template_row(tmp_path, monkeypatch) -> None:
    """The refusal must hand over the row format, not only the instruction.

    Two people hit this gate cold within an hour and both reverse-engineered
    the columns by reading the file. Asserting only that it raises passes just
    as happily on the message that taught them nothing, so this parses the
    template the message carries and checks it against the file's own header.
    """
    header = MAP.read_text(encoding="utf-8").splitlines()[0]
    columns = header.split("\t")
    message = _raise_message(
        tmp_path,
        monkeypatch,
        map_text=header + "\n",
        flags_text=FLAGS.read_text(encoding="utf-8"),
    )

    rows = [
        line.strip()
        for line in message.splitlines()
        if len(line.split("\t")) == len(columns)
    ]
    assert len(rows) == 1, (
        f"the refusal must carry exactly one {len(columns)}-column template row, "
        f"found {len(rows)}"
    )
    leaf, tier, typing, refs, *_rest = rows[0].split("\t")
    assert leaf.split()[0], "the template needs a group-qualified action"
    assert tier in LEGAL_TIERS
    assert typing == leaf, "a T1 template shows the typing unchanged"
    assert refs.isdigit(), "refs is a reference count"
    for column in columns:
        assert column in message, f"the refusal never glosses the {column!r} column"
    assert "verb-callers.py" in message, "say where the refs count comes from"


def test_flags_refusal_says_what_to_do_about_each_side(tmp_path, monkeypatch) -> None:
    """The hidden-option gate reported two set differences and stopped there."""
    message = _raise_message(
        tmp_path,
        monkeypatch,
        map_text=MAP.read_text(encoding="utf-8"),
        flags_text="# emptied by the test\n",
    )

    templates = [
        line.strip()
        for line in message.splitlines()
        if re.fullmatch(r"\s*[a-z][a-z0-9 -]* !--[a-z0-9-]+", line)
    ]
    assert len(templates) == 1, (
        "the refusal must carry exactly one `<action> !--<option>` template, "
        f"found {templates}"
    )
    assert "add that line" in message and "delete that line" in message, (
        "code-only and file-only must each say which way to fix the file"
    )
