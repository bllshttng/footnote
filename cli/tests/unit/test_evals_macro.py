"""The macro verb delegates to the native fno-agents binary (the fold lives in Rust)."""
from __future__ import annotations

import subprocess
from pathlib import Path

from typer.testing import CliRunner

from fno.evals.cli import evals_app

runner = CliRunner()


def test_macro_refuses_without_the_binary(monkeypatch) -> None:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: None)
    result = runner.invoke(evals_app, ["macro"])
    assert result.exit_code == 2
    assert "fno-agents binary was not found" in result.output


def test_macro_forwards_flags_and_journals(monkeypatch, tmp_path: Path) -> None:
    import fno.paths
    from fno import rust_binary

    journal = tmp_path / "events.jsonl"
    journal.touch()
    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: tmp_path / "fno-agents")
    monkeypatch.setattr(fno.paths, "event_journals", lambda: [journal])
    captured: dict = {}

    def fake_run(argv, check=False):
        captured["argv"] = argv
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    result = runner.invoke(
        evals_app,
        ["macro", "--since", "7d", "--window", "9", "--all", "--json",
         "--topic", "termination:Budget"],
    )
    assert result.exit_code == 0
    argv = captured["argv"]
    assert argv[1] == "evals-macro"
    assert "--all" in argv and "--json" in argv
    assert argv[argv.index("--since") + 1] == "7d"
    assert argv[argv.index("--window") + 1] == "9"
    assert argv[argv.index("--topic") + 1] == "termination:Budget"
    assert argv[argv.index("--events") + 1] == str(journal)
