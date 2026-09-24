"""Tests for `fno agents king history` - the crown-scope reign readback.

The journal scan and the caller-crown scope resolution are the Rust
king-history verb (tested there); Python is the relay. Covers the relay
wiring and the refusal taxonomy.
"""
from __future__ import annotations

import pytest

SCOPE = "x-a792/fleet"


def _patch_binary(monkeypatch, path) -> None:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: path)


def test_command_relays_the_native_read(tmp_path, monkeypatch) -> None:
    """The command hands scope, EVERY resolved journal path, and format to the binary."""
    from typer.testing import CliRunner

    from fno.king.cli import agents_king_app

    journal = tmp_path / "events.jsonl"
    journal.write_text("", encoding="utf-8")
    seen: dict = {}
    stub = tmp_path / "stub-fno-agents"
    stub.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    stub.chmod(0o755)

    def fake_run(argv, **_kwargs):
        seen["argv"] = argv

        class Proc:
            returncode = 0
            stdout = '{"scope": "%s", "matched": 0}' % SCOPE
            stderr = ""

        return Proc()

    from fno.king import history as history_module

    _patch_binary(monkeypatch, str(stub))
    monkeypatch.setattr(history_module.subprocess, "run", fake_run)
    monkeypatch.setenv("FNO_EVENTS_PATH", str(journal))

    result = CliRunner().invoke(agents_king_app, ["history", "--scope", SCOPE, "--json"])

    assert result.exit_code == 0, result.output
    assert seen["argv"][1] == "king-history"
    assert "--scope" in seen["argv"] and SCOPE in seen["argv"]
    assert seen["argv"].count("--events-path") >= 1
    assert str(journal) in seen["argv"]
    assert "--json" in seen["argv"]
    assert '"matched": 0' in result.output


def test_command_refuses_when_binary_missing(monkeypatch) -> None:
    from typer.testing import CliRunner

    from fno.king.cli import agents_king_app

    _patch_binary(monkeypatch, None)

    result = CliRunner().invoke(agents_king_app, ["history", "--scope", SCOPE])

    assert result.exit_code == 127
    assert "binary" in result.output


def test_verdict_read_relays_scope_journals_and_json(tmp_path, monkeypatch) -> None:
    """The transport hands --cwd, the optional scope, EVERY journal, and the
    JSON flag; the verb assembles its own inputs (x-5952)."""
    journal = tmp_path / "events.jsonl"
    journal.write_text("", encoding="utf-8")
    seen: dict = {}
    stub = tmp_path / "stub-fno-agents"
    stub.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    stub.chmod(0o755)

    def fake_run(argv, **_kwargs):
        seen["argv"] = argv

        class Proc:
            returncode = 0
            stdout = "{}"
            stderr = ""

        return Proc()

    from fno.king import history as history_module

    _patch_binary(monkeypatch, str(stub))
    monkeypatch.setattr(history_module.subprocess, "run", fake_run)

    history_module.verdict_read([journal], SCOPE, as_json=True)

    argv = seen["argv"]
    assert argv[1] == "king-history"
    assert "--verdict" in argv
    assert "--cwd" in argv
    assert "--scope" in argv and SCOPE in argv
    assert str(journal) in argv
    assert "--json" in argv
    assert "--compaction-ceiling" not in argv
    assert "--inherited-undelivered" not in argv


def test_verdict_cmd_passes_the_native_render_through(monkeypatch) -> None:
    # The human words belong to the native renderer (x-5952): this shell
    # passes stdout and the exit code through, absent bounds named absent.
    from typer.testing import CliRunner

    from fno.king import history as history_module
    from fno.king.cli import agents_king_app

    def fake_run(argv, **_kwargs):
        class Proc:
            returncode = 0
            stdout = (
                "verdict: converging\n  iterations: 5 of 40 (within)\n"
                "  compactions: absent\n"
            )
            stderr = ""

        return Proc()

    monkeypatch.setattr(history_module.subprocess, "run", fake_run)

    result = CliRunner().invoke(agents_king_app, ["verdict"])

    assert result.exit_code == 0, result.output
    assert "compactions: absent" in result.output
    assert "8 recorded" not in result.output


def test_verdict_cmd_relays_a_native_failure(monkeypatch) -> None:
    # A failed read exits nonzero naming the cause and prints no verdict word
    # (AC3-ERR): the wrapper never substitutes defaults after a failed read.
    from typer.testing import CliRunner

    from fno.king import history as history_module
    from fno.king.cli import agents_king_app

    def fake_run(argv, **_kwargs):
        class Proc:
            returncode = 1
            stdout = ""
            stderr = "fno-agents king-verdict: scope x unreadable: graph corrupt"

        return Proc()

    monkeypatch.setattr(history_module.subprocess, "run", fake_run)

    result = CliRunner().invoke(agents_king_app, ["verdict"])

    assert result.exit_code != 0
    assert "verdict: converging" not in result.output
    assert "graph corrupt" in result.output
