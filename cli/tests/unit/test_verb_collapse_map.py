from __future__ import annotations

import csv
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


def _load_callers():
    spec = importlib.util.spec_from_file_location("collapse_map_callers", CALLERS)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_map_covers_current_surface_once():
    rows = _rows()
    mapped = [row["current-leaf"] for row in rows]
    assert len(mapped) == len(set(mapped))
    assert set(mapped) == _leaves()


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
    leaves = _leaves()
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
    groups_with_dispatch = {
        row["current-leaf"].split()[0] for row in rows if row["tier"] == "T1"
    }
    kept = Counter(
        row["current-leaf"].split()[0]
        for row in rows
        if row["tier"] == "KEEP"
    )
    projected = len(groups_with_dispatch) + sum(kept.values())
    assert projected == 84
    assert projected <= 99
