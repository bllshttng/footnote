"""Tests for ``fno backlog project-root`` (G2 unmapped-project detector).

The verb exposes the raw work-map lookup with NO cwd fallback, so the G2
session-project invariant can refuse an unmapped foreign wave by name
(AC2-ERR) instead of guessing. Mapped -> print root + exit 0; unmapped ->
empty stdout + exit 1.
"""
from __future__ import annotations

import os
import textwrap
from pathlib import Path
from unittest.mock import patch

from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _patch_candidates(cfg: Path):
    return patch("fno.graph._intake._settings_candidate_paths", return_value=[cfg])


def _write_workmap(tmp_path: Path) -> Path:
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: web
                  path: ~/code/web
    """))
    return cfg


def test_mapped_project_prints_root_exit_zero(tmp_path):
    cfg = _write_workmap(tmp_path)
    with _patch_candidates(cfg):
        result = runner.invoke(app, ["backlog", "project-root", "web"])
    assert result.exit_code == 0, result.output
    assert result.output.strip() == os.path.abspath(os.path.expanduser("~/code/web"))


def test_unmapped_project_exits_one_empty(tmp_path):
    cfg = _write_workmap(tmp_path)
    with _patch_candidates(cfg):
        result = runner.invoke(app, ["backlog", "project-root", "etl"])
    assert result.exit_code == 1
    assert result.output.strip() == ""  # nothing guessed
