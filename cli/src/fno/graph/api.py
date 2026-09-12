"""The typed graph client: one function per Rust ``backlog::api`` function.

Each call drives the store keeper's ``api`` command (one op per function,
same name and fields) and parses the reply into ``fno.graph.types`` models.
The backend is the store's own choice; callers never learn which one
answered. This wave ships the six functions the plan's verification names
plus the AC16 typed views; the remaining Rust names gain wrappers when
group 3 moves their first callers. ``cmd_version`` is the
``fno backlog version`` verb body, registered in ``fno.graph.cli``.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer

from fno.graph._constants import GRAPH_JSON
from fno.graph.store import _client_for
from fno.graph.types import (
    Comment,
    CommentCreateInput,
    Node,
    NodeConnection,
    NodeFilter,
    NodePayload,
    NodeUpdateInput,
    PageInfo,
    RelationType,
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


def node_update(node_id: str, input: NodeUpdateInput, *, path: Path = GRAPH_JSON) -> NodePayload:
    body = input.model_dump(exclude_none=True)
    return _payload(_api("node_update", {"id": node_id, "input": body}, path=path))


def relation_create(
    node_id: str, related_node_id: str, type: RelationType, *, path: Path = GRAPH_JSON
) -> NodePayload:
    params = {"id": node_id, "related": related_node_id, "type": type}
    return _payload(_api("relation_create", params, path=path))


def comment_create(node_id: str, input: CommentCreateInput, *, path: Path = GRAPH_JSON) -> NodePayload:
    return _payload(_api("comment_create", {"id": node_id, "input": input.model_dump()}, path=path))


def cmd_version() -> None:
    """Print the store's mutation counter: it grows one per write."""
    typer.echo(version())
