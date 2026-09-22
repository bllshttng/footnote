"""Terminal-node archive sweep + read-through, against rows in the store.

Rows live in the sqlite store beside graph.json; archive residency is the
`archived_at` stamp on the row itself. These tests seed graph.json (the
import source), ride the keeper like every command, and read outcomes back
through the same seams callers hit: wire_rows, read_archive_entries, and the
sqlite column. graph-archive.json is an advisory rebuild; nothing here
writes it.

The module needs the compiled runtime and skips whole where the smoke
harness deleted the worker binary (the parity-test convention).
"""
from __future__ import annotations

import pytest

from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)

pytestmark = requires_rust

import json  # noqa: E402
import sqlite3  # noqa: E402
from datetime import datetime, timedelta, timezone  # noqa: E402
from pathlib import Path  # noqa: E402

from typer.testing import CliRunner  # noqa: E402

from fno.cli import app  # noqa: E402
from fno.graph.api import wire_rows  # noqa: E402
from fno.graph.archive import (  # noqa: E402
    _archive_bucket_counts,
    _last_sweep_line,
    _receipt_reason_order,
    partition_for_archive,
    retire_stale_postmortems,
)
from fno.graph.store import read_archive_entries  # noqa: E402

runner = CliRunner()
NOW = datetime(2026, 7, 8, tzinfo=timezone.utc)


def _old(days: int) -> str:
    return (NOW - timedelta(days=days)).isoformat()


def _real_old(days: int) -> str:
    return (datetime.now(timezone.utc) - timedelta(days=days)).isoformat()


FULL = {"type": "feature", "status": "idea", "priority": "p2", "domain": "code",
        "created_at": "2026-09-11T00:00:00+00:00"}


def _row(node_id: str, **overrides):
    return {**FULL, "id": node_id, "slug": node_id, "title": node_id, "tags": [], **overrides}


def _seed(graph: Path, *rows) -> None:
    graph.write_text(json.dumps({"entries": list(rows)}), encoding="utf-8")


def _archived_at(graph: Path, node_id: str):
    with sqlite3.connect(graph.with_suffix(".db")) as connection:
        row = connection.execute("SELECT archived_at FROM nodes WHERE id = ?", (node_id,)).fetchone()
    return None if row is None else row[0]


@pytest.fixture
def world(tmp_path, monkeypatch):
    from fno.graph.store import _worker_binary
    graph = tmp_path / "graph.json"
    _seed(graph)  # empty import source; the store folds it on first open
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    from fno import doctor_graph
    return {"graph": graph, "tmp": tmp_path}


def _events_of_type(world: dict, type_name: str) -> list[dict]:
    from tests._event_rows import event_rows

    return [
        ev for ev in event_rows(world["tmp"] / "events.jsonl")
        if ev.get("type") == type_name
    ]


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


# -- command + read-through --------------------------------------------------


def test_get_read_through_resolves_archived_node(world):
    _seed(world["graph"], _row("ab-arch0001", title="Old", status="done",
                               completed_at="2026-01-01T00:00:00Z",
                               archived_at="2026-01-02T00:00:00Z"))
    r = runner.invoke(app, ["backlog", "get", "ab-arch0001"])
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["id"] == "ab-arch0001"
    assert out["_archived"] is True


def test_get_missing_everywhere_exits_1(world):
    r = runner.invoke(app, ["backlog", "get", "ab-nope0001"])
    assert r.exit_code == 1


def test_find_read_through_resolves_archived_node(world):
    """AC1-UI: the dedup path must still surface an archived node (stamped
    _archived), or archiving done nodes silently destroys /think + /blueprint
    recall against everything ever shipped."""
    _seed(world["graph"], _row("ab-arch0001", slug="old-archived-feature",
                               title="Old Archived Feature", domain="code", status="done",
                               completed_at="2026-01-01T00:00:00Z",
                               archived_at="2026-01-02T00:00:00Z"))
    r = runner.invoke(app, ["backlog", "find", "Archived Feature", "--json"])
    assert r.exit_code == 0, r.output
    hits = json.loads(r.output)
    assert [h["id"] for h in hits] == ["ab-arch0001"]
    assert hits[0]["_archived"] is True


