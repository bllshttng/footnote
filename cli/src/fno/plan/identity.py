"""Shared identity checks for node-bearing plan filenames."""
from __future__ import annotations

import re
from pathlib import Path

from fno.graph._constants import NODE_ID_BODY

_PLAN_NODE_RE = re.compile(rf"(?<![a-z0-9])({NODE_ID_BODY})\.md$")


def plan_filename_node_id(path: str | Path) -> str | None:
    """Return the node id encoded by a plan basename, if it has one.

    Group-plan fragments are references to one file and are ignored while
    identifying the basename. Id-less plan names remain valid and return None.
    """
    basename = Path(str(path).split("#", 1)[0]).name
    match = _PLAN_NODE_RE.search(basename)
    return match.group(1) if match else None
