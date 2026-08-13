from __future__ import annotations

import csv
import importlib
import importlib.util
from collections import Counter
from pathlib import Path


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
    assert len(mapped) == 322


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
