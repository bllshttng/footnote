"""The verb x harness matrix is a projection of the capability table and each
skill's `requires.harness` frontmatter. These tests pin the cell rule on a
synthetic table and on five committed cells, so a table or frontmatter edit
that changes a verdict shows up here before the freshness gate does."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
RENDERER = REPO_ROOT / "scripts" / "diagnostics" / "render-harness-matrix.py"
TABLE = REPO_ROOT / "crates" / "fno-agents" / "src" / "harness_capabilities.toml"


@pytest.fixture(scope="module")
def renderer():
    spec = importlib.util.spec_from_file_location("render_harness_matrix", RENDERER)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_no_row_is_unmeasured(renderer) -> None:
    assert renderer.verb_cell(None, []) == "unmeasured"
    assert renderer.verb_cell({}, ["loop"]) == "unmeasured"


def test_refused_surface_is_absent_whatever_the_needs(renderer) -> None:
    row = {"command_surface": "refused", "loop_participation": "native"}
    assert renderer.verb_cell(row, []) == "absent"
    assert renderer.verb_cell(row, ["loop"]) == "absent"


def test_no_needs_reads_native_on_a_rendered_surface(renderer) -> None:
    assert renderer.verb_cell({"command_surface": "slash"}, []) == "native"


@pytest.mark.parametrize(
    ("participation", "extension", "expected"),
    [
        ("native", "", "native"),
        ("extension", "cli/src/fno/setup/assets/pi/footnote.ts", "native"),
        ("extension", "", "capable"),
        ("none", "", "absent"),
        (None, "", "unmeasured"),
    ],
)
def test_loop_need_maps_loop_participation(renderer, participation, extension, expected) -> None:
    row = {"command_surface": "slash", "loop_participation": participation, "loop_extension": extension}
    assert renderer.verb_cell(row, ["loop"]) == expected


def test_worst_need_wins(renderer) -> None:
    row = {
        "command_surface": "slash",
        "loop_participation": "native",
        "features": {"spawn": {"state": "capable"}},
    }
    assert renderer.verb_cell(row, ["loop", "spawn"]) == "capable"
    assert renderer.verb_cell(row, ["loop", "subagent_dispatch"]) == "unmeasured"


def test_out_of_vocabulary_need_refuses_naming_the_skill(renderer, tmp_path: Path) -> None:
    skill = tmp_path / "bogus" / "SKILL.md"
    skill.parent.mkdir()
    skill.write_text("---\nname: bogus\nrequires:\n  harness:\n    - telepathy\n---\n# bogus\n")
    with pytest.raises(SystemExit) as raised:
        renderer.skill_needs(tmp_path)
    assert "bogus" in str(raised.value) and "telepathy" in str(raised.value)


def test_committed_cells(renderer) -> None:
    """Five cells read off the committed table and skills. A change here is a
    real verdict change, not a rendering detail."""
    rendered = renderer.render_verb_matrix(TABLE)
    rows = {
        line.split("|")[1].strip(): [cell.strip().strip("`") for cell in line.split("|")[3:-1]]
        for line in rendered.splitlines()
        if line.startswith("| ") and not line.startswith("| verb")
    }
    harnesses = list(renderer.KNOWN_HARNESSES)
    target = dict(zip(harnesses, rows["target"]))
    assert target["claude"] == "native"
    assert target["gemini"] == "absent"
    assert target["cursor-agent"] == "capable"
    assert target["hermes"] == "unmeasured"
    assert dict(zip(harnesses, rows["pr"]))["opencode"] == "unmeasured"


def test_every_skill_is_a_row(renderer) -> None:
    rendered = renderer.render_verb_matrix(TABLE)
    names = {p.parent.name for p in (REPO_ROOT / "skills").glob("*/SKILL.md")}
    assert names, "positive control: the skills tree is not empty"
    for name in names:
        assert f"| {name} |" in rendered, name
