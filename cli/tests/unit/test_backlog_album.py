"""`fno backlog album`: the memento book over graph-archive.json (x-b2bf).

The album is a read verb over the file the sweep already maintains. Cards are
done nodes sorted newest-first; a card with no gift says so rather than hiding.
"""
from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _route(tmp_path: Path, monkeypatch) -> Path:
    import fno.graph._constants as gc

    archive = tmp_path / "graph-archive.json"
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", archive)
    return archive


def _seed_archive(archive: Path, entries: list[dict]) -> None:
    archive.write_text(json.dumps({"entries": entries}) + "\n")


def test_json_sorted_desc_gift_only_when_present(tmp_path, monkeypatch):
    archive = _route(tmp_path, monkeypatch)
    _seed_archive(
        archive,
        [
            {
                "id": "x-old0001",
                "title": "first ship",
                "status": "done",
                "completed_at": "2026-06-01T00:00:00Z",
                "pr_url": "https://github.com/o/r/pull/1",
                "pr_number": 1,
            },
            {
                "id": "x-new0001",
                "title": "latest ship",
                "status": "done",
                "completed_at": "2026-08-14T00:00:00Z",
            },
            {
                "id": "x-super001",
                "title": "superseded work",
                "status": "done",
                "superseded_by": "x-new0001",
                "completed_at": "2026-08-15T00:00:00Z",
            },
        ],
    )
    r = runner.invoke(app, ["backlog", "album", "--json"])
    assert r.exit_code == 0, r.output
    cards = json.loads(r.output)
    # Newest first, superseded excluded.
    assert [c["id"] for c in cards] == ["x-new0001", "x-old0001"]
    # pr_url present only when recorded; never a placeholder.
    assert cards[0].get("pr_url") is None
    assert cards[1]["pr_url"].endswith("/pull/1")


def test_text_names_no_gift_rather_than_hiding(tmp_path, monkeypatch):
    archive = _route(tmp_path, monkeypatch)
    _seed_archive(
        archive,
        [
            {
                "id": "x-gift001",
                "title": "gifted",
                "status": "done",
                "completed_at": "2026-08-14T00:00:00Z",
                "pr_number": 883,
            },
            {
                "id": "x-bare001",
                "title": "no gift recorded",
                "status": "done",
                "completed_at": "2026-08-13T00:00:00Z",
            },
        ],
    )
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    assert "album: 2 shipped" in r.output
    assert "PR #883" in r.output
    assert "no gift" in r.output
    assert "no gift recorded" in r.output  # the card renders, gap visible


def test_paging_offset_and_limit(tmp_path, monkeypatch):
    archive = _route(tmp_path, monkeypatch)
    _seed_archive(
        archive,
        [
            {
                "id": f"x-page{i:03d}",
                "title": f"card {i}",
                "status": "done",
                "completed_at": f"2026-08-{i + 1:02d}T00:00:00Z",
            }
            for i in range(5)
        ],
    )
    r = runner.invoke(app, ["backlog", "album", "--json", "--limit", "2", "--offset", "1"])
    assert r.exit_code == 0, r.output
    cards = json.loads(r.output)
    assert [c["id"] for c in cards] == ["x-page003", "x-page002"]


def test_absent_archive_is_empty_not_an_error(tmp_path, monkeypatch):
    _route(tmp_path, monkeypatch)  # never seeded
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    assert "album is empty" in r.output
    r = runner.invoke(app, ["backlog", "album", "--json"])
    assert r.exit_code == 0, r.output
    assert json.loads(r.output) == []
