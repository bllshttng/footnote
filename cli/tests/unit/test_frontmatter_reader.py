"""Unit tests for _read_plan_frontmatter helper."""
from pathlib import Path

import pytest


def test_reads_single_file_plan(tmp_path):
    from fno.graph._intake import _read_plan_frontmatter
    plan = tmp_path / "plan-Y.md"
    plan.write_text("---\nproject: bar\n---\n# title\n")
    fm = _read_plan_frontmatter(str(plan))
    assert fm == {"project": "bar"}


def test_missing_index_returns_empty(tmp_path):
    from fno.graph._intake import _read_plan_frontmatter
    plan = tmp_path / "plan-empty"
    plan.mkdir()
    fm = _read_plan_frontmatter(str(plan))
    assert fm == {}


def test_malformed_yaml_returns_empty_with_warning(tmp_path, capsys):
    from fno.graph._intake import _read_plan_frontmatter
    plan = tmp_path / "plan-bad.md"
    plan.write_text("---\n: : :\n---\nbody\n")
    fm = _read_plan_frontmatter(str(plan))
    assert fm == {}
    err = capsys.readouterr().err
    assert "could not parse" in err
    assert str(plan) in err


def test_no_frontmatter_returns_empty(tmp_path, capsys):
    from fno.graph._intake import _read_plan_frontmatter
    plan = tmp_path / "plan-no-fm.md"
    plan.write_text("# Title\nNo frontmatter here.\n")
    fm = _read_plan_frontmatter(str(plan))
    assert fm == {}
    assert capsys.readouterr().err == ""


def test_nonexistent_path_returns_empty(tmp_path):
    from fno.graph._intake import _read_plan_frontmatter
    fm = _read_plan_frontmatter(str(tmp_path / "does-not-exist"))
    assert fm == {}


def test_unclosed_frontmatter_returns_empty(tmp_path):
    """A frontmatter that opens with --- but never closes is malformed."""
    from fno.graph._intake import _read_plan_frontmatter
    plan = tmp_path / "plan-unclosed.md"
    plan.write_text("---\nproject: foo\n# no closing fence\nbody body body\n")
    fm = _read_plan_frontmatter(str(plan))
    assert fm == {}
