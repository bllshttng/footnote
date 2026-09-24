"""The annotate spellings are retired: each refuses, naming its replacement (AC9, amended)."""
from __future__ import annotations

from typer.testing import CliRunner

from fno.annotate.cli import annotate_app

runner = CliRunner()


def test_add_refuses_and_names_the_replacement():
    result = runner.invoke(annotate_app, ["add", "--message", "the bug", "--node", "x-1"])
    assert result.exit_code == 2
    assert 'fno backlog note <node> "<text>" --blocking' in result.output


def test_list_refuses_and_names_the_replacement():
    result = runner.invoke(annotate_app, ["list", "--node", "x-1"])
    assert result.exit_code == 2
    assert "fno backlog notes findings [<node>]" in result.output


def test_resolve_refuses_and_names_the_replacement():
    result = runner.invoke(annotate_app, ["resolve", "abcd1234"])
    assert result.exit_code == 2
    assert "fno backlog note --resolve <finding-id>" in result.output


def test_no_source_still_imports_the_journal_writer():
    import pathlib

    repo = pathlib.Path(__file__).resolve().parents[2]
    assert not (repo / "cli" / "src" / "fno" / "annotate" / "core.py").exists(), (
        "annotate/core.py is retired; the findings store owns gate state now"
    )
