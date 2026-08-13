"""Tests for the `fno carveout list` default scope.

`list` read the whole canonical ledger by default while `--session-id` /
`--pr-number` were opt-in, so the safe path was the one nobody took: an agent
pointed at `fno carveout list` saw a month of every other session's rows and
could not tell which were its own. The default flips to the current session.

The banner is the load-bearing part. A scoped read that silently returns empty
when no session resolves is the absence-as-success trap - the reader cannot
tell "no carve-outs" from "no session id, so nothing matched". So the
no-session case prints EVERY row plus a banner naming the count and the reason,
and the tests assert that banner by its own text.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.carveout.cli import carveout_app

runner = CliRunner()


def _row(cid: str, session_id: str | None, kind: str = "oos-bug") -> dict:
    return {
        "id": cid,
        "ts": "2026-08-01T00:00:00Z",
        "session_id": session_id,
        "kind": kind,
        "priority": None,
        "need": None,
        "description": f"{cid} left undone",
        "truncated": False,
    }


@pytest.fixture()
def ledger(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    # resolve_repo_root is @cache'd and gets warmed with the REAL worktree root
    # before this fixture's setenv lands, so without the clear the scoped read
    # filters on the live session id and returns nothing - a green-looking
    # empty that has nothing to do with the behavior under test.
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    path = tmp_path / ".fno" / "carveouts.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(
            json.dumps(r) + "\n"
            for r in (
                _row("cv-mine1", "sess-mine"),
                _row("cv-theirs", "sess-other"),
                _row("cv-mine2", "sess-mine", kind="deferred"),
                _row("cv-orphan", None),
            )
        ),
        encoding="utf-8",
    )
    return path


def test_scope_default_lists_only_this_session(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    result = runner.invoke(carveout_app, ["list"])
    assert result.exit_code == 0, result.output
    assert "cv-mine1" in result.output
    assert "cv-mine2" in result.output
    assert "cv-theirs" not in result.output
    assert "cv-orphan" not in result.output


def test_scope_default_banners_when_no_session_resolves(ledger: Path):
    """No session id: print everything AND say so, never an empty success."""
    result = runner.invoke(carveout_app, ["list"])
    assert result.exit_code == 0, result.output
    for cid in ("cv-mine1", "cv-theirs", "cv-mine2", "cv-orphan"):
        assert cid in result.output
    # Assert the banner by its own text: the count and the reason.
    assert "no active session" in result.output.lower()
    assert "4" in result.output


def test_scope_default_all_restores_the_unscoped_read(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    result = runner.invoke(carveout_app, ["list", "--all"])
    assert result.exit_code == 0, result.output
    for cid in ("cv-mine1", "cv-theirs", "cv-mine2", "cv-orphan"):
        assert cid in result.output


def test_scope_default_all_works_with_no_session_too(ledger: Path):
    result = runner.invoke(carveout_app, ["list", "--all"])
    assert result.exit_code == 0, result.output
    for cid in ("cv-mine1", "cv-theirs", "cv-mine2", "cv-orphan"):
        assert cid in result.output
    # --all is an explicit global read, so it does not need the banner.
    assert "no active session" not in result.output.lower()


def test_scope_default_explicit_session_id_still_wins(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    result = runner.invoke(carveout_app, ["list", "--session-id", "sess-other"])
    assert result.exit_code == 0, result.output
    assert "cv-theirs" in result.output
    assert "cv-mine1" not in result.output


def test_scope_default_json_mode_is_scoped_too(
    ledger: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    result = runner.invoke(carveout_app, ["list", "--json"])
    assert result.exit_code == 0, result.output
    ids = [json.loads(line)["id"] for line in result.stdout.splitlines() if line.strip()]
    assert sorted(ids) == ["cv-mine1", "cv-mine2"]
