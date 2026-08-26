"""AC5-HP (x-baef): the one-shot model_tier -> difficulty graph migration.

Dry-run by default, --apply to write, idempotent on the second run, and loud
on a row carrying both spellings (measured impossible at migration time, so
one appearing means a writer re-added the retired key and the verb refuses
rather than guessing which band wins).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """A fresh empty graph.json; same patch surface as the integration fixture."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _seed(g: Path, n: int) -> list[str]:
    entries = [
        {"id": f"x-{i:04x}", "title": f"legacy {i}", "model_tier": "high" if i % 2 else "medium"}
        for i in range(n)
    ]
    g.write_text(json.dumps({"entries": entries}))
    return [e["id"] for e in entries]


def test_migrate_difficulty_dry_run_apply_then_empty(tmp_graph):
    ids = _seed(tmp_graph, 21)

    r = runner.invoke(app, ["backlog", "migrate-difficulty"])
    assert r.exit_code == 0, r.output
    receipt = json.loads(r.output)
    assert sorted(receipt["candidates"]) == sorted(ids)
    assert receipt["candidate_count"] == 21
    assert receipt["apply"] is False
    for e in json.loads(tmp_graph.read_text())["entries"]:
        assert "difficulty" not in e, "dry-run must not write"

    r2 = runner.invoke(app, ["backlog", "migrate-difficulty", "--apply"])
    assert r2.exit_code == 0, r2.output
    entries = {e["id"]: e for e in json.loads(tmp_graph.read_text())["entries"]}
    for i, nid in enumerate(ids):
        band = "high" if i % 2 else "medium"
        row = entries[nid]
        assert row["difficulty"] == band
        assert "model_tier" not in row
        hist = row["difficulty_history"]
        assert hist[-1]["value"] == band
        assert hist[-1]["source"] == "migration"
        assert hist[-1].get("ts")

    r3 = runner.invoke(app, ["backlog", "migrate-difficulty"])
    assert r3.exit_code == 0, r3.output
    receipt3 = json.loads(r3.output)
    assert receipt3["candidate_count"] == 0
    assert receipt3["candidates"] == []


def test_migrate_difficulty_refuses_rows_carrying_both_spellings(tmp_graph):
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "x-0001", "model_tier": "high", "difficulty": "low"},
    ]}))
    for argv in (
        ["backlog", "migrate-difficulty"],
        ["backlog", "migrate-difficulty", "--apply"],
    ):
        r = runner.invoke(app, argv)
        assert r.exit_code == 2, r.output
        assert "x-0001" in r.output
    row = json.loads(tmp_graph.read_text())["entries"][0]
    assert row["model_tier"] == "high" and row["difficulty"] == "low"


def test_migrate_difficulty_survives_null_history(tmp_graph):
    """A hand-edited row can carry `difficulty_history: null`; setdefault
    returns that null and .append dies with AttributeError. `or []` instead."""
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "x-0002", "model_tier": "high", "difficulty_history": None},
    ]}))
    r = runner.invoke(app, ["backlog", "migrate-difficulty", "--apply"])
    assert r.exit_code == 0, r.output
    row = json.loads(tmp_graph.read_text())["entries"][0]
    assert row["difficulty"] == "high"
    assert "model_tier" not in row
    assert [h["source"] for h in row["difficulty_history"]] == ["migration"]


def test_migrate_difficulty_normalizes_and_refuses_bad_bands(tmp_graph):
    """The migration is a difficulty writer, so it runs the same
    normalize_difficulty every other writer runs: case variants fold, and a
    value that is not a band is refused by id instead of migrated verbatim
    under a success receipt."""
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "x-0003", "model_tier": "HIGH"},
        {"id": "x-0004", "model_tier": "turbo"},
    ]}))
    r = runner.invoke(app, ["backlog", "migrate-difficulty", "--apply"])
    assert r.exit_code == 2, r.output
    assert "x-0004 model_tier='turbo'" in r.output
    rows = {e["id"]: e for e in json.loads(tmp_graph.read_text())["entries"]}
    # refused before any write: the good row is untouched too
    assert rows["x-0003"]["model_tier"] == "HIGH"
    assert "difficulty" not in rows["x-0003"]
    assert "difficulty" not in rows["x-0004"]

    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "x-0003", "model_tier": "HIGH"},
    ]}))
    r2 = runner.invoke(app, ["backlog", "migrate-difficulty", "--apply"])
    assert r2.exit_code == 0, r2.output
    row = json.loads(tmp_graph.read_text())["entries"][0]
    assert row["difficulty"] == "high"
    assert row["difficulty_history"][-1]["value"] == "high"


def test_migrate_difficulty_refuses_non_string_band(tmp_graph):
    """x-baef round-3: a non-string model_tier (hand-edit) gets the designed
    by-id exit-2 refusal, not an AttributeError traceback from strip()."""
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "x-0005", "model_tier": 3},
    ]}))
    r = runner.invoke(app, ["backlog", "migrate-difficulty", "--apply"])
    assert r.exit_code == 2, r.output
    assert "x-0005 model_tier=3" in r.output
    assert json.loads(tmp_graph.read_text())["entries"][0].get("difficulty") is None


def test_migrate_difficulty_drains_machine_leftovers(tmp_graph):
    """x-baef round-4: the retired --model-tier handler wrote BOTH keys with
    equal bands, so same-band pairs are what machine-created leftovers look
    like and the migration drains them; a garbage retired key under a live
    difficulty drops without a band judgment. Only a divergent pair refuses."""
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "x-0006", "model_tier": "high", "difficulty": "high"},
        {"id": "x-0007", "model_tier": "high", "difficulty": "low"},
        {"id": "x-0008", "model_tier": 3, "difficulty": "low"},
        {"id": "x-0009", "model_tier": "high"},
    ]}))
    r = runner.invoke(app, ["backlog", "migrate-difficulty", "--apply"])
    assert r.exit_code == 2, r.output
    assert "x-0007" in r.output and "hand-picked band" in r.output
    rows = {e["id"]: e for e in json.loads(tmp_graph.read_text())["entries"]}
    assert all("model_tier" in e for e in rows.values()), "divergent row refuses the batch"

    _make = [{"id": "x-0006", "model_tier": "high", "difficulty": "high"},
             {"id": "x-0008", "model_tier": 3, "difficulty": "low"},
             {"id": "x-0009", "model_tier": "high"}]
    tmp_graph.write_text(json.dumps({"entries": _make}))
    r2 = runner.invoke(app, ["backlog", "migrate-difficulty", "--apply"])
    assert r2.exit_code == 0, r2.output
    rows = {e["id"]: e for e in json.loads(tmp_graph.read_text())["entries"]}
    assert all("model_tier" not in e for e in rows.values())
    assert rows["x-0006"]["difficulty"] == "high"  # same-band pair drains
    assert [h["source"] for h in rows["x-0006"]["difficulty_history"]] == ["migration"]
    assert rows["x-0008"]["difficulty"] == "low"  # garbage key dropped, band kept
    assert rows["x-0009"]["difficulty"] == "high"  # band-less row gains its band