def test_find_ignores_a_corrupt_advisory_file(world):
    """AC1-ERR: find reads the store; a corrupt graph-archive.json left on
    disk is inert - the miss falls through to exit-1, never a crash."""
    (world["tmp"] / "graph-archive.json").write_text("{not json at all")

    r = runner.invoke(app, ["backlog", "find", "anything", "--json"])
    assert r.exit_code == 1
    assert r.exception is None or isinstance(r.exception, SystemExit)


def test_roadmap_archive_guards_across_roadmaps(world):
    """A --roadmap-id sweep must not archive a done node still referenced by an
    OPEN node in a DIFFERENT roadmap (codex P2: guard the full graph)."""
    _seed(world["graph"],
          _row("ab-dep00001", roadmap_id="rm-A", status="done",
               completed_at="2026-01-01T00:00:00Z"),
          _row("ab-open0001", roadmap_id="rm-B", plan_path="p.md",
               blocked_by=["ab-dep00001"]))
    r = runner.invoke(
        app, ["backlog", "archive", "--apply", "--older-than-days", "0", "--roadmap-id", "rm-A"]
    )
    assert r.exit_code == 0, r.output
    assert "held back (referenced-by-open-node): 1" in r.output
    rows = {e["id"]: e for e in wire_rows(path=world["graph"], include_archived=True)}
    assert not rows["ab-dep00001"].get("archived_at")  # held: rm-B still blocks on it


def test_roadmap_restricted_held_counts_only_that_roadmap(world):
    """A --roadmap-id run's receipt describes what THAT run considered; other
    roadmaps' too-recent nodes are not this run's holds."""
    _seed(world["graph"],
          _row("ab-old0001", roadmap_id="rm-A", status="done", completed_at=_real_old(400)),
          _row("ab-new0001", roadmap_id="rm-B", status="done", completed_at=_real_old(5)))
    r = runner.invoke(
        app, ["backlog", "archive", "--apply", "--older-than-days", "30", "--roadmap-id", "rm-A"]
    )
    assert r.exit_code == 0, r.output
    assert "held back (too-recent): 0" in r.output  # rm-B's node is not this run's hold
    assert _archived_at(world["graph"], "ab-old0001")
    assert not _archived_at(world["graph"], "ab-new0001")


def test_receipt_passes_through_an_unknown_skip_reason():
    held = _archive_bucket_counts([{"_skip": "some-future-reason"}])
    assert held["some-future-reason"] == 1
    assert held["too-recent"] == 0  # zero-fill holds for the known four
    assert "some-future-reason" in _receipt_reason_order(held)


# -- receipt: bucket breakdown + archived_at (x-a023) -------------------------


def test_dry_run_prints_all_four_buckets_zero_filled(world):
    _seed(world["graph"], _row("ab-open0001", plan_path="p.md"))  # nothing terminal
    r = runner.invoke(app, ["backlog", "archive"])
    assert r.exit_code == 0, r.output
    for reason in (
        "referenced-by-open-node", "related-peer-not-archived",
        "too-recent", "no-parseable-timestamp",
    ):
        assert f"held back ({reason}): 0" in r.output


def test_apply_stamps_archived_at(world):
    _seed(world["graph"], _row("ab-done0001", status="done", completed_at=_real_old(40)))
    r = runner.invoke(app, ["backlog", "archive", "--apply", "--older-than-days", "30"])
    assert r.exit_code == 0, r.output
    assert _archived_at(world["graph"], "ab-done0001")  # stamped, not just moved


