"""compute_readiness: the three-way, read-time dependency readiness result.

recompute_statuses (statuses.py) no longer derives `status` from `blocked_by`
at write time - see test_graph_statuses.py's *_at_write_time tests. This file
covers the two things that replaced it: the pure function itself, and the
read-time overlay every graph reader shares via
`fno.graph.store._apply_graph_defaults`.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.graph.statuses import compute_readiness, recompute_statuses
from fno.graph.store import locked_mutate_graph, read_graph


def _entry(eid: str, **kwargs) -> dict:
    base = {
        "id": eid,
        "title": eid,
        "completed_at": None,
        "session_id": None,
        "claimed_at": None,
        "blocked_by": [],
        "plan_path": f"plans/{eid}.md",
        "status": "ready",
    }
    base.update(kwargs)
    return base


def _write(tmp_path: Path, entries: list[dict]) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return p


# -- compute_readiness (pure function) --


def test_no_blockers_is_ready():
    e = _entry("ab-aaaaaaaa")
    assert compute_readiness(e, {}) == ("ready", None)


def test_all_blockers_completed_is_ready():
    blocker = _entry("ab-bbbbbbbb", completed_at="2026-01-01T00:00:00Z")
    e = _entry("ab-cccccccc", blocked_by=["ab-bbbbbbbb"])
    assert compute_readiness(e, {"ab-bbbbbbbb": blocker}) == ("ready", None)


def test_open_blocker_is_blocked_by_with_the_blocker_id():
    blocker = _entry("ab-dddddddd")
    e = _entry("ab-eeeeeeee", blocked_by=["ab-dddddddd"])
    assert compute_readiness(e, {"ab-dddddddd": blocker}) == ("blocked-by", "ab-dddddddd")


def test_missing_blocker_id_is_unknown_dep_never_ready():
    """The failure mode named in the dispatch brief: an id absent from the
    graph must fail closed to unknown-dep, never resolve as satisfied."""
    e = _entry("ab-ffffffff", blocked_by=["ab-does-not-exist"])
    result = compute_readiness(e, {})
    assert result == ("unknown-dep", "ab-does-not-exist")
    assert result[0] != "ready"


def test_first_open_or_unknown_blocker_wins_in_declared_order():
    blocker = _entry("ab-gggggggg")  # open
    e = _entry("ab-hhhhhhhh", blocked_by=["ab-gggggggg", "ab-unknown"])
    assert compute_readiness(e, {"ab-gggggggg": blocker}) == ("blocked-by", "ab-gggggggg")


# -- read-time overlay via read_graph / _apply_graph_defaults --


def test_read_graph_overlays_blocked_for_an_open_blocker(tmp_path: Path):
    p = _write(
        tmp_path,
        [
            _entry("ab-iiiiiiii"),
            _entry("ab-jjjjjjjj", blocked_by=["ab-iiiiiiii"]),
        ],
    )
    rows = {e["id"]: e for e in read_graph(p)}
    assert rows["ab-jjjjjjjj"]["status"] == "blocked"
    assert rows["ab-jjjjjjjj"]["blocked_reason"] == "blocked-by:ab-iiiiiiii"
    assert rows["ab-iiiiiiii"]["blocked_reason"] is None


def test_read_graph_overlays_blocked_for_an_unknown_dependency(tmp_path: Path):
    p = _write(tmp_path, [_entry("ab-kkkkkkkk", blocked_by=["ab-nonexistent"])])
    rows = {e["id"]: e for e in read_graph(p)}
    assert rows["ab-kkkkkkkk"]["status"] == "blocked"
    assert rows["ab-kkkkkkkk"]["blocked_reason"] == "unknown-dep:ab-nonexistent"
    assert rows["ab-kkkkkkkk"]["status"] != "ready"


def test_read_graph_overrides_a_stale_persisted_ready(tmp_path: Path):
    """The property this whole change exists for: a stale on-disk `ready`
    (e.g. written before this change, or by a node whose blocker regressed
    after its own last mutation) must not be trusted. The read-time overlay
    recomputes from blocked_by + the blocker's own completed_at every time."""
    p = _write(
        tmp_path,
        [
            _entry("ab-llllllll"),  # blocker: not completed
            _entry("ab-mmmmmmmm", blocked_by=["ab-llllllll"], status="ready"),
        ],
    )
    rows = {e["id"]: e for e in read_graph(p)}
    assert rows["ab-mmmmmmmm"]["status"] == "blocked"
    assert rows["ab-mmmmmmmm"]["blocked_reason"] == "blocked-by:ab-llllllll"


