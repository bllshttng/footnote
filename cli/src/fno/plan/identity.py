"""Shared identity checks for node-bearing plan filenames."""
from __future__ import annotations

import re
from pathlib import Path

_PLAN_NODE_RE = re.compile(r"-(?P<prefix>[a-z][a-z0-9]{0,7})-(?P<hex>[0-9a-f]{4,8})\.md$")


def plan_filename_node_id(path: str | Path, prefixes: set[str] | None = None) -> str | None:
    """Return the node id encoded by a plan basename, if it has one.

    Group-plan fragments are references to one file and are ignored; id-less
    plan names remain valid and return None.
    """
    basename = Path(str(path).split("#", 1)[0]).name
    match = _PLAN_NODE_RE.search(basename)
    if match is None:
        return None
    prefix = match.group("prefix")
    if prefixes is not None and prefix not in {p.rstrip("-") for p in prefixes}:
        return None
    return f"{prefix}-{match.group('hex')}"
