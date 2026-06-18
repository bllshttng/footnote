"""Unit tests for graph entry canonicalization: status-forward key order +
derived ``children`` summary index.

Canonicalization runs inside ``locked_mutate_graph`` after ``recompute_statuses``
so every write produces a consistent, status-near-top key order and a fresh
inverse-of-``parent`` ``children`` index. The index entries are compact
summaries ``{id, title, project, _status}`` -- enough to scan a node's children
without a second lookup, light enough not to denormalize the flat store.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.graph.store import (
    CANONICAL_FIELD_ORDER,
    canonicalize_entries,
    locked_mutate_graph,
    _read_json,
)


def _make_graph(tmp_path: Path, entries: list[dict]) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}) + "\n")
    return p


def _noop(entries):
    return entries


# -- key ordering --


def test_status_is_second_key_after_id(tmp_path):
    """After a mutation, every written entry has ``_status`` as its 2nd key."""
    path = _make_graph(tmp_path, [{"id": "ab-aaaa0001", "title": "T"}])
    locked_mutate_graph(path, _noop)
    raw = _read_json(path)  # raw read preserves on-disk key order
    keys = list(raw[0].keys())
    assert keys[0] == "id"
    assert keys[1] == "_status"


def test_canonical_order_is_applied(tmp_path):
    """The written key order matches CANONICAL_FIELD_ORDER for known keys."""
    path = _make_graph(tmp_path, [{"id": "ab-aaaa0002", "title": "T"}])
    locked_mutate_graph(path, _noop)
    raw = _read_json(path)
    keys = [k for k in raw[0].keys() if k in CANONICAL_FIELD_ORDER]
    expected = [k for k in CANONICAL_FIELD_ORDER if k in raw[0]]
    assert keys == expected


def test_extra_keys_preserved_at_end(tmp_path):
    """Legacy/unknown keys (e.g. ``points``) survive canonicalization, appended
    after the canonical keys -- never dropped."""
    path = _make_graph(
        tmp_path, [{"id": "ab-aaaa0003", "title": "T", "points": 5}]
    )
    locked_mutate_graph(path, _noop)
    raw = _read_json(path)
    assert raw[0]["points"] == 5
    # appended after the canonical block
    keys = list(raw[0].keys())
    assert keys.index("points") > keys.index("_status")


# -- children index --


def test_children_summary_for_parent(tmp_path):
    """A parent node's ``children`` lists compact summaries of its direct
    children: {id, title, project, _status}."""
    path = _make_graph(
        tmp_path,
        [
            {"id": "ab-parent01", "title": "Epic", "project": "fno"},
            {
                "id": "ab-child001",
                "title": "Child A",
                "project": "fno",
                "parent": "ab-parent01",
                "plan_path": "plans/a.md",  # -> ready
            },
        ],
    )
    locked_mutate_graph(path, _noop)
    raw = {e["id"]: e for e in _read_json(path)}
    kids = raw["ab-parent01"]["children"]
    assert kids == [
        {
            "id": "ab-child001",
            "title": "Child A",
            "project": "fno",
            "_status": "ready",
        }
    ]


def test_leaf_node_has_empty_children(tmp_path):
    path = _make_graph(tmp_path, [{"id": "ab-leaf0001", "title": "Leaf"}])
    locked_mutate_graph(path, _noop)
    raw = _read_json(path)
    assert raw[0]["children"] == []


def test_children_sorted_by_id(tmp_path):
    path = _make_graph(
        tmp_path,
        [
            {"id": "ab-parent02", "title": "Epic"},
            {"id": "ab-zzzz0001", "title": "Z", "parent": "ab-parent02"},
            {"id": "ab-aaaa0009", "title": "A", "parent": "ab-parent02"},
        ],
    )
    locked_mutate_graph(path, _noop)
    raw = {e["id"]: e for e in _read_json(path)}
    ids = [c["id"] for c in raw["ab-parent02"]["children"]]
    assert ids == ["ab-aaaa0009", "ab-zzzz0001"]


def test_children_index_is_drift_free(tmp_path):
    """Changing a child's title and re-mutating refreshes the parent summary."""
    path = _make_graph(
        tmp_path,
        [
            {"id": "ab-parent03", "title": "Epic"},
            {"id": "ab-child003", "title": "Old", "parent": "ab-parent03"},
        ],
    )
    locked_mutate_graph(path, _noop)

    def rename(entries):
        for e in entries:
            if e["id"] == "ab-child003":
                e["title"] = "New"
        return entries

    locked_mutate_graph(path, rename)
    raw = {e["id"]: e for e in _read_json(path)}
    assert raw["ab-parent03"]["children"][0]["title"] == "New"


def test_children_ignores_dangling_parent(tmp_path):
    """A parent pointer to a non-existent node does not create a phantom entry."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-orphan01", "title": "Orphan", "parent": "ab-nonexist"}],
    )
    locked_mutate_graph(path, _noop)
    raw = _read_json(path)
    # The orphan still serializes; just no parent summary is fabricated.
    assert raw[0]["children"] == []


def test_self_parent_is_not_its_own_child(tmp_path):
    """A self-parented node (corrupt row) must not become its own child."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-selfpar1", "title": "Self", "parent": "ab-selfpar1"}],
    )
    locked_mutate_graph(path, _noop)
    raw = _read_json(path)
    assert raw[0]["children"] == []


# -- rank field (ab-95a4a479: curated ranking) --


def test_rank_in_canonical_field_order():
    """``rank`` is a canonical key so canonicalize keeps it (not appended as an
    unknown extra) -- without this entry the field would be dropped/reordered."""
    assert "rank" in CANONICAL_FIELD_ORDER


def test_rank_backfilled_null_on_next_mutation(tmp_path):
    """A node with no ``rank`` key gets ``rank: null`` written on the next
    mutation -- self-healing backfill, like the status-forward migration."""
    path = _make_graph(tmp_path, [{"id": "ab-rank0001", "title": "T"}])
    locked_mutate_graph(path, _noop)
    raw = _read_json(path)
    assert "rank" in raw[0]
    assert raw[0]["rank"] is None


def test_rank_value_persists_across_column_change(tmp_path):
    """AC1-FR: a ranked node carries its ``rank`` through a column change
    (priority edit) -- rank re-homes with the node, it is not reset."""
    path = _make_graph(
        tmp_path,
        [{"id": "ab-rank0002", "title": "T", "priority": "p2", "rank": 3.5}],
    )

    def bump_priority(entries):
        for e in entries:
            if e["id"] == "ab-rank0002":
                e["priority"] = "p1"  # Next -> Now, a column change
        return entries

    locked_mutate_graph(path, bump_priority)
    raw = _read_json(path)
    assert raw[0]["priority"] == "p1"
    assert raw[0]["rank"] == 3.5


# -- canonicalize_entries unit (no I/O) --


def test_canonicalize_entries_pure():
    entries = [
        {"id": "ab-p", "title": "P", "_status": "idea"},
        {"id": "ab-c", "title": "C", "parent": "ab-p", "_status": "idea"},
    ]
    out = canonicalize_entries(entries)
    by_id = {e["id"]: e for e in out}
    assert by_id["ab-p"]["children"] == [
        {"id": "ab-c", "title": "C", "project": None, "_status": "idea"}
    ]
    assert list(by_id["ab-c"].keys())[:2] == ["id", "_status"]
