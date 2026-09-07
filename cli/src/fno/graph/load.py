"""graph/load.py - the graph reader.

Public API:
    load_graph(path)    - Read graph.json and return the defaulted entries.

Reads the bytes through the store keeper's gated read (the same gate the
write path publishes under) and parses them. The SHA256 sidecar this module
used to carry is gone: it raised a mismatch on ordinary sessions whose every
write had landed, and the documented remedy was a rehash everyone ran by
reflex. The keeper's version conflict is what serializes writers now.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.graph._constants import GRAPH_JSON


def load_graph(path: Path | None = None, *, keep_malformed: bool = False) -> list[dict]:
    """Read graph.json and return its entries.

    Args:
        path: Path to graph.json. Defaults to ~/.fno/graph.json.

    Returns:
        List of graph entry dicts with the canonical migration/defaults pass
        applied (see :func:`_entries`) -- the same vocabulary ``read_graph``
        returns, not the raw on-disk rows.
    """
    if path is None:
        path = GRAPH_JSON

    if not path.exists():
        return []

    from fno.graph.store import read_file_bytes

    raw_bytes = read_file_bytes(Path(path))
    return _entries(json.loads(raw_bytes), keep_malformed=keep_malformed)


def _entries(data: object, *, keep_malformed: bool = False) -> list[dict]:
    """Extract the entry list and run the canonical migration/defaults pass.

    One seam: this is the same ``_apply_graph_defaults`` ``read_graph`` uses
    (the ported store's defaults pipeline), so a row whose on-disk ``status``
    predates a rename (``claimed`` -> ``in_progress``) reads identically no
    matter which reader a caller reached for.

    Imported function-locally to keep this module free of a load-time dependency on
    ``store`` (which is the write path), matching ``query_by_source_inbox_msg`` below.
    """
    from fno.graph.store import _apply_graph_defaults

    return _apply_graph_defaults(
        data.get("entries", []) if isinstance(data, dict) else [],
        keep_malformed=keep_malformed,
    )


def query_by_source_inbox_msg(msg_id: str, path: Path | None = None) -> list[dict]:
    """Return sidecar rows whose source_inbox_msg matches msg_id.

    source_inbox_msg is footnote-owned provenance (the same family as
    source_node_id / source_plan_path), so the scan runs over the sidecar
    projection and works on any tracker backend. An explicit ``path`` (a
    hermetic-test redirect) is still honored by reading that file directly.
    """
    if path is not None:
        from fno.graph.store import read_graph

        return [e for e in read_graph(path) if e.get("source_inbox_msg") == msg_id]
    from fno.tracker import sidecar as sidecar_store

    return [
        {"id": nid, "source_inbox_msg": sc.source_inbox_msg}
        for nid, sc in sidecar_store.load_all().items()
        if sc.source_inbox_msg == msg_id
    ]
