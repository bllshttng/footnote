"""`fno carveout update` - correct a carve-out without changing its identity.

The verb exists because the old correction path was `resolve` then `add`, which
has two defects a test can pin. It minted a new id, so an id already quoted in a
PR body or a mail became a dead pointer. And it was lossy: two writes, and a
failure between them left the ledger holding neither row. That happened live.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.carveout.core import (
    CarveoutError,
    CarveoutNotFound,
    add_carveout,
    read_carveouts,
    update_carveout,
)
from fno.cli import app

runner = CliRunner()


@pytest.fixture
def ledger_root(tmp_path, monkeypatch) -> Path:
    """A repo root whose carve-out ledger is a temp file, for core and CLI alike."""
    (tmp_path / ".fno").mkdir()
    import fno.carveout.core as core

    # The CLI imports this inside each command, so patching the core module is
    # the only patch needed; there is no module-level name on the cli module.
    monkeypatch.setattr(core, "resolve_carveout_root", lambda: tmp_path)
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda *a, **k: tmp_path)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return tmp_path


def _seed(root: Path, **over) -> str:
    kwargs = {"kind": "deferred", "description": "worded badly"}
    kwargs.update(over)
    cv, _ = add_carveout(root, storage_root=root, **kwargs)
    return cv.id


def _rows(root: Path) -> list[dict]:
    return read_carveouts(root)


# -- the identity guarantee --


def test_the_id_survives_an_edit(ledger_root):
    cid = _seed(ledger_root)
    rec = update_carveout(ledger_root, cid, description="worded correctly")
    assert rec["id"] == cid
    assert rec["description"] == "worded correctly"


def test_the_timestamp_and_session_survive_an_edit(ledger_root):
    """Identity is more than the id: a re-added row would also lose its `when`."""
    cid = _seed(ledger_root)
    before = _rows(ledger_root)[0]
    update_carveout(ledger_root, cid, description="new text")
    after = _rows(ledger_root)[0]
    assert after["ts"] == before["ts"]
    assert after["session_id"] == before["session_id"]


def test_one_edit_leaves_exactly_one_row(ledger_root):
    """The lossy window closed: one locked rewrite, not a remove plus an append."""
    cid = _seed(ledger_root)
    update_carveout(ledger_root, cid, description="new text")
    rows = [r for r in _rows(ledger_root) if r["id"] == cid]
    assert len(rows) == 1


def test_omitted_fields_are_untouched(ledger_root):
    cid = _seed(ledger_root, need="an open question", priority="p1")
    update_carveout(ledger_root, cid, description="new text")
    row = _rows(ledger_root)[0]
    assert row["need"] == "an open question"
    assert row["priority"] == "p1"


def test_only_the_named_row_changes(ledger_root):
    first = _seed(ledger_root, description="leave me alone")
    second = _seed(ledger_root, description="fix me")
    update_carveout(ledger_root, second, description="fixed")
    by_id = {r["id"]: r for r in _rows(ledger_root)}
    assert by_id[first]["description"] == "leave me alone"
    assert by_id[second]["description"] == "fixed"


def test_a_long_description_is_retruncated(ledger_root):
    cid = _seed(ledger_root)
    rec = update_carveout(ledger_root, cid, description="x" * 9000, cap=100)
    assert rec["truncated"] is True
    assert "truncated" in rec["description"]


def test_truncation_clears_when_the_new_text_is_short(ledger_root):
    cid = _seed(ledger_root, description="y" * 9000)
    assert _rows(ledger_root)[0]["truncated"] is True
    update_carveout(ledger_root, cid, description="short now")
    assert _rows(ledger_root)[0]["truncated"] is False


# -- refusals --


def test_an_absent_id_is_refused_not_created(ledger_root):
    """Create-on-miss would resurrect a row /pr merged already consumed."""
    _seed(ledger_root)
    with pytest.raises(CarveoutNotFound):
        update_carveout(ledger_root, "cv-deadbeef", description="new")
    assert len(_rows(ledger_root)) == 1


def test_an_absent_ledger_is_refused(ledger_root):
    with pytest.raises(CarveoutNotFound):
        update_carveout(ledger_root, "cv-deadbeef", description="new")


def test_an_invalid_kind_is_refused_before_any_write(ledger_root):
    cid = _seed(ledger_root)
    with pytest.raises(CarveoutError):
        update_carveout(ledger_root, cid, kind="not-a-kind")
    assert _rows(ledger_root)[0]["kind"] == "deferred"


def test_a_malformed_neighbour_line_is_preserved(ledger_root):
    """One bad row must not cost the others, matching consume_carveouts."""
    cid = _seed(ledger_root)
    path = ledger_root / ".fno" / "carveouts.jsonl"
    path.write_text(path.read_text() + "{not json at all\n")
    update_carveout(ledger_root, cid, description="new text")
    assert "{not json at all" in path.read_text()


def test_an_unwritable_ledger_raises_rather_than_reporting_a_clean_noop(
    ledger_root, monkeypatch
):
    """The contract that differs from consume_carveouts.

    That function returns 0 for both "already gone" and "could not write",
    because both leave its caller's invariant intact. Here they differ: telling
    an operator the correction landed while the old wording is still on disk is
    the failure this whole node is about.
    """
    cid = _seed(ledger_root)

    def _boom(self, *a, **k):
        raise OSError("read-only file system")

    monkeypatch.setattr(Path, "write_text", _boom)
    with pytest.raises(CarveoutError):
        update_carveout(ledger_root, cid, description="new text")


# -- the CLI surface --


def test_the_cli_prints_the_preserved_id(ledger_root):
    cid = _seed(ledger_root)
    res = runner.invoke(app, ["carveout", "update", cid, "-d", "corrected"])
    assert res.exit_code == 0, res.output
    assert cid in res.output


def test_no_field_given_is_a_usage_error(ledger_root):
    """A no-op that prints a success line is the lie this verb exists to stop."""
    cid = _seed(ledger_root)
    res = runner.invoke(app, ["carveout", "update", cid])
    assert res.exit_code == 2
    assert "nothing to update" in res.output


def test_a_blank_description_is_a_usage_error(ledger_root):
    cid = _seed(ledger_root)
    res = runner.invoke(app, ["carveout", "update", cid, "-d", "   "])
    assert res.exit_code == 2
    assert _rows(ledger_root)[0]["description"] == "worded badly"


def test_an_absent_id_exits_one_from_the_cli(ledger_root):
    _seed(ledger_root)
    res = runner.invoke(app, ["carveout", "update", "cv-deadbeef", "-d", "x"])
    assert res.exit_code == 1
    assert "not on the ledger" in res.output


def test_an_invalid_priority_is_a_usage_error(ledger_root):
    cid = _seed(ledger_root)
    res = runner.invoke(app, ["carveout", "update", cid, "-p", "p9"])
    assert res.exit_code == 2


def test_crossing_the_backfill_boundary_warns_and_still_applies(ledger_root):
    """A warning, not a refusal: the row changes consumer, but the edit is legitimate."""
    cid = _seed(ledger_root, kind="deferred")
    res = runner.invoke(app, ["carveout", "update", cid, "-k", "backfill"])
    assert res.exit_code == 0, res.output
    assert "/fno:pr merged" in res.output
    assert _rows(ledger_root)[0]["kind"] == "backfill"


def test_a_kind_change_within_the_harvest_does_not_warn(ledger_root):
    """Positive control for the test above: the warning is about the boundary,
    not about any kind change at all."""
    cid = _seed(ledger_root, kind="deferred")
    res = runner.invoke(app, ["carveout", "update", cid, "-k", "oos-bug"])
    assert res.exit_code == 0, res.output
    assert "instead" not in res.output
