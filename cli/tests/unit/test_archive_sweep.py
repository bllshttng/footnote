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
    # Seam readers (guarded metadata/display reads) resolve paths.graph_json
    # at call time; pin the resolver to the same hermetic file.
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
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


def test_roadmap_restricted_held_counts_only_that_roadmap(tmp_path, monkeypatch):
    """A --roadmap-id run's receipt describes what THAT run considered; other
    roadmaps' too-recent nodes are not this run's holds."""
    g, _archive = _route(tmp_path, monkeypatch)
    _seed(g, [
        {"id": "ab-old0001", "roadmap_id": "rm-A", "completed_at": "2020-01-01T00:00:00Z"},
        {"id": "ab-new0001", "roadmap_id": "rm-B", "completed_at": "2026-08-01T00:00:00Z"},
    ])
    r = runner.invoke(
        app, ["backlog", "archive", "--apply", "--older-than-days", "30", "--roadmap-id", "rm-A"]
    )
    assert r.exit_code == 0, r.output
    assert "held back (too-recent): 0" in r.output  # rm-B's node is not this run's hold


def test_receipt_passes_through_an_unknown_skip_reason():
    from fno.graph.cli import _archive_bucket_counts, _receipt_reason_order

    held = _archive_bucket_counts([{"_skip": "some-future-reason"}])
    assert held["some-future-reason"] == 1
    assert held["too-recent"] == 0  # zero-fill holds for the known four
    assert "some-future-reason" in _receipt_reason_order(held)


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
    assert evs[0]["data"]["mode"] == "apply"


