"""The verb x harness matrix is a projection of the capability table and each
skill's `requires.harness` frontmatter. These tests pin the cell rule on a
synthetic table and on committed cells, so a table or frontmatter edit that
changes a verdict shows up here before the freshness gate does."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
RENDERER = REPO_ROOT / "scripts" / "diagnostics" / "render-harness-matrix.py"
TABLE = REPO_ROOT / "crates" / "fno-agents" / "src" / "harness_capabilities.toml"
VOCAB = ["loop", "spawn", "subagent_dispatch", "claude", "grok"]


@pytest.fixture(scope="module")
def renderer():
    spec = importlib.util.spec_from_file_location("render_harness_matrix", RENDERER)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_no_row_is_unmeasured(renderer) -> None:
    assert renderer.verb_cell("hermes", None, []) == "unmeasured"
    assert renderer.verb_cell("hermes", {}, ["loop"]) == "unmeasured"


def test_refused_surface_is_absent_whatever_the_needs(renderer) -> None:
    row = {"command_surface": "refused", "loop_participation": "native"}
    assert renderer.verb_cell("gemini", row, []) == "absent"
    assert renderer.verb_cell("gemini", row, ["loop"]) == "absent"


def test_no_needs_reads_native_on_a_rendered_surface(renderer) -> None:
    assert renderer.verb_cell("pi", {"command_surface": "slash"}, []) == "native"


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
    assert renderer.verb_cell("pi", row, ["loop"]) == expected


def test_harness_name_is_an_identity_need(renderer) -> None:
    row = {"command_surface": "slash"}
    assert renderer.verb_cell("claude", row, ["claude"]) == "native"
    assert renderer.verb_cell("grok", row, ["claude"]) == "absent"


def test_worst_need_wins(renderer) -> None:
    row = {
        "command_surface": "slash",
        "loop_participation": "native",
        "features": {"spawn": {"state": "capable"}},
    }
    assert renderer.verb_cell("claude", row, ["loop", "spawn"]) == "capable"
    assert renderer.verb_cell("claude", row, ["loop", "subagent_dispatch"]) == "unmeasured"


def test_vocabulary_is_loop_plus_table_keys_plus_roster(renderer) -> None:
    table = {"probe": {"features.review": {}, "features.spawn": {}, "thread": {}}}
    vocab = renderer.need_vocabulary(table)
    assert vocab[:3] == ["loop", "review", "spawn"]
    assert "claude" in vocab and "thread" not in vocab


def _skill(tmp_path: Path, frontmatter: str) -> Path:
    skill = tmp_path / "bogus" / "SKILL.md"
    skill.parent.mkdir()
    skill.write_text(f"---\nname: bogus\n{frontmatter}---\n# bogus\n")
    return tmp_path


def test_out_of_vocabulary_need_refuses_naming_the_skill(renderer, tmp_path: Path) -> None:
    root = _skill(tmp_path, "requires:\n  harness:\n    - telepathy\n")
    with pytest.raises(SystemExit) as raised:
        renderer.skill_needs(root, VOCAB)
    assert "bogus" in str(raised.value) and "telepathy" in str(raised.value)


def test_scalar_need_refuses_naming_the_value(renderer, tmp_path: Path) -> None:
    root = _skill(tmp_path, "requires:\n  harness: loop\n")
    with pytest.raises(SystemExit) as raised:
        renderer.skill_needs(root, VOCAB)
    assert "must be a list" in str(raised.value) and "'loop'" in str(raised.value)


def test_crlf_frontmatter_still_reads_needs(renderer, tmp_path: Path) -> None:
    skill = tmp_path / "bogus" / "SKILL.md"
    skill.parent.mkdir()
    skill.write_bytes(b"---\r\nname: bogus\r\nrequires:\r\n  harness:\r\n    - loop\r\n---\r\n# bogus\r\n")
    assert renderer.skill_needs(tmp_path, VOCAB) == {"bogus": ["loop"]}


def test_committed_cells(renderer) -> None:
    """Cells read off the committed table and skills. A change here is a
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
    keepalive = dict(zip(harnesses, rows["cache-keepalive"]))
    assert keepalive["claude"] == "native" and keepalive["grok"] == "absent"


def test_every_skill_is_a_row(renderer) -> None:
    rendered = renderer.render_verb_matrix(TABLE)
    names = {p.parent.name for p in (REPO_ROOT / "skills").glob("*/SKILL.md")}
    assert names, "positive control: the skills tree is not empty"
    for name in names:
        assert f"| {name} |" in rendered, name
