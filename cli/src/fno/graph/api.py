"""The typed graph client: one function per Rust ``backlog::api`` function.

Each call drives the store keeper's ``api`` command (one op per function,
same name and fields) and parses the reply into ``fno.graph.types`` models.
The backend is the store's own choice; callers never learn which one
answered. ``cmd_version`` is the ``fno backlog version`` verb body,
registered in ``fno.graph.cli``.
"""
from __future__ import annotations

from pathlib import Path
from typing import List, Optional

import typer

from fno.graph._constants import GRAPH_JSON
from fno.graph.store import _client_for
from fno.graph.types import (
    Comment,
    CommentCreateInput,
    Dispatch,
    EncounterInput,
    Node,
    NodeConnection,
    NodeCreateInput,
    NodeFilter,
    NodePayload,
    NodeUpdateInput,
    PageInfo,
    PullRequestInput,
    RelationType,
    SessionRecord,
)


def _api(op: str, params: dict, *, path: Path = GRAPH_JSON) -> dict:
    return _client_for(path).request("api", {"op": op, **params})


def _connection(reply: dict) -> NodeConnection:
    return NodeConnection(
        nodes=[Node.model_validate(row) for row in reply.get("nodes", [])],
        page_info=PageInfo.model_validate(reply.get("page_info") or {}),
    )


def _payload(reply: dict) -> NodePayload:
    body = reply.get("node")
    return NodePayload(
        success=bool(reply.get("success")),
        node=Node.model_validate(body) if body else None,
        version=int(reply["version"]),
    )


def node(node_id: str, *, path: Path = GRAPH_JSON) -> Optional[Node]:
    reply = _api("node", {"id": node_id}, path=path)
    body = reply.get("node")
    return Node.model_validate(body) if body else None


def nodes(
    filter: Optional[NodeFilter] = None,
    *,
    first: Optional[int] = None,
    after: Optional[str] = None,
    include_archived: bool = False,
    order_by: str = "ordinal",
    path: Path = GRAPH_JSON,
) -> NodeConnection:
    page = {
        "first": first,
        "after": after,
        "include_archived": include_archived,
        "order_by": order_by,
    }
    body = filter.model_dump(exclude_none=True) if filter else {}
    return _connection(_api("nodes", {"filter": body, **page}, path=path))


def comments(node_id: str, *, first: Optional[int] = None, path: Path = GRAPH_JSON) -> list:
    """The node's progress notes, typed; the connection's page info is
    keeper-side detail no caller needs this wave."""
    reply = _api("comments", {"id": node_id, "first": first}, path=path)
    return [Comment.model_validate(row) for row in reply.get("nodes", [])]


def version(*, path: Path = GRAPH_JSON) -> int:
    return int(_api("version", {}, path=path)["version"])


def node_create(input: NodeCreateInput, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("node_create", {"input": input.model_dump(exclude_none=True)}, path=path))


def node_update(node_id: str, input: NodeUpdateInput, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("node_update", {"id": node_id, "input": input.model_dump(exclude_none=True)}, path=path))


def node_batch_update(ids: List[str], input: NodeUpdateInput, *, path: Path = GRAPH_JSON) -> List[Node]:
    reply = _api("node_batch_update", {"ids": ids, "input": input.model_dump(exclude_none=True)}, path=path)
    return [Node.model_validate(row) for row in (reply.get("node") or [])]


def node_archive(node_id: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("node_archive", {"id": node_id}, path=path))


def node_unarchive(node_id: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("node_unarchive", {"id": node_id}, path=path))


def node_delete(node_id: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("node_delete", {"id": node_id}, path=path))


def relation_create(
    node_id: str, related_node_id: str, type: RelationType, *, path: Path = GRAPH_JSON
) -> NodePayload:
    return _payload(_api("relation_create", {"id": node_id, "related": related_node_id, "type": type}, path=path))


def relation_delete(
    node_id: str, related_node_id: str, type: RelationType, *, path: Path = GRAPH_JSON
) -> NodePayload:
    return _payload(_api("relation_delete", {"id": node_id, "related": related_node_id, "type": type}, path=path))


def label_add(node_id: str, name: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("label_add", {"id": node_id, "name": name}, path=path))


def label_remove(node_id: str, name: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("label_remove", {"id": node_id, "name": name}, path=path))


def comment_create(node_id: str, input: CommentCreateInput, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("comment_create", {"id": node_id, "input": input.model_dump()}, path=path))


def pull_request_attach(node_id: str, input: PullRequestInput, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("pull_request_attach", {"id": node_id, "input": input.model_dump(exclude_none=True)}, path=path))


def session_append(node_id: str, row: SessionRecord, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("session_append", {"id": node_id, "row": row.model_dump(exclude_none=True)}, path=path))


def session_end(node_id: str, session_id: str, ended_by: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("session_end", {"id": node_id, "session_id": session_id, "ended_by": ended_by}, path=path))


def encounter_create(node_id: str, input: EncounterInput, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("encounter_create", {"id": node_id, "input": input.model_dump()}, path=path))


def dispatch_set(node_id: str, dispatch: Optional[Dispatch], *, path: Path = GRAPH_JSON) -> NodePayload:
    body = dispatch.model_dump(exclude_none=True) if dispatch else None
    return _payload(_api("dispatch_set", {"id": node_id, "dispatch": body}, path=path))


def cmd_version() -> None:
    """Print the store's mutation counter: it grows one per write."""
    typer.echo(version())