def test_dry_run_also_emits_the_swept_event(tmp_path, monkeypatch):
    # AC8: EVERY run emits, dry-run included - the daily groom rehearsal is
    # the leg most likely to break quietly, and a silent dry-run is
    # indistinguishable from one that never ran.
    g, _archive = _route(tmp_path, monkeypatch)
    events_path = _with_events(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-done0001", "completed_at": _old(40)}])
    r = runner.invoke(app, ["backlog", "archive"])
    assert r.exit_code == 0, r.output
    evs = _events_of_type(events_path, "graph_archive_swept")
    assert len(evs) == 1
    assert evs[0]["data"]["moved"] == 1
    assert evs[0]["data"]["mode"] == "dry-run"


# -- soft-edge release (x-e520: the archive drains soft-held terminal nodes) --


def test_soft_edges_no_longer_hold_a_terminal_node():
    # THE MEASURED CASE: 203 terminal nodes pinned ONLY by an open node's
    # related peer or source_node_id. Hard edges still guard (next test); soft
    # ones release instead.
    soft_related = {"id": "x-softrel", "completed_at": _old(40)}
    soft_origin = {"id": "x-softorg", "completed_at": _old(40)}
    open_peer = {"id": "x-open", "plan_path": "p.md", "related": ["x-softrel"],
                 "source_node_id": "x-softorg"}
    to_a, rem, skip = partition_for_archive([soft_related, soft_origin, open_peer], 30, NOW)
    assert {e["id"] for e in to_a} == {"x-softrel", "x-softorg"}
    assert [e["id"] for e in rem] == ["x-open"]
    assert skip == []


def test_hard_edges_still_hold_their_targets():
    # No superseded_by case: a node carrying it is TERMINAL by module
    # definition, so an "open node's superseded_by reference" cannot exist.
    for edge in ("blocked_by", "parent", "supersedes"):
        held = {"id": "x-hard", "completed_at": _old(40)}
        if edge == "parent":
            open_node = {"id": "x-open", "plan_path": "p.md", "parent": "x-hard"}
        elif edge == "supersedes":
            open_node = {"id": "x-open", "plan_path": "p.md", "supersedes": ["x-hard"]}
        else:
            open_node = {"id": "x-open", "plan_path": "p.md", "blocked_by": ["x-hard"]}
        to_a, rem, skip = partition_for_archive([held, open_node], 30, NOW)
        assert to_a == [], f"{edge} must still guard its target"
        assert [s["_skip"] for s in skip] == ["referenced-by-open-node"]


def test_terminal_related_pair_still_moves_together():
    # The pair rule survives, scoped to terminal pairs: two terminal peers of
    # different ages must not split (the younger staying behind would name an
    # id the working graph no longer has). An OPEN peer no longer holds the
    # pair (its edge is soft and gets stripped).
    older = {"id": "x-pair-old", "completed_at": _old(60), "related": ["x-pair-new"]}
    newer = {"id": "x-pair-new", "completed_at": _old(5), "related": ["x-pair-old"]}
    to_a, rem, skip = partition_for_archive([older, newer], 30, NOW)
    assert to_a == []
    assert {e["id"] for e in rem} == {"x-pair-old", "x-pair-new"}
    assert {s["_skip"] for s in skip} == {"too-recent", "related-peer-not-archived"}


def test_release_soft_edges_strips_only_the_archived_refs():
    from fno.graph.archive import release_soft_edges

    remaining = [
        {"id": "x-open", "plan_path": "p.md", "related": ["x-gone", "x-stays"],
         "source_node_id": "x-gone"},
        {"id": "x-other", "plan_path": "q.md", "related": ["x-stays"],
         "source_node_id": "x-keeps"},
        # A hard edge survives untouched even if it somehow names an archived
        # id (it cannot arise from the partition, but the strip must never
        # touch hard semantics regardless).
        {"id": "x-blocked", "plan_path": "r.md", "blocked_by": ["x-gone"]},
    ]
    patched, stripped = release_soft_edges(remaining, {"x-gone"})
    assert stripped == 2  # one related entry + one source_node_id
    by_id = {e["id"]: e for e in patched}
    assert by_id["x-open"]["related"] == ["x-stays"]
    assert by_id["x-open"]["source_node_id"] is None
    assert by_id["x-other"]["related"] == ["x-stays"]
    assert by_id["x-other"]["source_node_id"] == "x-keeps"
    assert by_id["x-blocked"]["blocked_by"] == ["x-gone"]
    # Pure: the input dicts are not mutated.
    assert remaining[0]["related"] == ["x-gone", "x-stays"]
    assert release_soft_edges(remaining, set())[0] is remaining


def test_apply_leaves_no_open_reference_to_an_archived_id(tmp_path, monkeypatch):
    # AC2 end-to-end: the soft-held node moves, the open side is stripped, the
    # working graph holds no reference to the archived id through any edge
    # type, and `backlog get` still resolves the archived node read-through.
    real_now = datetime.now(timezone.utc)

    def _real_old(days: int) -> str:
        return (real_now - timedelta(days=days)).isoformat()

    g, archive = _route(tmp_path, monkeypatch)
    events_path = _with_events(tmp_path, monkeypatch)
    _seed(g, [
        {"id": "x-softrel", "title": "soft held", "completed_at": _real_old(40),
         "related": ["x-open"]},
        {"id": "x-open", "title": "open peer", "plan_path": "p.md",
         "related": ["x-softrel"], "source_node_id": "x-softrel"},
    ])
    r = runner.invoke(app, ["backlog", "archive", "--apply", "--older-than-days", "30"])
    assert r.exit_code == 0, r.output
    assert "soft edges stripped from open nodes: 2" in r.output
    working = json.loads(g.read_text())["entries"]
    assert [e["id"] for e in working] == ["x-open"]
    assert working[0]["related"] == []
    assert working[0]["source_node_id"] is None
    # The archived side kept its own related entry (its copy leaves with it).
    archived = json.loads(archive.read_text())["entries"]
    assert [e["id"] for e in archived] == ["x-softrel"]
    assert archived[0].get("related") == ["x-open"]
    # The event carries the strip count.
    evs = _events_of_type(events_path, "graph_archive_swept")
    assert evs[0]["data"]["moved"] == 1
    assert evs[0]["data"]["soft_edges_stripped"] == 2
    # Read-through still resolves the archived node by id.
    r_get = runner.invoke(app, ["backlog", "get", "x-softrel"])
    assert r_get.exit_code == 0, r_get.output


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
    evs = _events_of_type(events_path, "graph_archive_ids_reminted")
    assert len(evs) == 1
    assert evs[0]["data"]["remint_count"] == 1
    assert evs[0]["data"]["remap"]["ab-dup00001"] == entries[0]["id"]
    # A repair is not a sweep: it must never be recorded as one.
    assert _events_of_type(events_path, "graph_archive_swept") == []


def test_dedupe_ids_apply_with_no_collision_writes_nothing(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [{"id": "ab-live0001", "plan_path": "p.md"}])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-arch0001", "completed_at": "2026-01-01T00:00:00Z"}
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "archive-dedupe-ids", "--apply"])
    assert r.exit_code == 0, r.output
    assert "No colliding archive ids found." in r.output


