"""Wave 4: terminal-node archive sweep + read-through fallback.

Pure logic (partition guards, age filter, dedup merge) plus the command's
dry-run/apply behavior and `backlog get`'s read-through into the archive.
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app
from fno.graph.archive import (
    merge_into_archive,
    partition_for_archive,
    remint_archive_collisions,
    stamp_archived_at,
)

runner = CliRunner()
NOW = datetime(2026, 7, 8, tzinfo=timezone.utc)


def _old(days: int) -> str:
    return (NOW - timedelta(days=days)).isoformat()


# -- partition_for_archive -------------------------------------------------


def test_old_done_archived():
    e = {"id": "x-1", "completed_at": _old(40)}
    to_a, rem, skip = partition_for_archive([e], 30, NOW)
    assert [x["id"] for x in to_a] == ["x-1"]
    assert rem == []


def test_recent_done_held():
    e = {"id": "x-1", "completed_at": _old(5)}
    to_a, rem, skip = partition_for_archive([e], 30, NOW)
    assert to_a == []
    assert [s["_skip"] for s in skip] == ["too-recent"]


def test_open_node_never_archived():
    e = {"id": "x-1", "plan_path": "p.md"}  # no completed_at/superseded_by
    to_a, rem, skip = partition_for_archive([e], 0, NOW)
    assert to_a == []
    assert [x["id"] for x in rem] == ["x-1"]


def test_superseded_archived():
    e = {"id": "x-1", "superseded_by": "x-2", "updated": _old(40)}
    to_a, _rem, _skip = partition_for_archive([e], 30, NOW)
    assert [x["id"] for x in to_a] == ["x-1"]


def test_blocker_of_open_node_never_archived():
    done_blocker = {"id": "x-dep", "completed_at": _old(99)}
    open_node = {"id": "x-open", "plan_path": "p.md", "blocked_by": ["x-dep"]}
    to_a, rem, skip = partition_for_archive([done_blocker, open_node], 0, NOW)
    assert to_a == []  # x-dep held: an open node still waits on it
    assert {x["id"] for x in rem} == {"x-dep", "x-open"}
    assert [s["_skip"] for s in skip] == ["referenced-by-open-node"]


def test_parent_of_open_child_never_archived():
    parent = {"id": "x-epic", "completed_at": _old(99)}
    child = {"id": "x-child", "plan_path": "p.md", "parent": "x-epic"}
    to_a, _rem, skip = partition_for_archive([parent, child], 0, NOW)
    assert to_a == []
    assert skip[0]["_skip"] == "referenced-by-open-node"


def test_no_timestamp_held():
    e = {"id": "x-1", "completed_at": ""}  # terminal via nothing -> open, actually
    # A done node whose completed_at is falsy is not terminal; use superseded to
    # exercise the no-timestamp path.
    e = {"id": "x-1", "superseded_by": "x-2"}  # no updated/created_at
    to_a, rem, skip = partition_for_archive([e], 0, NOW)
    assert to_a == []
    assert skip[0]["_skip"] == "no-parseable-timestamp"


# -- merge_into_archive (crash-window dedup) -------------------------------


def test_merge_dedups_by_id_last_wins():
    existing = [{"id": "x-1", "completed_at": "a"}]
    new = [{"id": "x-1", "completed_at": "b"}, {"id": "x-2", "completed_at": "c"}]
    merged = merge_into_archive(existing, new)
    by_id = {e["id"]: e for e in merged}
    assert len(merged) == 2
    assert by_id["x-1"]["completed_at"] == "b"  # duplicate healed, last wins


# -- command + read-through ------------------------------------------------


def _route(tmp_path, monkeypatch) -> tuple[Path, Path]:
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Route paths.graph_archive_json (used by cmd_get read-through) to the temp.
    import fno.paths as p
    monkeypatch.setattr(p, "graph_json", lambda: g)
    return g, tmp_path / "graph-archive.json"


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}) + "\n")


def test_get_read_through_resolves_archived_node(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])  # working graph empty
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-arch0001", "slug": "archived-node", "title": "Old", "completed_at": "2026-01-01T00:00:00Z"}
    ]}) + "\n")

    r = runner.invoke(app, ["backlog", "get", "ab-arch0001"])
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["id"] == "ab-arch0001"
    assert out["_archived"] is True


def test_get_missing_everywhere_exits_1(tmp_path, monkeypatch):
    g, _archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    r = runner.invoke(app, ["backlog", "get", "ab-nope0001"])
    assert r.exit_code == 1


def test_find_read_through_resolves_archived_node(tmp_path, monkeypatch):
    """AC1-UI: the dedup path must still surface an archived node (stamped
    _archived), or archiving done nodes silently destroys /think + /blueprint
    recall against everything ever shipped."""
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])  # working graph empty
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-arch0001", "slug": "old-archived-feature",
         "title": "Old Archived Feature", "domain": "code",
         "completed_at": "2026-01-01T00:00:00Z"}
    ]}) + "\n")

    r = runner.invoke(app, ["backlog", "find", "Archived Feature", "--json"])
    assert r.exit_code == 0, r.output
    hits = json.loads(r.output)
    assert [h["id"] for h in hits] == ["ab-arch0001"]
    assert hits[0]["_archived"] is True


def test_find_corrupt_archive_is_miss_not_crash(tmp_path, monkeypatch):
    """AC1-ERR: a corrupt graph-archive.json is a miss (fall through to exit-1),
    never a crash propagated to the caller."""
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])  # working graph empty
    archive.write_text("{not json at all")

    r = runner.invoke(app, ["backlog", "find", "anything", "--json"])
    assert r.exit_code == 1
    assert r.exception is None or isinstance(r.exception, SystemExit)


def test_roadmap_archive_guards_across_roadmaps(tmp_path, monkeypatch):
    """A --roadmap-id sweep must not archive a done node still referenced by an
    OPEN node in a DIFFERENT roadmap (codex P2: guard the full graph)."""
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [
        {"id": "ab-dep00001", "roadmap_id": "rm-A", "completed_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-open0001", "roadmap_id": "rm-B", "plan_path": "p.md", "blocked_by": ["ab-dep00001"]},
    ])
    r = runner.invoke(
        app, ["backlog", "archive", "--apply", "--older-than-days", "0", "--roadmap-id", "rm-A"]
    )
    assert r.exit_code == 0, r.output
    live = {e["id"] for e in json.loads(g.read_text())["entries"]}
    assert "ab-dep00001" in live  # held: an open node in rm-B still blocks on it
    assert not archive.exists() or "ab-dep00001" not in {
        e["id"] for e in json.loads(archive.read_text())["entries"]
    }


# -- receipt: bucket breakdown + archived_at (x-a023) -----------------------


def _with_events(tmp_path, monkeypatch):
    import fno.paths as p

    monkeypatch.setattr(p, "state_dir", lambda: tmp_path)
    return tmp_path / "events.jsonl"


def _events_of_type(events_path: Path, type_name: str) -> list[dict]:
    if not events_path.exists():
        return []
    out = []
    for line in events_path.read_text().splitlines():
        ev = json.loads(line)
        if ev.get("type") == type_name:
            out.append(ev)
    return out


def test_dry_run_prints_all_four_buckets_zero_filled(tmp_path, monkeypatch):
    g, _archive = _route(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-open0001", "plan_path": "p.md"}])  # nothing terminal
    r = runner.invoke(app, ["backlog", "archive"])
    assert r.exit_code == 0, r.output
    for reason in (
        "referenced-by-open-node", "related-peer-not-archived",
        "too-recent", "no-parseable-timestamp",
    ):
        assert f"held back ({reason}): 0" in r.output


def test_apply_stamps_archived_at(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-done0001", "completed_at": _old(40)}])
    r = runner.invoke(app, ["backlog", "archive", "--apply", "--older-than-days", "30"])
    assert r.exit_code == 0, r.output
    entry = json.loads(archive.read_text())["entries"][0]
    assert entry["id"] == "ab-done0001"
    assert entry.get("archived_at")  # stamped, not just moved


def test_apply_emits_swept_event_with_moved_and_held_counts(tmp_path, monkeypatch):
    # cmd_archive computes `now` live (datetime.now()), unlike the pure
    # partition_for_archive tests above which inject the fixed NOW -- so the
    # age offsets here are relative to the REAL clock, not the NOW constant.
    real_now = datetime.now(timezone.utc)

    def _real_old(days: int) -> str:
        return (real_now - timedelta(days=days)).isoformat()

    g, _archive = _route(tmp_path, monkeypatch)
    events_path = _with_events(tmp_path, monkeypatch)
    _seed(g, [
        {"id": "ab-done0001", "completed_at": _real_old(40)},
        {"id": "ab-done0002", "completed_at": _real_old(5)},  # too-recent, held
    ])
    r = runner.invoke(app, ["backlog", "archive", "--apply", "--older-than-days", "30"])
    assert r.exit_code == 0, r.output
    evs = _events_of_type(events_path, "graph_archive_swept")
    assert len(evs) == 1
    data = evs[0]["data"]
    assert data["moved"] == 1
    assert data["held_too_recent"] == 1
    assert data["held_referenced"] == 0
    assert data["older_than_days"] == 30


def test_apply_with_nothing_to_move_still_emits_zero_moved_event(tmp_path, monkeypatch):
    g, _archive = _route(tmp_path, monkeypatch)
    events_path = _with_events(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-open0001", "plan_path": "p.md"}])
    r = runner.invoke(app, ["backlog", "archive", "--apply"])
    assert r.exit_code == 0, r.output
    evs = _events_of_type(events_path, "graph_archive_swept")
    assert len(evs) == 1
    assert evs[0]["data"]["moved"] == 0


# -- remint_archive_collisions (x-f69b) --------------------------------------


def test_remint_no_collision_is_noop():
    archive_entries = [{"id": "ab-arch0001", "completed_at": "2026-01-01T00:00:00Z"}]
    patched, remap = remint_archive_collisions({"ab-live0001"}, archive_entries)
    assert remap == {}
    assert patched == archive_entries


def test_remint_reissues_colliding_archive_entry_and_keeps_previous_id():
    archive_entries = [
        {"id": "ab-dup00001", "title": "old archived node", "completed_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-other001", "completed_at": "2026-01-01T00:00:00Z"},
    ]
    patched, remap = remint_archive_collisions({"ab-dup00001"}, archive_entries)
    assert list(remap.keys()) == ["ab-dup00001"]
    new_id = remap["ab-dup00001"]
    reminted = next(e for e in patched if e.get("previous_id") == "ab-dup00001")
    assert reminted["id"] == new_id
    assert reminted["title"] == "old archived node"
    # The untouched entry passes through unchanged.
    other = next(e for e in patched if e["id"] == "ab-other001")
    assert "previous_id" not in other


def test_stamp_archived_at_sets_field_without_mutating_input():
    entries = [{"id": "ab-1"}, {"id": "ab-2", "archived_at": "old"}]
    stamped = stamp_archived_at(entries, "2026-08-14T00:00:00Z")
    assert [e["archived_at"] for e in stamped] == ["2026-08-14T00:00:00Z"] * 2
    assert "archived_at" not in entries[0]  # input untouched


# -- fno backlog get resolves a reminted id via previous_id (x-f69b) --------


def test_get_resolves_reminted_archive_entry_via_previous_id(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-newid001", "previous_id": "ab-oldid001", "slug": "reminted",
         "title": "Reminted Node", "completed_at": "2026-01-01T00:00:00Z"}
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "get", "ab-oldid001"])
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["id"] == "ab-newid001"
    assert out["_archived"] is True


# -- fno backlog archive-dedupe-ids (x-f69b) ---------------------------------


def test_dedupe_ids_dry_run_reports_without_writing(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-dup00001", "plan_path": "p.md"}])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-dup00001", "completed_at": "2026-01-01T00:00:00Z"}
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "archive-dedupe-ids"])
    assert r.exit_code == 0, r.output
    assert "would remint 1 archive id(s)" in r.output
    assert "ab-dup00001" in json.loads(archive.read_text())["entries"][0]["id"]  # unwritten


def test_dedupe_ids_apply_writes_and_keeps_previous_id(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    events_path = _with_events(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-dup00001", "plan_path": "p.md"}])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-dup00001", "title": "old", "completed_at": "2026-01-01T00:00:00Z"}
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "archive-dedupe-ids", "--apply"])
    assert r.exit_code == 0, r.output
    entries = json.loads(archive.read_text())["entries"]
    assert len(entries) == 1
    assert entries[0]["previous_id"] == "ab-dup00001"
    assert entries[0]["id"] != "ab-dup00001"
    evs = _events_of_type(events_path, "graph_archive_swept")
    assert len(evs) == 1
    assert evs[0]["data"]["remint_count"] == 1


def test_dedupe_ids_apply_with_no_collision_writes_nothing(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-live0001", "plan_path": "p.md"}])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-arch0001", "completed_at": "2026-01-01T00:00:00Z"}
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "archive-dedupe-ids", "--apply"])
    assert r.exit_code == 0, r.output
    assert "No colliding archive ids found." in r.output


# -- entries_with_archive never returns duplicate ids (x-f69b VERIFY ask) ---


def test_entries_with_archive_never_returns_duplicate_ids(tmp_path, monkeypatch):
    from fno.graph.store import entries_with_archive
    import fno.paths as p

    archive_path = tmp_path / "graph-archive.json"
    archive_path.write_text(json.dumps({"entries": [
        {"id": "x-dup", "title": "archived version"}
    ]}) + "\n")
    monkeypatch.setattr(p, "graph_archive_json", lambda: archive_path)

    working = [{"id": "x-dup", "title": "live version"}]
    merged = entries_with_archive(working)
    ids = [e["id"] for e in merged]
    assert ids.count("x-dup") == 1  # working entry wins; archived duplicate dropped
    assert next(e for e in merged if e["id"] == "x-dup")["title"] == "live version"