def test_deleting_the_persisted_flag_does_not_change_the_answer(tmp_path: Path):
    """Never a stored boolean: the same fixture read with no persisted
    `status` key at all (simulating a graph that never wrote one) produces
    the identical overlay result as one that persisted a stale value."""
    p = _write(
        tmp_path,
        [
            _entry("ab-nnnnnnnn"),
            {
                "id": "ab-oooooooo",
                "title": "ab-oooooooo",
                "blocked_by": ["ab-nnnnnnnn"],
                "plan_path": "plans/ab-oooooooo.md",
                # no "status" key at all
            },
        ],
    )
    rows = {e["id"]: e for e in read_graph(p)}
    assert rows["ab-oooooooo"]["status"] == "blocked"
    assert rows["ab-oooooooo"]["blocked_reason"] == "blocked-by:ab-nnnnnnnn"


def test_terminal_statuses_outrank_blocked(tmp_path: Path):
    """done/superseded/deferred/in_review are direct facts about the node
    itself and must not be demoted to blocked just because blocked_by names
    an open dependency.

    The overlay trusts the PERSISTED status for exactly these four buckets
    (they are direct, single-node facts that recompute_statuses still
    derives correctly at write time - only the blocked_by cross-node join
    moved to read time), so this fixture sets `status` explicitly to what
    recompute_statuses would already have written, rather than relying on
    read_graph to derive it from completed_at/superseded_by/deferred_at
    (read_graph applies defaults, it does not re-run recompute_statuses).
    """
    p = _write(
        tmp_path,
        [
            _entry("ab-open0001"),
            _entry(
                "ab-done0001",
                blocked_by=["ab-open0001"],
                completed_at="2026-01-01T00:00:00Z",
                status="done",
            ),
            _entry(
                "ab-super0001", blocked_by=["ab-open0001"], superseded_by="ab-x", status="superseded"
            ),
            _entry(
                "ab-defer0001",
                blocked_by=["ab-open0001"],
                deferred_at="2026-01-01T00:00:00Z",
                status="deferred",
            ),
            _entry("ab-review0001", blocked_by=["ab-open0001"], pr_number=42, status="in_review"),
        ],
    )
    rows = {e["id"]: e for e in read_graph(p)}
    assert rows["ab-done0001"]["status"] == "done"
    assert rows["ab-super0001"]["status"] == "superseded"
    assert rows["ab-defer0001"]["status"] == "deferred"
    assert rows["ab-review0001"]["status"] == "in_review"


def test_recompute_statuses_never_round_trips_a_stale_blocked_reason():
    """The write path (locked_mutate_graph) reads via _apply_graph_defaults
    BEFORE handing entries to the mutator, so a dict can arrive at
    recompute_statuses already carrying a `blocked_reason` the read-time
    overlay set moments earlier. recompute_statuses must scrub it rather
    than let it round-trip to disk - `blocked_reason` is answered fresh on
    every read (compute_readiness) and must never be a field a write can
    leave stale.
    """
    entry = _entry(
        "ab-ppppppp1",
        blocked_by=["ab-still-open"],
        blocked_reason="blocked-by:ab-still-open",  # simulates the read-time leak
    )
    result = recompute_statuses([entry])
    assert result[0]["blocked_reason"] is None


def test_locked_mutate_graph_overlays_blocked_on_the_entries_it_hands_to_render(
    tmp_path: Path,
):
    """recompute_statuses no longer derives `blocked`, so the entries passed
    to render_graph_md/render_graph_html right after a mutation would show a
    dependency-blocked sibling as `ready`/`idea` (its persisted status)
    unless the overlay is re-applied before rendering. Exercise the real
    write path end to end: a no-op mutation on a graph containing an
    already-blocked sibling must return (and therefore render) `blocked`,
    not the persisted `ready`.
    """
    p = _write(
        tmp_path,
        [
            _entry("ab-qqqqqqqq"),  # open blocker
            _entry("ab-rrrrrrrr", blocked_by=["ab-qqqqqqqq"]),  # sibling being touched
        ],
    )
    result = locked_mutate_graph(p, lambda entries: entries)
    rows = {e["id"]: e for e in result}
    assert rows["ab-rrrrrrrr"]["status"] == "blocked"
    assert rows["ab-rrrrrrrr"]["blocked_reason"] == "blocked-by:ab-qqqqqqqq"