# -- fno backlog album (x-a023 browse surface) -------------------------------


def test_album_empty_archive_says_so(tmp_path, monkeypatch):
    g, _archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    assert "The album is empty." in r.output


def test_album_sorts_newest_first_and_shows_the_gift(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-old00001", "title": "Old One", "status": "done",
         "completed_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-new00001", "title": "New One", "status": "done",
         "completed_at": "2026-06-01T00:00:00Z", "pr_number": 1,
         "pr_url": "https://github.com/x/y/pull/1"},
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    lines = [line for line in r.output.splitlines() if line.strip()]
    assert lines[0].startswith("album: 2 shipped")
    assert lines[1].startswith("2026-06-01")
    assert "PR #1" in lines[1]
    assert lines[2].startswith("2026-01-01")
    assert "no gift" in lines[2]


def test_album_excludes_superseded(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-done0001", "title": "Shipped", "status": "done",
         "completed_at": "2026-06-01T00:00:00Z"},
        {"id": "ab-super001", "title": "Eclipsed", "status": "superseded",
         "superseded_by": "ab-done0001", "completed_at": "2026-06-02T00:00:00Z"},
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    assert "ab-done0001" in r.output
    assert "ab-super001" not in r.output


def test_album_project_filter(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    archive.write_text(json.dumps({"entries": [
        {"id": "ab-p1", "title": "P1", "status": "done",
         "completed_at": "2026-01-01T00:00:00Z", "project": "alpha"},
        {"id": "ab-p2", "title": "P2", "status": "done",
         "completed_at": "2026-01-02T00:00:00Z", "project": "beta"},
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "album", "--project", "alpha"])
    assert r.exit_code == 0, r.output
    assert "ab-p1" in r.output
    assert "ab-p2" not in r.output


def test_album_json_output_and_limit(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    archive.write_text(json.dumps({"entries": [
        {"id": f"ab-{i:04d}", "status": "done", "title": f"n{i}",
         "completed_at": f"2026-01-{i:02d}T00:00:00Z"}
        for i in range(1, 6)
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "album", "--limit", "2", "--json"])
    assert r.exit_code == 0, r.output
    hits = json.loads(r.output)
    assert len(hits) == 2
    assert hits[0]["id"] == "ab-0005"  # newest first
    # Cards, not full entries: the gift appears only when recorded.
    assert hits[0] == {"id": "ab-0005", "title": "n5", "completed_at": "2026-01-05T00:00:00Z"}


def test_album_reports_overflow_count(tmp_path, monkeypatch):
    g, archive = _route(tmp_path, monkeypatch)
    _seed(g, [])
    archive.write_text(json.dumps({"entries": [
        {"id": f"ab-{i:04d}", "status": "done",
         "completed_at": f"2026-01-{i:02d}T00:00:00Z"}
        for i in range(1, 6)
    ]}) + "\n")
    r = runner.invoke(app, ["backlog", "album", "--limit", "2"])
    assert r.exit_code == 0, r.output
    assert "3 more" in r.output


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
