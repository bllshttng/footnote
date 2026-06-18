"""Alias parity + event dual-read for the `backlog inbox` -> `backlog capture`
rename (ab-bf7cc0d8).

Covers:
  AC1b-HP  `fno backlog inbox X` is byte-identical to `fno backlog capture X`
           (same Typer app registered under both names)
  AC2-ERR  validation errors are identical through either spelling
  AC3-UI   `fno backlog --help` lists `capture` and hides `inbox`; the alias
           still renders its own help
  AC4-EDGE a session mixing legacy inbox_add and new capture_add rows counts
           both in capture-pass
  AC6-FR   capture-pass counts the capture_add the same binary just emitted
           (emit/reader coherence)
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
# AC1b-HP: alias parity through the parent app
# --------------------------------------------------------------------------

@pytest.mark.parametrize("spelling", ["capture", "inbox"])
def test_add_works_through_both_spellings(spelling: str) -> None:
    res = runner.invoke(
        _backlog_cli(),
        [spelling, "add", "parity item", "--source", "PR#1", "--why", "w"],
    )
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout.splitlines()[-1])
    assert payload["id"].startswith("fu-")


def test_alias_write_matches_canonical(tmp_path: Path) -> None:
    """Both spellings share one write path: distinct titles append
    identically-shaped blocks, and a same-(title, where) add through the ALIAS
    dedups against the item the canonical spelling captured (Phase 3 pre-check
    applies regardless of spelling)."""
    cli = _backlog_cli()
    from fno.paths import inbox_path

    res_cap = runner.invoke(
        cli, ["capture", "add", "same title", "--source", "PR#1", "--why", "w"]
    )
    assert res_cap.exit_code == 0, res_cap.output
    cap_payload = json.loads(res_cap.stdout.splitlines()[-1])

    # Alias add with a DIFFERENT title: appends a second, identically-shaped block.
    res_alias = runner.invoke(
        cli, ["inbox", "add", "other title", "--source", "PR#1", "--why", "w"]
    )
    assert res_alias.exit_code == 0, res_alias.output

    def norm(t: str) -> str:
        return re.sub(r"fu-[0-9a-f]{6}", "fu-XXXXXX", t)

    text = inbox_path().read_text(encoding="utf-8")
    blocks = [
        ln for ln in norm(text).splitlines() if ln.startswith("- [ ] fu-XXXXXX")
    ]
    assert blocks == [
        "- [ ] fu-XXXXXX - same title (p2)",
        "- [ ] fu-XXXXXX - other title (p2)",
    ]

    # Alias add of the SAME (title, where): dedups against the canonical's item.
    res_dup = runner.invoke(
        cli, ["inbox", "add", "same title", "--source", "PR#2", "--why", "w"]
    )
    assert res_dup.exit_code == 0, res_dup.output
    dup_payload = json.loads(res_dup.stdout.splitlines()[-1])
    assert dup_payload["deduped"] is True
    assert dup_payload["id"] == cap_payload["id"]


def test_list_parity_between_spellings() -> None:
    cli = _backlog_cli()
    add = runner.invoke(
        cli, ["capture", "add", "listed item", "--source", "PR#1", "--why", "w"]
    )
    assert add.exit_code == 0, add.output
    res_cap = runner.invoke(cli, ["capture", "list", "--json"])
    res_alias = runner.invoke(cli, ["inbox", "list", "--json"])
    assert res_cap.exit_code == res_alias.exit_code == 0
    assert res_cap.stdout == res_alias.stdout


# --------------------------------------------------------------------------
# AC2-ERR: validation parity
# --------------------------------------------------------------------------

def test_validation_error_parity() -> None:
    cli = _backlog_cli()
    args = ["add", "x", "--source", "[[feedback_x]]", "--why", "y"]
    res_cap = runner.invoke(cli, ["capture", *args])
    res_alias = runner.invoke(cli, ["inbox", *args])
    assert res_cap.exit_code == res_alias.exit_code == 2
    assert res_cap.output == res_alias.output


# --------------------------------------------------------------------------
# AC3-UI: help visibility
# --------------------------------------------------------------------------

def test_backlog_help_lists_capture_hides_inbox() -> None:
    res = runner.invoke(_backlog_cli(), ["--help"])
    assert res.exit_code == 0
    assert "capture" in res.output
    # `inbox` must not appear as a listed subcommand. It may appear inside
    # prose (e.g. another command's help text), so anchor on the command
    # column: a line starting with optional box-drawing + whitespace + the
    # bare word.
    listed = [
        ln for ln in res.output.splitlines()
        if re.match(r"^[\s|│]*inbox\s", ln)
    ]
    assert listed == [], f"alias leaked into help: {listed}"


def test_alias_help_still_renders() -> None:
    res = runner.invoke(_backlog_cli(), ["inbox", "--help"])
    assert res.exit_code == 0
    for verb in ("add", "list", "promote", "dismiss", "tidy"):
        assert verb in res.output


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