def test_board_renders_derived_blocked_status_outside_in_progress(tmp_path: Path):
    from fno.graph.render import render_graph_md

    p = _write(
        tmp_path,
        [
            _entry("blocker"),
            _entry(
                "dependent",
                title="BlockedCard",
                status="ready",
                priority="p1",
                blocked_by=["blocker"],
            ),
        ],
    )

    entries = read_graph(p)
    dependent = next(entry for entry in entries if entry["id"] == "dependent")
    assert dependent["status"] == "blocked"

    output = tmp_path / "graph.md"
    render_graph_md(entries, output)
    content = output.read_text()
    active_body = content.split("## In Progress", 1)[1].split("\n## ", 1)[0]
    now_body = content.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert "BlockedCard" in now_body
    assert "BlockedCard" not in active_body


# -- children summaries derive through the same overlay --

# The parent's `children` array is the surface a king surveys an epic with.
# Its `status` must speak the derived truth (open blocker -> blocked), never
# the stored field, which by design never encodes blocked.


def test_read_graph_children_summary_derives_blocked(tmp_path: Path):
    """The exact on-disk shape a mis-dispatched epic child carries today:
    a child stored `ready` with a non-empty blocked_by over an open blocker
    must read `blocked` BOTH at top level and inside the parent's summary."""
    p = _write(
        tmp_path,
        [
            _entry("ab-epic00001"),
            _entry("ab-block0001"),
            _entry(
                "ab-child0002",
                parent="ab-epic00001",
                blocked_by=["ab-block0001"],
            ),
        ],
    )
    rows = {e["id"]: e for e in read_graph(p)}
    assert rows["ab-child0002"]["status"] == "blocked"
    summary = rows["ab-epic00001"]["children"][0]
    assert summary["id"] == "ab-child0002"
    assert summary["status"] == "blocked"


def test_write_persists_derived_children_summary(tmp_path: Path):
    """Raw graph.json readers (the Rust mux) see the summary as persisted, so
    the write path must stamp the derived status, not the cascade field."""
    from fno.graph.store import _read_json

    p = _write(
        tmp_path,
        [
            _entry("ab-epic00002"),
            _entry("ab-block0002"),
            _entry(
                "ab-child0003",
                parent="ab-epic00002",
                blocked_by=["ab-block0002"],
            ),
        ],
    )
    locked_mutate_graph(p, lambda entries: entries)
    raw = {e["id"]: e for e in _read_json(p)}
    # The child's own stored status stays cascade-derived (never `blocked`
    # on disk) while its summary in the parent reads blocked.
    assert raw["ab-child0003"]["status"] == "ready"
    assert raw["ab-epic00002"]["children"][0]["status"] == "blocked"


def test_children_summary_terminal_statuses_pass_through(tmp_path: Path):
    """done / in_review children keep their own status even with a stale
    blocked_by: terminal facts about the child outrank the overlay, and an
    epic's done count must not change under derivation."""
    p = _write(
        tmp_path,
        [
            _entry("ab-epic00003"),
            _entry("ab-block0003"),
            _entry(
                "ab-child0004",
                parent="ab-epic00003",
                completed_at="2026-08-21T00:00:00Z",
                # Stored terminal: the completed_at cascade stamps done at
                # write time, so that is the shape a live row carries on disk.
                status="done",
                blocked_by=["ab-block0003"],
            ),
            _entry(
                "ab-child0005",
                parent="ab-epic00003",
                pr_number=1234,
                # Stored terminal: the pr_number cascade stamps in_review at
                # write time, so that is the shape a live row carries on disk.
                status="in_review",
                blocked_by=["ab-block0003"],
            ),
        ],
    )
    rows = {e["id"]: e for e in read_graph(p)}
    statuses = {c["id"]: c["status"] for c in rows["ab-epic00003"]["children"]}
    assert statuses == {"ab-child0004": "done", "ab-child0005": "in_review"}


def test_readiness_status_terminal_passthrough_and_reason():
    """The shared wrapper: terminal statuses return untouched; an open
    blocker returns blocked plus its reason; ready returns the cascade
    status unchanged."""
    from fno.graph.statuses import readiness_status

    blocker = _entry("ab-block0004")
    assert readiness_status(_entry("ab-ssssssss", status="done"),
                            {"ab-block0004": blocker}) == ("done", None)
    assert readiness_status(
        _entry("ab-tttttttt", blocked_by=["ab-block0004"]),
        {"ab-block0004": blocker},
    ) == ("blocked", "blocked-by:ab-block0004")
    assert readiness_status(_entry("ab-uuuuuuuu"),
                            {"ab-block0004": blocker}) == ("ready", None)
