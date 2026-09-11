"""Tests for `fno agents king history` - the crown-scope reign readback.

Scope resolution and the native-read relay are Python; the journal scan
itself is the Rust king-history verb and is tested there. Covers explicit
and caller-derived scope, the refusal taxonomy, and the relay wiring.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno.king.history import HistoryUnreadable, canonicalize_scope, resolve_scope

SCOPE = "x-a792/fleet"


def _patch_caller(monkeypatch, row) -> None:
    from fno.agents import crown

    monkeypatch.setattr(crown, "calling_agent_row", lambda: row)


def test_canonicalize_passes_through_a_plain_scope() -> None:
    assert canonicalize_scope("x-a792") == "x-a792"


def test_caller_crown_resolves_when_no_scope_given(monkeypatch) -> None:
    _patch_caller(monkeypatch, SimpleNamespace(crown_scope=SCOPE))

    assert resolve_scope("") == SCOPE


def test_unresolvable_caller_crown_refuses(monkeypatch) -> None:
    from fno.agents.crown import AGENT_UNREGISTERED, REGISTRY_UNREADABLE

    for bad in (REGISTRY_UNREADABLE, AGENT_UNREGISTERED, SimpleNamespace(crown_scope="")):
        _patch_caller(monkeypatch, bad)
        with pytest.raises(HistoryUnreadable, match="--scope"):
            resolve_scope("")


def test_explicit_scope_beats_the_caller(monkeypatch) -> None:
    _patch_caller(monkeypatch, SimpleNamespace(crown_scope="other/epic"))

    assert resolve_scope(SCOPE) == SCOPE


def _patch_binary(monkeypatch, path) -> None:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: path)


def test_command_relays_the_native_read(tmp_path, monkeypatch) -> None:
    """The command hands scope, pinned journal path, and format to the binary."""
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
    assert "--events-path" in seen["argv"] and str(journal) in seen["argv"]
    assert "--json" in seen["argv"]
    assert '"matched": 0' in result.output


def test_command_refuses_when_binary_missing(monkeypatch) -> None:
    from typer.testing import CliRunner

    from fno.king.cli import agents_king_app

    _patch_binary(monkeypatch, None)

    result = CliRunner().invoke(agents_king_app, ["history", "--scope", SCOPE])

    assert result.exit_code == 127
    assert "binary" in result.output
