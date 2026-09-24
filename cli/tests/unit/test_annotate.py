"""The annotate shim: every legacy spelling rewrites its argv and names its replacement (AC9)."""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.annotate.cli import annotate_app

runner = CliRunner()


@pytest.fixture()
def forwarded(monkeypatch):
    """Capture the rewritten argv instead of execing the entrypoint."""
    calls: list[list[str]] = []
    monkeypatch.setattr("fno.annotate.cli.os.execvp", lambda prog, argv: calls.append(argv))
    return calls


def test_add_rewrites_to_note_blocking(forwarded):
    result = runner.invoke(annotate_app, ["add", "--message", "the bug", "--node", "x-1"])
    assert result.exit_code == 0, result.output
    assert 'fno backlog note <node> "<text>" --blocking' in result.output
    assert forwarded[0][1:] == ["backlog", "note", "x-1", "the bug", "--blocking"]


def test_add_passes_block_fields(forwarded):
    result = runner.invoke(
        annotate_app,
        [
            "add", "-m", "the bug", "--node", "x-1",
            "--block-cmd", "fno test",
            "--block-excerpt-file", "-",
        ],
    )
    assert result.exit_code == 0, result.output
    assert forwarded[0][1:] == [
        "backlog", "note", "x-1", "the bug", "--blocking",
        "--block-cmd", "fno test", "--block-excerpt-file", "-",
    ]


def test_list_rewrites_to_notes_findings(forwarded):
    result = runner.invoke(annotate_app, ["list", "--node", "x-1", "--json"])
    assert result.exit_code == 0, result.output
    assert "fno backlog notes findings [<node>]" in result.output
    assert forwarded[0][1:] == ["backlog", "notes", "findings", "--node", "x-1", "--json"]


def test_resolve_rewrites_to_note_resolve(forwarded):
    result = runner.invoke(annotate_app, ["resolve", "abcd1234"])
    assert result.exit_code == 0, result.output
    assert "fno backlog note --resolve <finding-id>" in result.output
    assert forwarded[0][1:] == ["backlog", "note", "--resolve", "abcd1234"]


def test_no_source_still_imports_the_journal_writer():
    import pathlib

    repo = pathlib.Path(__file__).resolve().parents[2]
    assert not (repo / "cli" / "src" / "fno" / "annotate" / "core.py").exists(), (
        "annotate/core.py is retired; the findings store owns gate state now"
    )
