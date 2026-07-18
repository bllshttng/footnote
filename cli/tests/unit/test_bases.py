"""Unit tests for the canonical Base emitter (x-6c2b wave 2)."""
from __future__ import annotations

from typer.testing import CliRunner

from fno.cli import app
from fno.graph._bases import BASES, GENERATED_MARKER, write_base

runner = CliRunner()


def test_bases_verb_emits_files(tmp_path):
    """The `backlog bases --out DIR` verb is registered and emits both bases."""
    res = runner.invoke(app, ["backlog", "bases", "--out", str(tmp_path)])
    assert res.exit_code == 0, res.output
    assert (tmp_path / "epics.base").is_file()
    assert (tmp_path / "missions.base").is_file()
    assert "written" in res.output


def test_missions_base_filters_top_level_epics(tmp_path):
    """missions.base scopes to top-level epics (type epic AND no parent)."""
    write_base(tmp_path / "missions.base", BASES["missions.base"])
    text = (tmp_path / "missions.base").read_text(encoding="utf-8")
    assert 'type == "epic"' in text
    assert "parent == null" in text


def test_writes_marked_base(tmp_path):
    target = tmp_path / "epics.base"
    assert write_base(target, BASES["epics.base"]) == "written"
    text = target.read_text(encoding="utf-8")
    assert text.startswith(GENERATED_MARKER)
    assert 'type == "epic"' in text


def test_idempotent_second_write_unchanged(tmp_path):
    target = tmp_path / "epics.base"
    write_base(target, BASES["epics.base"])
    assert write_base(target, BASES["epics.base"]) == "unchanged"


def test_refuses_to_clobber_hand_authored(tmp_path):
    """A file without the generated marker is never overwritten."""
    target = tmp_path / "epics.base"
    target.write_text("filters:\n  and: []\n", encoding="utf-8")
    assert write_base(target, BASES["epics.base"]) == "refused"
    assert "GENERATED" not in target.read_text(encoding="utf-8")


def test_refreshes_stale_generated_file(tmp_path):
    target = tmp_path / "epics.base"
    target.write_text(GENERATED_MARKER + "\nstale: true\n", encoding="utf-8")
    assert write_base(target, BASES["epics.base"]) == "written"
    assert target.read_text(encoding="utf-8") == BASES["epics.base"]