def test_apply_emits_swept_event_with_moved_and_held_counts(world):
    # cmd_archive computes `now` live (datetime.now()), unlike the pure
    # partition_for_archive tests above which inject the fixed NOW -- so the
    # age offsets here are relative to the REAL clock, not the NOW constant.
    _seed(world["graph"],
          _row("ab-done0001", status="done", completed_at=_real_old(40)),
          _row("ab-done0002", status="done", completed_at=_real_old(5)))  # too-recent, held
    r = runner.invoke(app, ["backlog", "archive", "--apply", "--older-than-days", "30"])
    assert r.exit_code == 0, r.output
    evs = _events_of_type(world, "graph_archive_swept")
    assert len(evs) == 1
    data = evs[0]["data"]
    assert data["moved"] == 1
    assert data["held_too_recent"] == 1
    assert data["held_referenced"] == 0
    assert data["older_than_days"] == 30


# -- receipt: last-sweep freshness marker -------------------------------------


def test_last_sweep_line_reports_newest_archived_at(world):
    from datetime import datetime as dt, timezone as tz

    _seed(world["graph"],
          _row("ab-1", archived_at="2026-09-09T10:00:00Z"),
          _row("ab-2", archived_at="2026-09-10T09:00:00Z"))  # newest wins
    now = dt(2026, 9, 10, 12, 0, tzinfo=tz.utc)
    assert _last_sweep_line(now) == "2026-09-10T09:00:00Z (3h ago)"


def test_last_sweep_line_says_none_on_record_for_an_empty_store(world):
    from datetime import datetime as dt, timezone as tz

    assert _last_sweep_line(dt(2026, 9, 10, 12, 0, tzinfo=tz.utc)) == "none on record"


    monkeypatch.setattr(gs, "read_archive_entries", _boom)
    assert _last_sweep_line(dt(2026, 9, 10, 12, 0, tzinfo=tz.utc)) == "unknown (archive unreadable)"


def test_dry_run_receipt_carries_last_sweep_marker(world):
    _seed(world["graph"],
          _row("ab-open0001", plan_path="p.md"),
          _row("ab-1", archived_at="2026-09-10T09:00:00Z"))
    r = runner.invoke(app, ["backlog", "archive"])
    assert r.exit_code == 0, r.output
    assert "last sweep: 2026-09-10T09:00:00Z" in r.output


# -- retirement: stale postmortem receipts ------------------------------------


def _pm_receipt(node_id: str, days_old: int, now=None, **extra) -> dict:
    from datetime import datetime as dt, timedelta, timezone as tz

    # Anchor to the caller's clock when given: a wall-clock age drifts past a
    # fixed cutoff (this suite's 2026-09-10) and the test flips for real.
    created = ((now or datetime.now(tz.utc)) - timedelta(days=days_old)).isoformat()
    return {
        "id": node_id,
        "status": "idea",
        "slug": f"postmortem-doneprgreen-{node_id}",
        "title": f"postmortem DonePRGreen: {node_id}",
        "created_at": created,
        "details": "gist\n\nSource: postmortem:x.md\n\n"
                   "<!-- retro-triage source_pr=None finding_hash=ab12cd34 -->",
        **extra,
    }


def test_retire_closes_only_stale_unclaimed_receipts():
    from datetime import datetime as dt, timezone as tz

    # Real now, not a fixed date: the fixture ages receipts relative to the
    # wall clock, so a fixed cutoff goes stale as the calendar advances.
    now = dt.now(tz.utc)
    entries = [
        _pm_receipt("ab-old00001", 40, now=now),                       # retired
        _pm_receipt("ab-young0001", 5, now=now),                       # too young, stays
        _pm_receipt("ab-claim0001", 40, now=now, locked_by="s1"),      # claimed, stays
        _pm_receipt("ab-queued001", 40, now=now, queued_at=now.isoformat()),  # ack-pending, stays
        _pm_receipt("ab-defer0001", 40, now=now, status="deferred"),   # human disposition, stays
        _pm_receipt("ab-notrail01", 40, now=now, details="no trailer here"),  # not a receipt, stays
        {"id": "ab-open0002", "status": "ready"},             # not a receipt, stays
    ]
    patched, retired = retire_stale_postmortems(entries, now)
    by_id = {e["id"]: e for e in patched}
    assert [e["id"] for e in retired] == ["ab-old00001"]
    assert by_id["ab-old00001"]["status"] == "done"
    assert by_id["ab-old00001"]["retired"] == "stale-postmortem-receipt"
    assert by_id["ab-old00001"]["completed_at"]
    assert by_id["ab-young0001"]["status"] == "idea"
    assert by_id["ab-claim0001"]["status"] == "idea"
    assert by_id["ab-queued001"]["status"] == "idea"
    assert by_id["ab-defer0001"]["status"] == "deferred"
    assert by_id["ab-notrail01"]["status"] == "idea"


