"""Shared reader for the immutable target session manifest.

One typed contract for every consumer of ``.fno/target-state.md`` so the
resume-bind primitive and the orienter cannot drift apart on what a manifest
looks like. The manifest is YAML frontmatter (validated by
:func:`fno.agent.state.load_agent_context`) PLUS body ``key: value`` lines that
init writes below the frontmatter for the body-only identity fields. Both halves
are merged here.

The manifest is the identity anchor for native resume (x-2ccd): its
``harness``/``harness_session_id``/``fno_id``/``graph_node_id``/
``target_claim_holder`` tuple is what a resumed process must match to rebind
the node claim.
"""
from __future__ import annotations

import re
from pathlib import Path
from typing import Any, Optional


# Body keys appended below the frontmatter (init-target-state.sh writes them as
# `key: value` lines, NOT YAML frontmatter), so load_agent_context (frontmatter
# only) never sees them. Kept here as the single source of the body-key set.
BODY_KEYS = ("graph_node_id", "target_claim_key", "target_claim_holder")


def read_target_manifest(project_root: Path) -> Optional[dict[str, Any]]:
    """Merged session manifest: frontmatter + the body ``key: value`` lines.

    Returns the frontmatter dict (from :func:`load_agent_context`) enriched with
    any body-only keys (``graph_node_id`` / ``target_claim_*``) the frontmatter
    did not already carry. ``None`` when no manifest exists. Never raises.

    ``project_root`` pins both the frontmatter read and the body read to the
    SAME manifest (``load_agent_context`` otherwise detects the root from cwd,
    which can differ under ``FNO_REPO_ROOT`` or a subdirectory).
    """
    raw: Optional[dict[str, Any]] = None
    try:
        from fno.agent.state import load_agent_context

        ctx = load_agent_context(project_root_override=project_root)
        if ctx.session is not None:
            raw = dict(ctx.session.raw)
    except Exception:  # noqa: BLE001 - no/unreadable manifest is fine
        pass
    manifest = project_root / ".fno" / "target-state.md"
    try:
        text = manifest.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return raw
    for key in BODY_KEYS:
        if raw and raw.get(key):
            continue
        m = re.search(rf"^{key}\s*:\s*(.+)$", text, re.MULTILINE)
        if m:
            val = m.group(1).strip().strip("\"'")
            if val and val != "null":
                raw = raw or {}
                raw[key] = val
    return raw


def manifest_identity(raw: Optional[dict[str, Any]]) -> Optional[dict[str, str]]:
    """The minimal identity tuple a resume-bind needs, or None if incomplete.

    Returns ``{harness, harness_session_id, fno_id, graph_node_id,
    target_claim_key, target_claim_holder}`` when every field is present and
    non-null, else None. A manifest missing any of these cannot prove a resumed
    process belongs to this target attempt, so the rebind must refuse.
    """
    if not raw:
        return None
    fields = (
        "harness",
        "harness_session_id",
        "fno_id",
        "graph_node_id",
        "target_claim_key",
        "target_claim_holder",
    )
    ident: dict[str, str] = {}
    for f in fields:
        val = raw.get(f)
        val = str(val).strip().strip("\"'") if val is not None else ""
        if not val or val == "null":
            return None
        ident[f] = val
    return ident
