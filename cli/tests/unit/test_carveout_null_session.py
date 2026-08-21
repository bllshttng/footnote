"""The default scope must not swallow rows that have no session_id at all.

`carveout add` mints an unscoped row ON PURPOSE when no session resolves, and
warns on stderr while doing it. Measured on this repo, 13 of 39 live rows carry
no `session_id`. A scoped default that filters on equality drops every one of
them the moment the reader DOES have a session, with no banner, because the
banner only covers the reader-has-no-session case.

The concrete break that motivates this file: file a carve-out before
`fno do target init`, then list after init in the same session, and `add` and
`list` no longer round-trip.
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
        {"id": "cv-unscoped", "ts": "2026-08-01T00:00:00Z", "session_id": None,
         "kind": "oos-bug", "priority": None, "need": None,
         "description": "filed before target init", "truncated": False},
        {"id": "cv-mine", "ts": "2026-08-02T00:00:00Z", "session_id": "sess-x",
         "kind": "oos-bug", "priority": None, "need": None,
         "description": "filed after init", "truncated": False},
        {"id": "cv-theirs", "ts": "2026-08-03T00:00:00Z", "session_id": "sess-y",
         "kind": "oos-bug", "priority": None, "need": None,
         "description": "another session", "truncated": False},
    ]
    path.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
    return path


def test_unscoped_rows_survive_the_scoped_default(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-x")
    result = runner.invoke(carveout_app, ["list"])
    assert result.exit_code == 0, result.output
    assert "cv-mine" in result.output
    # An ownerless row belongs to nobody, so scoping it away hides it from
    # EVERY session forever. It has to ride along with the scoped read.
    assert "cv-unscoped" in result.output
    # Another session's owned row is still filtered out.
    assert "cv-theirs" not in result.output


def test_add_then_list_round_trips_when_add_was_unscoped(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    """add before init, list after init, in one session."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    added = runner.invoke(
        carveout_app, ["add", "--kind", "oos-bug", "recorded with no session"]
    )
    assert added.exit_code == 0, added.output
    new_id = added.stdout.strip().splitlines()[-1].strip()

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-x")
    listed = runner.invoke(carveout_app, ["list"])
    assert listed.exit_code == 0, listed.output
    assert new_id in listed.output


def test_all_still_shows_every_row(ledger: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-x")
    result = runner.invoke(carveout_app, ["list", "--all"])
    assert result.exit_code == 0, result.output
    for cid in ("cv-unscoped", "cv-mine", "cv-theirs"):
        assert cid in result.output
