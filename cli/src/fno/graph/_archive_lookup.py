"""Tell an archived node id apart from an absent one, and refuse on it.

The archive lives in the graph store a not-found refusal never mentions, so
an id that lives there needs its own answer: which row it is, and the verb
that restores it. Reopen (exit 4, its own wording) and update (exit 1) share
the lookup; each verb keeps its own exit code and message.
"""

from __future__ import annotations

import sys
from typing import Any, Optional


def archived_entry(node_id: str) -> Optional[dict[str, Any]]:
    """The node's archived row from the store, or None. Read-only, never raises.

    Reopen needs this to tell "archived" apart from "absent". Without it an
    archived node reports "not found", which is the same message a typo gets,
    while the node sits readable in the store - an absence with two
    explanations and no way to distinguish them.
    """
    from fno.graph._intake import _find_node
    from fno.graph.store import read_archive_entries

    try:
        # The archive is default-backend storage: never consulted behind an
        # external selection.
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            return None
        # `_find_node`, not an exact compare: it is what resolved the id against
        # the working graph, so an abbreviated id resolves the same way here.
        return _find_node(read_archive_entries(), node_id)
    except Exception:  # noqa: BLE001 - the archive is advisory; a bad read must not mask the real refusal
        return None


def refuse_update_if_archived(node_id: str) -> bool:
    """Print the update refusal and return True when node_id is archived."""
    entry = archived_entry(node_id)
    if entry is None:
        return False
    aid = entry.get("id") or node_id
    print(
        f"Error: node {aid} is archived; run `fno backlog unarchive {aid}`"
        " to restore it before updating.",
        file=sys.stderr,
    )
    return True
