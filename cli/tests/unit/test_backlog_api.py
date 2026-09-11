"""The typed graph client (fno.graph.api) against a real store keeper.

Every test seeds a temp graph.json, lets the keeper import it, and drives
the typed surface end to end: Python call -> keeper `api` op ->
backlog::api function -> typed reply parsed back into
fno.graph.types models.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.rust_binary import find_dev_binary
from fno.graph import api
from fno.graph.types import (
    Comment,
    CommentCreateInput,
    Dispatch,
    Node,
    NodeClaim,
    NodeCreateInput,
    NodeFilter,
    NodeUpdateInput,
    PullRequest,
    RelationType,
    SessionRecord,
)

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)

pytestmark = requires_rust


def _row(id: str, title: str, status: str, **extra) -> dict:
    row = {
        "id": id,
        "slug": id,
        "title": title,
        "type": "feature",
        "status": status,
        "priority": "p2",
        "domain": "code",
        "created_at": "2026-09-11T00:00:00+00:00",
    }
    row.update(extra)
    return row


def _seed(tmp_path: Path, rows: list[dict]) -> Path:
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": rows}) + "\n")
    return graph


def _fixture(tmp_path: Path) -> Path:
    return _seed(
        tmp_path,
        [
            _row("ab-one", "One", "idea", project="fno", details="the one body",
                 locked_by="holder-1", tags=["infra"],
                 sessions=[{"phase": "do", "harness": "claude", "session_id": "s-1"}]),
            _row("ab-two", "Two", "ready", project="fno", parent="ab-one"),
            _row("ab-three", "Three", "ready", project="fno"),
            _row("ab-four", "Four", "done", project="other",
                 archived_at="2026-09-10T00:00:00+00:00"),
        ],
    )


def test_entry_is_node_alias():
    from fno.graph import types

    assert types.Entry is types.Node


def test_node_query_returns_typed_models(tmp_path):
    # AC16-HP: typed aggregates and description == the wire details.
    graph = _fixture(tmp_path)
    n = api.node("ab-one", path=graph)
    assert n is not None
    assert isinstance(n, Node)
    assert isinstance(n.claim, NodeClaim)
    assert n.claim.locked_by == "holder-1"
    assert isinstance(n.dispatch, Dispatch)
    assert all(isinstance(pr, PullRequest) for pr in n.pull_requests)
    assert all(isinstance(row, SessionRecord) for row in n.sessions)
    assert all(isinstance(row, Comment) for row in n.comments)
    assert n.description == "the one body"
    assert api.node("ab-absent", path=graph) is None


def test_nodes_pagination_first_then_cursor(tmp_path):
    graph = _fixture(tmp_path)
    page_one = api.nodes(NodeFilter(project="fno"), first=2, path=graph)
    assert [n.id for n in page_one.nodes] == ["ab-one", "ab-two"]
    assert page_one.page_info.has_next_page
    page_two = api.nodes(
        NodeFilter(project="fno"), first=2, after=page_one.page_info.end_cursor, path=graph
    )
    assert [n.id for n in page_two.nodes] == ["ab-three"]
    assert not page_two.page_info.has_next_page


def test_nodes_first_none_returns_all_live_rows(tmp_path):
    graph = _fixture(tmp_path)
    conn = api.nodes(path=graph)
    assert [n.id for n in conn.nodes] == ["ab-one", "ab-two", "ab-three"]
    assert all(isinstance(n, Node) for n in conn.nodes)


def test_nodes_include_archived_only_when_asked(tmp_path):
    graph = _fixture(tmp_path)
    conn = api.nodes(include_archived=True, path=graph)
    assert [n.id for n in conn.nodes] == ["ab-one", "ab-two", "ab-three", "ab-four"]


def test_nodes_filter_by_project(tmp_path):
    graph = _fixture(tmp_path)
    conn = api.nodes(NodeFilter(project="other"), include_archived=True, path=graph)
    assert [n.id for n in conn.nodes] == ["ab-four"]


def test_version_grows_one_per_mutation(tmp_path):
    graph = _seed(tmp_path, [_row("ab-one", "One", "idea", project="fno")])
    before = api.version(path=graph)
    payload = api.node_create(
        NodeCreateInput(id="ab-new", title="New", project="fno"), path=graph
    )
    assert payload.success
    assert payload.version == before + 1
    assert api.version(path=graph) == before + 1


def test_node_update_payload_and_refusal(tmp_path):
    graph = _fixture(tmp_path)
    payload = api.node_update(
        "ab-two", NodeUpdateInput(description="updated body"), path=graph
    )
    assert payload.success
    assert payload.node.description == "updated body"
    reread = api.node("ab-two", path=graph)
    assert reread.description == "updated body"
    before = api.version(path=graph)
    refused = api.node_update("ab-absent", NodeUpdateInput(title="x"), path=graph)
    assert not refused.success
    assert refused.node is None
    assert refused.version == before
    assert api.version(path=graph) == before


def test_relation_create_and_delete(tmp_path):
    graph = _fixture(tmp_path)
    payload = api.relation_create("ab-one", "ab-two", RelationType.blocks, path=graph)
    assert payload.success
    assert payload.node.claim is not None
    reread = api.node("ab-one", path=graph)
    assert reread.blocked_by == ["ab-two"]
    removed = api.relation_delete("ab-one", "ab-two", RelationType.blocks, path=graph)
    assert removed.success
    assert api.node("ab-one", path=graph).blocked_by in (None, [])


def test_comment_create_appends_typed(tmp_path):
    graph = _fixture(tmp_path)
    payload = api.comment_create("ab-one", CommentCreateInput(body="a note"), path=graph)
    assert payload.success
    conn = api.comments("ab-one", path=graph)
    assert [c.body for c in conn] == ["a note"]
    assert api.node("ab-one", path=graph).comments[0].body == "a note"


def test_label_add_and_remove(tmp_path):
    graph = _fixture(tmp_path)
    assert api.label_add("ab-one", "urgent", path=graph).success
    assert api.node("ab-one", path=graph).labels == ["infra", "urgent"]
    assert api.label_remove("ab-one", "infra", path=graph).success
    assert api.node("ab-one", path=graph).labels == ["urgent"]


def test_node_archive_hides_row_until_included(tmp_path):
    graph = _fixture(tmp_path)
    assert api.node_archive("ab-two", path=graph).success
    assert "ab-two" not in [n.id for n in api.nodes(path=graph).nodes]
    assert "ab-two" in [n.id for n in api.nodes(include_archived=True, path=graph).nodes]
    assert api.node_unarchive("ab-two", path=graph).success
    assert "ab-two" in [n.id for n in api.nodes(path=graph).nodes]


def test_node_delete_roundtrip(tmp_path):
    graph = _fixture(tmp_path)
    assert api.node_delete("ab-three", path=graph).success
    assert api.node("ab-three", path=graph) is None
    assert not api.node_delete("ab-three", path=graph).success