def test_apply_retires_receipts_and_reports_them(world):
    created = _real_old(40)
    _seed(world["graph"],
          _row("ab-old00001", status="idea", created_at=created,
               details="<!-- retro-triage source_pr=None finding_hash=ab12cd34 -->"),
          _row("ab-live0001", status="idea", created_at=created,
               details="a real idea with no trailer"))
    r = runner.invoke(app, ["backlog", "archive", "--apply", "--older-than-days", "30"])
    assert r.exit_code == 0, r.output
    assert "Retired 1 stale postmortem receipt(s)" in r.output
    rows = {e["id"]: e for e in wire_rows(path=world["graph"], include_archived=True)}
    assert rows["ab-old00001"]["status"] == "done"
    assert rows["ab-old00001"]["retired"] == "stale-postmortem-receipt"
    assert rows["ab-live0001"]["status"] == "idea"
def test_dry_run_reports_would_retire_count(world):
    _seed(world["graph"], _row("ab-old00001", status="idea", created_at=_real_old(40),
                               details="<!-- retro-triage source_pr=None finding_hash=ab12 -->"))
    r = runner.invoke(app, ["backlog", "archive"])
    assert r.exit_code == 0, r.output
    assert "would retire 1 stale postmortem receipt(s)" in r.output
    rows = {e["id"]: e for e in wire_rows(path=world["graph"])}
    assert rows["ab-old00001"]["status"] == "idea"  # dry-run never mutates


def test_apply_with_nothing_to_move_still_emits_zero_moved_event(world):
    _seed(world["graph"], _row("ab-open0001", plan_path="p.md"))
    r = runner.invoke(app, ["backlog", "archive", "--apply"])
    assert r.exit_code == 0, r.output
    evs = _events_of_type(world, "graph_archive_swept")
    assert len(evs) == 1
    assert evs[0]["data"]["moved"] == 0
    assert evs[0]["data"]["mode"] == "apply"


def test_dry_run_also_emits_the_swept_event(world):
    # AC8: EVERY run emits, dry-run included - the daily groom rehearsal is
    # the leg most likely to break quietly, and a silent dry-run is
    # indistinguishable from one that never ran.
    _seed(world["graph"], _row("ab-done0001", status="done", completed_at=_real_old(40)))
    r = runner.invoke(app, ["backlog", "archive"])
    assert r.exit_code == 0, r.output
    evs = _events_of_type(world, "graph_archive_swept")
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


def test_apply_leaves_no_open_reference_to_an_archived_id(world):
    # AC2 end-to-end: the soft-held node moves, the open side is stripped, the
    # live rows hold no reference to the archived id through any edge type,
    # and `backlog get` still resolves the archived node read-through.
    _seed(world["graph"],
          _row("x-softrel", title="soft held", status="done", completed_at=_real_old(40),
               related=["x-open"]),
          _row("x-open", title="open peer", plan_path="p.md",
               related=["x-softrel"], source_node_id="x-softrel"))
    r = runner.invoke(app, ["backlog", "archive", "--apply", "--older-than-days", "30"])
    assert r.exit_code == 0, r.output
    assert "soft edges stripped from open nodes: 2" in r.output
    working = wire_rows(path=world["graph"])
    assert [e["id"] for e in working] == ["x-open"]
    assert working[0].get("related") == []
    assert working[0].get("source_node_id") is None
    # The archived side kept its own related entry (its copy leaves with it).
    archived = read_archive_entries(path=world["graph"])
    assert [e["id"] for e in archived] == ["x-softrel"]
    assert archived[0].get("related") == ["x-open"]
    # The event carries the strip count.
    evs = _events_of_type(world, "graph_archive_swept")
    assert evs[0]["data"]["moved"] == 1
    assert evs[0]["data"]["soft_edges_stripped"] == 2
    # Read-through still resolves the archived node by id.
    r_get = runner.invoke(app, ["backlog", "get", "x-softrel"])
    assert r_get.exit_code == 0, r_get.output


