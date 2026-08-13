"""A scoped read always says it was scoped, especially when it finds nothing.

The no-session branch was banner-ed from the start. The session-RESOLVES branch
was not, so `fno carveout list` printed nothing at all when the current session
owned no rows - byte-identical to a clear ledger, while 39 rows sat in it. Same
absence-as-success trap, one branch over.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.carveout.cli import carveout_app

runner = CliRunner()


@pytest.fixture()
def ledger(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    path = tmp_path / ".fno" / "carveouts.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    rows = [
        {"id": f"cv-{i}", "ts": "2026-08-01T00:00:00Z", "session_id": "sess-theirs",
         "kind": "oos-bug", "priority": None, "need": None,
         "description": f"row {i}", "truncated": False}
        for i in range(3)
    ]
    path.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
    return path


def test_scoped_empty_read_is_never_silent(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    result = runner.invoke(carveout_app, ["list"])
    assert result.exit_code == 0, result.output
    # No rows of mine, and the reader must be able to tell that apart from an
    # empty ledger. Assert the positive marker: the scope and both counts.
    assert "scoped to session sess-mine" in result.output
    assert "0 of 3" in result.output
    assert "--all" in result.output


def test_scoped_nonempty_read_also_banners(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-theirs")
    result = runner.invoke(carveout_app, ["list"])
    assert result.exit_code == 0, result.output
    assert "3 of 3" in result.output
    assert "cv-0" in result.output


def test_all_does_not_banner(ledger: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    result = runner.invoke(carveout_app, ["list", "--all"])
    assert result.exit_code == 0, result.output
    assert "scoped to session" not in result.output
    assert "cv-0" in result.output
