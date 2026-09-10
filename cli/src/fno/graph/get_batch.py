"""`fno backlog get`, several ids: forward to `fno-agents graph-get` (x-997a). Split out of graph/cli.py (over-budget)."""
from __future__ import annotations

import subprocess
from typing import List

import typer


def resolve_or_dispatch(ids: List[str], *, field: object, grouped: bool, strict: bool) -> str:
    """A single id: return it, or serve the exact hit from the keeper's
    by-id read (one row over the wire instead of the whole graph; falls back
    on any miss so tiers 1-3 and the archive walk stay with the caller).
    Several: dispatch the batch read, never return."""
    if len(ids) > 1:
        _dispatch(ids, field=field, grouped=grouped, strict=strict)
    token = ids[0]
    from fno.tracker import active_backend_name

    from fno.graph.store import read_nodes_by_ids

    if active_backend_name() == "graph":
        fast = read_nodes_by_ids(_graph_path(), [token])
        if fast and fast["entries"] and not fast["missing"]:
            e = fast["entries"][0]
            if e.get("id") == token or e.get("slug") == token.lower():
                _echo_entry(e, field, grouped)
                raise typer.Exit()
    return token


def _echo_entry(e: dict, field: object, grouped: bool) -> None:
    """Render the exact hit exactly as the caller's exact branch does: the
    one renderer, re-used, so the fast path's bytes are the slow path's."""
    from fno.graph.cli import _echo_node_entry
    from fno.graph._intake import project_root_from_settings

    # The caller's annotation is loose; the renderer wants str | None.
    field_name: "str | None" = field if isinstance(field, str) else None
    if field_name == "_status":
        field_name = "status"
    root = project_root_from_settings(e["project"]) if e.get("project") else None
    e["_resolved_cwd"] = root or e.get("cwd")
    _echo_node_entry(e, field_name, grouped)


def _graph_path():
    from fno.graph.cli import _graph_path as graph_path

    return graph_path()


def _dispatch(ids: List[str], *, field: object, grouped: bool, strict: bool) -> None:
    if field or grouped or strict:
        typer.echo(
            "fno backlog get: --field/--grouped/--strict take exactly one id; "
            "pass one id at a time for those, or drop them for a plain batch read.", err=True,
        )
        raise typer.Exit(code=2)
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno backlog get: the fno-agents binary was not found, and a batch read of "
            "several ids needs it. Reinstall fno, run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN. Pass one id at a time to use the all-Python path instead.", err=True,
        )
        raise typer.Exit(code=2)
    result = subprocess.run([str(binary), "graph-get", *ids, "--json"], check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
