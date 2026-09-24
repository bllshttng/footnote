"""The typed graph client: one function per Rust ``backlog::api`` function.

Each call drives the store keeper's ``api`` command (one op per function,
same name and fields) and parses the reply into ``fno.graph.types`` models.
``cmd_version`` is the ``fno backlog version`` verb body.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

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
    page = {"first": first, "after": after, "include_archived": include_archived, "order_by": order_by}
    body = filter.model_dump(exclude_none=True) if filter else {}
    return _connection(_api("nodes", {"filter": body, **page}, path=path))


def comments(node_id: str, *, first: Optional[int] = None, path: Path = GRAPH_JSON) -> list:
    """The node's progress notes, typed; page info is keeper-side detail."""
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


def archive_node(node_id: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    """Fold a node into the archive: it stops answering default reads."""
    return _payload(_api("node_archive", {"id": node_id}, path=path))


def unarchive_node(node_id: str, *, path: Path = GRAPH_JSON) -> NodePayload:
    """Restore an archived node to the live board."""
    return _payload(_api("node_unarchive", {"id": node_id}, path=path))


def cmd_version() -> None:
    """Print the store's mutation counter: it grows one per write."""
    import typer

    typer.echo(version())


def wire_rows(*, path: Path = GRAPH_JSON, include_archived: bool = False) -> list[dict]:
    """Wire rows; unrepresentable rows ride verbatim and archived residents
    answer only when asked for. Keeper failures remain visible to callers."""
    from pydantic import ValidationError

    reply = _api("rows", {"include_archived": include_archived}, path=path)
    out: list[dict] = []
    for row in reply.get("rows") or []:
        # The rows op serves archived residents raw (only the nodes op
        # filters server-side), so the default read hides them here.
        if not include_archived and isinstance(row, dict) and row.get("archived_at"):
            continue
        try:
            dumped = Node.model_validate(row).model_dump(by_alias=True)
        except ValidationError:
            out.append(row)  # an unrepresentable row rides verbatim
            continue
        if dumped.get("persisted_status"):
            dumped["status"] = dumped["persisted_status"]
        out.append(dumped)
    return out


def decisions(
    node: Optional[str] = None,
    decision_id: Optional[str] = None,
    *,
    path: Path = GRAPH_JSON,
) -> list[dict]:
    """The flattened decision rows from the store's decisions table after
    migration: data fields at the top level plus ``ts`` and ``_event_type``,
    the shape the file reader handed out. A node filter reads the node's
    own list; no filter reads the machine-wide index in file order."""
    params: dict = {}
    if node:
        params["node"] = node
    if decision_id:
        params["decision_id"] = decision_id
    return _api("decisions", params, path=path).get("rows", [])


def decision_record(event: dict, *, path: Path = GRAPH_JSON) -> dict:
    """Append one ``operator_decision`` event row to the decisions table."""
    return _api("decision_record", {"event": event}, path=path)


def decision_retract(event: dict, *, path: Path = GRAPH_JSON) -> dict:
    """Append one ``decision_retracted`` event row to the decisions table."""
    return _api("decision_retract", {"event": event}, path=path)
