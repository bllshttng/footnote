"""Tests for the carveout `scope` field (x-7685, US8/AC15 prerequisite).

The king orphan check (hooks/context-nudge.sh) must match a carved-out
orphaning by STRUCTURED scope, not by grepping `description` free text (a
reworded sentence would silently silence it). So the Carveout record carries an
optional `scope`, written via `fno carveout add --scope`, and the field is the
match key. Optional + last so existing records and the retro-triage harvest
parse unchanged.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.carveout.core import Carveout, add_carveout, read_carveouts
from fno.cli import app


def test_add_carveout_records_scope(tmp_path: Path):
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="workers review-orphaned",
        scope="x-d7e4",
    )
    assert cv.scope == "x-d7e4"
    rows = read_carveouts(tmp_path)
    assert rows[0]["scope"] == "x-d7e4"


def test_add_carveout_scope_defaults_none(tmp_path: Path):
    cv, _ = add_carveout(tmp_path, kind="oos-bug", description="no scope given")
    assert cv.scope is None


def test_old_record_without_scope_still_parses(tmp_path: Path):
    # A record written before this field existed has no "scope" key. It must
    # still list without error and read back with scope absent/None.
    ledger = tmp_path / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        json.dumps(
            {"id": "cv-old", "ts": "2026-01-01T00:00:00Z", "session_id": None,
             "kind": "deferred", "priority": None, "need": None,
             "description": "legacy", "truncated": False}
        ) + "\n",
        encoding="utf-8",
    )
    rows = read_carveouts(tmp_path)
    assert len(rows) == 1
    assert rows[0].get("scope") is None


def test_cli_scope_round_trips_through_list_jsonl(tmp_path: Path, monkeypatch):
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    runner = CliRunner()
    add = runner.invoke(
        app,
        ["carveout", "add", "--kind", "deferred", "--scope", "zzz-probe",
         "--need", "review-orphaned: no king to trigger review",
         "workers exit review-orphaned; advisory self-review"],
    )
    assert add.exit_code == 0, add.output + add.stderr
    listing = runner.invoke(app, ["carveout", "list", "--json"])
    assert listing.exit_code == 0, listing.output
    rows = [json.loads(ln) for ln in listing.stdout.splitlines() if ln.strip()]
    matched = [r for r in rows if r.get("scope") == "zzz-probe"]
    assert len(matched) == 1
    assert "review-orphaned" in matched[0]["description"]


def test_scope_match_is_structural_not_description_substring(tmp_path: Path):
    # The discriminator is the scope FIELD. A carveout whose description happens
    # to mention a scope but carries a different (or no) --scope must NOT match.
    add_carveout(
        tmp_path, kind="deferred",
        description="unrelated note that mentions x-d7e4 in prose",
        scope="x-other",
    )
    rows = read_carveouts(tmp_path)
    assert not any(r.get("scope") == "x-d7e4" for r in rows)
    assert any(r.get("scope") == "x-other" for r in rows)


def test_carveout_dataclass_scope_is_optional_last_field():
    # Optional + last: existing positional construction and asdict() are stable.
    cv = Carveout(
        id="cv-x", ts="t", session_id=None, kind="deferred", priority=None,
        need=None, description="d", truncated=False,
    )
    assert cv.scope is None
