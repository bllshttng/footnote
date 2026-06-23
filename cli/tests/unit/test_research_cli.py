"""Tests for the `fno research` CLI ship-step wiring (US3, AC1/AC5).

`run_round` is stubbed (no network); the test exercises the deliver wiring:
default-deliver writes the brief to config.research.output_dir, and an unset
output_dir fails loud (exit 5).
"""
from __future__ import annotations

from pathlib import Path

import pytest
import typer
from typer.testing import CliRunner

from fno.research import cli as research_cli
from fno.research.core import RoundResult, Source


def _app() -> typer.Typer:
    app = typer.Typer()
    app.command()(research_cli.research_command)
    return app


def _stub_round(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, note: str = "") -> Path:
    """Point run_round at a cache sidecar with one verified row; return its path."""
    cache = tmp_path / "cache" / "topic-words-here.sources.jsonl"
    cache.parent.mkdir(parents=True, exist_ok=True)
    if not note:
        cache.write_text(
            Source(url="https://a.example/p", fetched_at="t", hash="h", extract="finding", verified=True).to_json_line()
            + "\n",
            encoding="utf-8",
        )
    else:
        cache.touch()

    def fake_round(topic, **kw):
        return RoundResult(topic, "topic-words-here", str(cache), 1, 1, 0, note=note)

    monkeypatch.setattr(research_cli, "run_round", fake_round)
    return cache


def test_cli_delivers_to_output_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1: default ship writes the brief to config.research.output_dir; DoneAdvisory."""
    _stub_round(tmp_path, monkeypatch)
    out = tmp_path / "out"
    monkeypatch.setattr(research_cli, "_output_dir", lambda: str(out))

    res = CliRunner().invoke(_app(), ["topic words here", "--no-claim"])
    assert res.exit_code == 0, res.output
    assert "DoneAdvisory" in res.output
    assert (out / "topic-words-here.md").is_file()
    assert (out / "topic-words-here.sources.jsonl").is_file()


def test_cli_unset_output_dir_fails_loud(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC5: deliver with no output_dir exits 5 (never guesses)."""
    _stub_round(tmp_path, monkeypatch)
    monkeypatch.setattr(research_cli, "_output_dir", lambda: None)

    res = CliRunner().invoke(_app(), ["topic words here", "--no-claim"])
    assert res.exit_code == 5, res.output
    assert "output_dir" in res.output


def test_cli_no_deliver_keeps_retrieve_only(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """--no-deliver preserves the Group-1 cache-only behavior (no output_dir needed)."""
    _stub_round(tmp_path, monkeypatch)
    monkeypatch.setattr(research_cli, "_output_dir", lambda: None)

    res = CliRunner().invoke(_app(), ["topic words here", "--no-claim", "--no-deliver"])
    assert res.exit_code == 0, res.output
    assert "DoneAdvisory" not in res.output


def test_cli_no_sources_still_ships(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC3: a no-sources round still ships a stamped brief, DoneAdvisory."""
    _stub_round(tmp_path, monkeypatch, note="no sources found")
    out = tmp_path / "out"
    monkeypatch.setattr(research_cli, "_output_dir", lambda: str(out))

    res = CliRunner().invoke(_app(), ["topic words here", "--no-claim"])
    assert res.exit_code == 0, res.output
    assert (out / "topic-words-here.md").is_file()
    assert "no sources found" in (out / "topic-words-here.md").read_text().lower()
