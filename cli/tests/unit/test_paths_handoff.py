"""Tests for `fno paths handoff`.

The verb surfaces paths.handoffs_dir() so a session or hook resolves the canon
handoff doc through one door instead of composing a path. Filename key is the
session's canonical handle (first-8) unless --slug or --scope overrides. A
crown outlives its sessions, so --scope keys the doc on the crown scope and
returns the newest existing doc for that scope.
"""
from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.harness_identity import canonical_handle
from fno.paths import handoffs_dir


runner = CliRunner()
_ENV = {"COLUMNS": "240", "NO_COLOR": "1", "TERM": "dumb"}

# A uuid whose first-8 handle is unambiguous and not equal to its last-8.
SID = "c35abbca-bd2d-4407-8365-cf468baa7eea"
HANDLE = canonical_handle(SID)  # c35abbca (first-8), not 8baa7eea (last-8)


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    yield


def test_name_only_uses_canonical_handle_first_eight() -> None:
    result = runner.invoke(app, ["config", "paths", "handoff", "--session-id", SID, "--name-only"], env=_ENV)
    assert result.exit_code == 0, result.output
    name = result.output.strip()
    # Date prefix + first-8 handle. Guards the both-ends truncation hazard: the key must be the
    # head (c35abbca), never the tail (8baa7eea).
    assert re.fullmatch(r"\d{8}-[0-9a-f]{8}\.md", name), name
    assert name.endswith(f"-{HANDLE}.md"), name


def test_full_path_is_handoffs_dir_joined_with_filename() -> None:
    result = runner.invoke(app, ["config", "paths", "handoff", "--session-id", SID], env=_ENV)
    assert result.exit_code == 0, result.output
    full = Path(result.output.strip())
    name_only = runner.invoke(app, ["config", "paths", "handoff", "--session-id", SID, "--name-only"], env=_ENV).output.strip()
    assert full.name == name_only
    assert full.parent == handoffs_dir()


def test_slug_overrides_handle_key() -> None:
    result = runner.invoke(
        app, ["config", "paths", "handoff", "--session-id", SID, "--slug", "my-feature", "--name-only"], env=_ENV
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip().endswith("-my-feature.md")


def test_deprecated_session_alias_still_resolves() -> None:
    # --session is the hidden deprecated alias for --session-id; old call sites
    # (and the plan's original spec) keep working.
    result = runner.invoke(app, ["config", "paths", "handoff", "--session", SID, "--name-only"], env=_ENV)
    assert result.exit_code == 0, result.output
    assert result.output.strip().endswith(f"-{HANDLE}.md")


def test_scope_mints_a_dated_crown_keyed_name() -> None:
    result = runner.invoke(app, ["config", "paths", "handoff", "--scope", "x-a792", "--name-only"], env=_ENV)
    assert result.exit_code == 0, result.output
    name = result.output.strip()
    assert re.fullmatch(r"\d{8}-crown-x-a792\.md", name), name


def test_scope_returns_the_newest_existing_doc_for_that_crown() -> None:
    d = handoffs_dir()
    d.mkdir(parents=True, exist_ok=True)
    old = d / "20260901-crown-x-a792.md"
    new = d / "20260909-crown-x-a792.md"
    old.write_text("predecessor", encoding="utf-8")
    new.write_text("successor", encoding="utf-8")
    os.utime(old, (1_000_000, 1_000_000))
    os.utime(new, (2_000_000, 2_000_000))
    result = runner.invoke(app, ["config", "paths", "handoff", "--scope", "x-a792"], env=_ENV)
    assert result.exit_code == 0, result.output
    assert Path(result.output.strip()) == new


def test_scope_newest_ignores_other_scopes_and_session_keys() -> None:
    d = handoffs_dir()
    d.mkdir(parents=True, exist_ok=True)
    other_scope = d / "20260909-crown-x-other.md"
    session_key = d / "20260909-c35abbca.md"
    mine = d / "20260901-crown-x-a792.md"
    for p in (other_scope, session_key, mine):
        p.write_text("x", encoding="utf-8")
    os.utime(other_scope, (9_999_999, 9_999_999))
    os.utime(session_key, (9_999_999, 9_999_999))
    os.utime(mine, (1_000_000, 1_000_000))
    result = runner.invoke(app, ["config", "paths", "handoff", "--scope", "x-a792"], env=_ENV)
    assert result.exit_code == 0, result.output
    assert Path(result.output.strip()) == mine


def test_scope_sanitizes_a_portfolio_scope_into_the_key() -> None:
    # A portfolio crown stores its scope comma-joined; commas and spaces are
    # not filename-safe, so they collapse into the key separator.
    result = runner.invoke(
        app, ["config", "paths", "handoff", "--scope", "x-epic-a, x-epic-b", "--name-only"], env=_ENV
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip().endswith("-crown-x-epic-a-x-epic-b.md"), result.output


def test_scope_and_session_key_are_mutually_exclusive() -> None:
    result = runner.invoke(
        app, ["config", "paths", "handoff", "--scope", "x-a792", "--session-id", SID], env=_ENV
    )
    assert result.exit_code != 0
    result = runner.invoke(
        app, ["config", "paths", "handoff", "--scope", "x-a792", "--slug", "s"], env=_ENV
    )
    assert result.exit_code != 0
