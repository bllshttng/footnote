"""Wave 3.1: two-source triage picker (graph nodes + inbox fu-* items).

AC4-HP: `fno backlog triage context` surfaces both ab-* graph nodes and
unchecked fu-* inbox items in one payload, each labelled by id type.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

runner = CliRunner()


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    # See test_backlog_inbox_promote: guard against an upstream test leaving the
    # process cwd on a deleted tmp dir (which makes os.getcwd() raise).
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    yield
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()


def test_context_includes_inbox_items(tmp_path: Path) -> None:
    from fno.backlog.capture import add_item
    from fno.graph.triage import cli
    from fno.paths import inbox_path as resolve_inbox

    # Seed an inbox item.
    add_item(resolve_inbox(), title="small followup", source="PR#1", why="w", where="x", priority="p2")

    res = runner.invoke(cli, ["context"])
    assert res.exit_code == 0, res.output
    ctx = json.loads(res.stdout)
    assert "inbox_items" in ctx
    assert ctx["inbox_count"] == 1
    item = ctx["inbox_items"][0]
    assert item["id"].startswith("fu-")
    assert item["id_type"] == "fu"
    assert item["title"] == "small followup"


def test_context_inbox_empty_when_no_file(tmp_path: Path) -> None:
    from fno.graph.triage import cli
    res = runner.invoke(cli, ["context"])
    assert res.exit_code == 0, res.output
    ctx = json.loads(res.stdout)
    assert ctx["inbox_items"] == []
    assert ctx["inbox_count"] == 0
