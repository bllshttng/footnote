"""The annotate shim: every legacy spelling forwards and names its replacement (AC9)."""
from __future__ import annotations

from typer.testing import CliRunner

from fno.annotate.cli import annotate_app

runner = CliRunner()


def test_add_forwards_to_note_blocking(monkeypatch):
    from fno.graph import note_cli

    captured: dict = {}

    def fake_cmd_note(**kwargs):
        captured.update(kwargs)

    monkeypatch.setattr(note_cli, "cmd_note", fake_cmd_note)
    result = runner.invoke(
        annotate_app, ["add", "--message", "the bug", "--node", "x-1"]
    )
    assert result.exit_code == 0, result.output
    assert 'fno backlog note <node> "<text>" --blocking' in result.output
    assert captured["task_id"] == "x-1"
    assert captured["text"] == "the bug"
    assert captured["blocking"] is True


def test_add_forwards_block_fields(monkeypatch):
    from fno.graph import note_cli

    captured: dict = {}

    def fake_cmd_note(**kwargs):
        captured.update(kwargs)

    monkeypatch.setattr(note_cli, "cmd_note", fake_cmd_note)
    result = runner.invoke(
        annotate_app,
        [
            "add", "-m", "the bug", "--node", "x-1",
            "--block-cmd", "fno test",
            "--block-excerpt-file", "-",
        ],
    )
    assert result.exit_code == 0, result.output
    assert captured["block_cmd"] == "fno test"
    assert captured["block_excerpt_file"] == "-"


def test_list_forwards_to_notes_findings(monkeypatch):
    import subprocess

    import fno.annotate.cli as shim

    calls: list[list[str]] = []

    class Proc:
        returncode = 0

    def fake_run(argv, check=False):
        calls.append(argv)
        return Proc()

    monkeypatch.setattr(shim.subprocess, "run", fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: "/fake/fno-agents")
    result = runner.invoke(annotate_app, ["list", "--node", "x-1", "--json"])
    assert result.exit_code == 0, result.output
    assert "fno backlog notes findings [<node>]" in result.output
    assert calls[0][1:3] == ["backlog-notes", "findings"]
    assert "--node" in calls[0] and "x-1" in calls[0]
    assert "--json" in calls[0]


def test_resolve_forwards_to_note_resolve(monkeypatch):
    from fno.graph import note_cli

    captured: dict = {}

    def fake_cmd_note(**kwargs):
        captured.update(kwargs)

    monkeypatch.setattr(note_cli, "cmd_note", fake_cmd_note)
    result = runner.invoke(annotate_app, ["resolve", "abcd1234"])
    assert result.exit_code == 0, result.output
    assert "fno backlog note --resolve <finding-id>" in result.output
    assert captured["resolve"] == "abcd1234"
    assert captured["task_id"] is None
    assert captured["blocking"] is False


def test_no_source_still_imports_the_journal_writer():
    import pathlib

    repo = pathlib.Path(__file__).resolve().parents[2]
    assert not (repo / "cli" / "src" / "fno" / "annotate" / "core.py").exists(), (
        "annotate/core.py is retired; the findings store owns gate state now"
    )
