"""Tests for `fno paths handoff`.

The verb surfaces paths.handoffs_dir() so a session or hook resolves the canon
handoff doc through one door instead of composing a path. Filename key is the
session's canonical handle (first-8) unless --slug overrides.
"""
from __future__ import annotations

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
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    for fn in ("_settings", "resolve_repo_root"):
        obj = getattr(paths_mod, fn, None)
        if obj is not None:
            try:
                obj.cache_clear()  # type: ignore[attr-defined]
            except AttributeError:
                pass
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    for fn in ("_settings", "resolve_repo_root"):
        obj = getattr(paths_mod, fn, None)
        if obj is not None:
            try:
                obj.cache_clear()  # type: ignore[attr-defined]
            except AttributeError:
                pass


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
