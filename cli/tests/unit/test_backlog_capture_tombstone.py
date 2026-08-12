"""The removed `backlog inbox` alias, and the legacy event vocabulary it left.

`inbox` was a second registration of the `capture` Typer app: nine subcommands,
byte-identical behaviour, and nine baseline lines the surface paid for twice.
It is gone, and what remains here splits cleanly in two.

  TOMBSTONE   reaching for the removed spelling must name its replacement, not
              fail the way a typo fails.
  DUAL-READ   the alias is gone but its EVENTS are not. Sessions recorded
              `inbox_add` rows before the rename and those rows still have to
              count, so the reader vocabulary outlives the verb. Deleting these
              with the alias would have silently broken the capture-pass gate
              for any session that predates it.
"""
from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

runner = CliRunner()


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()


def _backlog_cli():
    from fno.graph.cli import cli
    return cli


# --------------------------------------------------------------------------
# The tombstone, and help visibility
# --------------------------------------------------------------------------

def test_removed_inbox_spelling_names_its_replacement() -> None:
    """The removed alias teaches; a genuine typo still gets the generic error.

    Both halves are asserted. Without the second, a tombstone table that
    swallowed EVERY unknown verb would pass the first and hide real typos
    behind a confident wrong answer.
    """
    res = runner.invoke(_backlog_cli(), ["inbox", "list"])
    assert res.exit_code != 0
    assert "was removed" in res.output
    assert "fno backlog capture" in res.output

    typo = runner.invoke(_backlog_cli(), ["inbocks", "list"])
    assert typo.exit_code != 0
    assert "was removed" not in typo.output


def test_backlog_capture_hidden_but_invocable() -> None:
    """x-71b6 tiering: `capture` is hidden from the advertised backlog menu,
    but stays fully invocable."""
    # capture is invocable (its own --help works) even though hidden.
    cap = runner.invoke(_backlog_cli(), ["capture", "--help"])
    assert cap.exit_code == 0, cap.output

    res = runner.invoke(_backlog_cli(), ["--help"])
    assert res.exit_code == 0
    # Neither the hidden sub-app nor its alias appears as a listed command row.
    # (Either word may still occur inside another command's prose, so anchor on
    # the command column: optional box-drawing + whitespace + the bare word.)
    for verb in ("capture", "inbox"):
        listed = [
            ln for ln in res.output.splitlines()
            if re.match(rf"^[\s|│]*{verb}\s", ln)
        ]
        assert listed == [], f"hidden {verb!r} leaked into help: {listed}"


# --------------------------------------------------------------------------
# AC4-EDGE + AC6-FR: event dual-read
# --------------------------------------------------------------------------

def _write_event(events_path: Path, etype: str, session_id: str) -> None:
    events_path.parent.mkdir(parents=True, exist_ok=True)
    with events_path.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps({"type": etype, "data": {"session_id": session_id}}) + "\n")


def test_capture_pass_counts_mixed_vocabulary(tmp_path: Path) -> None:
    """AC4-EDGE: one legacy inbox_add + one capture_add for the session => 2."""
    events = tmp_path / ".fno" / "events.jsonl"
    _write_event(events, "inbox_add", "SMIX")
    _write_event(events, "capture_add", "SMIX")
    # An unrelated session's event must not count.
    _write_event(events, "capture_add", "OTHER")

    res = runner.invoke(
        _backlog_cli(), ["capture", "capture-pass", "--session-id", "SMIX"]
    )
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout.splitlines()[-1])
    assert payload["entries_written"] == 2


def test_capture_pass_counts_legacy_only_session(tmp_path: Path) -> None:
    """Boundary: a session whose rows were ALL written by a pre-rename binary
    still seals the gate."""
    events = tmp_path / ".fno" / "events.jsonl"
    _write_event(events, "inbox_add", "SOLD")
    res = runner.invoke(
        _backlog_cli(), ["capture", "capture-pass", "--session-id", "SOLD"]
    )
    assert res.exit_code == 0, res.output
    assert json.loads(res.stdout.splitlines()[-1])["entries_written"] == 1


def test_emit_reader_coherence(tmp_path: Path) -> None:
    """AC6-FR: capture-pass counts the capture_add THIS binary just emitted.
    Regression-pins the emit-flips-but-reader-doesn't failure mode."""
    cli = _backlog_cli()
    state = tmp_path / ".fno" / "target-state.md"
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text("---\nsession_id: SCOH\n---\n", encoding="utf-8")

    add = runner.invoke(
        cli, ["capture", "add", "coherent", "--source", "PR#1", "--why", "w"]
    )
    assert add.exit_code == 0, add.output
    res = runner.invoke(cli, ["capture", "capture-pass"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.stdout.splitlines()[-1])["entries_written"] == 1


def test_empty_pass_read_back_accepts_new_vocabulary(tmp_path: Path) -> None:
    """The empty-pass fail-loud read-back finds the capture_empty_pass row the
    same invocation wrote (no false 'event did not land')."""
    res = runner.invoke(
        _backlog_cli(),
        ["capture", "empty-pass", "--reason", "nothing", "--session-id", "SEMP"],
    )
    assert res.exit_code == 0, res.output
    events = tmp_path / ".fno" / "events.jsonl"
    types = [
        json.loads(l)["type"] for l in events.read_text().splitlines() if l.strip()
    ]
    assert "capture_empty_pass" in types