# -- fno backlog album (x-a023 browse surface) --------------------------------


def test_album_empty_archive_says_so(world):
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    assert "The album is empty." in r.output


def test_album_sorts_newest_first_and_shows_the_gift(world):
    _seed(world["graph"],
          _row("ab-old00001", title="Old One", status="done",
               completed_at="2026-01-01T00:00:00Z", archived_at="2026-01-02T00:00:00Z"),
          _row("ab-new00001", title="New One", status="done",
               completed_at="2026-06-01T00:00:00Z", archived_at="2026-06-02T00:00:00Z",
               pr_number=1, pr_url="https://github.com/x/y/pull/1"))
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    lines = [line for line in r.output.splitlines() if line.strip()]
    assert lines[0].startswith("album: 2 shipped")
    assert lines[1].startswith("2026-06-01")
    assert "PR #1" in lines[1]
    assert lines[2].startswith("2026-01-01")
    assert "no gift" in lines[2]


def test_album_excludes_superseded(world):
    _seed(world["graph"],
          _row("ab-done0001", title="Shipped", status="done",
               completed_at="2026-06-01T00:00:00Z", archived_at="2026-06-02T00:00:00Z"),
          _row("ab-super001", title="Eclipsed", status="superseded",
               superseded_by="ab-done0001", completed_at="2026-06-02T00:00:00Z",
               archived_at="2026-06-03T00:00:00Z"))
    r = runner.invoke(app, ["backlog", "album"])
    assert r.exit_code == 0, r.output
    assert "ab-done0001" in r.output
    assert "ab-super001" not in r.output


def test_album_project_filter(world):
    _seed(world["graph"],
          _row("ab-p1", title="P1", status="done", completed_at="2026-01-01T00:00:00Z",
               archived_at="2026-01-02T00:00:00Z", project="alpha"),
          _row("ab-p2", title="P2", status="done", completed_at="2026-01-02T00:00:00Z",
               archived_at="2026-01-03T00:00:00Z", project="beta"))
    r = runner.invoke(app, ["backlog", "album", "--project", "alpha"])
    assert r.exit_code == 0, r.output
    assert "ab-p1" in r.output
    assert "ab-p2" not in r.output


def test_album_json_output_and_limit(world):
    _seed(world["graph"], *[
        _row(f"ab-{i:04d}", status="done", title=f"n{i}",
             completed_at=f"2026-01-{i:02d}T00:00:00Z",
             archived_at=f"2026-01-{i:02d}T00:00:00Z")
        for i in range(1, 6)
    ])
    r = runner.invoke(app, ["backlog", "album", "--limit", "2", "--json"])
    assert r.exit_code == 0, r.output
    hits = json.loads(r.output)
    assert len(hits) == 2
    assert hits[0]["id"] == "ab-0005"  # newest first
    # Cards, not full entries: the gift appears only when recorded.
    assert hits[0] == {"id": "ab-0005", "title": "n5", "completed_at": "2026-01-05T00:00:00Z"}


def test_album_reports_overflow_count(world):
    _seed(world["graph"], *[
        _row(f"ab-{i:04d}", status="done", completed_at=f"2026-01-{i:02d}T00:00:00Z",
             archived_at=f"2026-01-{i:02d}T00:00:00Z")
        for i in range(1, 6)
    ])
    r = runner.invoke(app, ["backlog", "album", "--limit", "2"])
    assert r.exit_code == 0, r.output
    assert "3 more" in r.output
