"""Project graph-node navigation fields onto a plan's frontmatter.

Python client for the keeper-served plan-doc writer (the Rust plan_doc
module). The graph is the authority; the frontmatter carries a projection so
Obsidian Bases can order "Next up" without a second lookup.
"""

from __future__ import annotations

import os
import sys
from typing import Any

from fno import paths
from fno.graph.store import StoreUnavailable, _client_for

_WARNED = False


def _warn_stale_keeper() -> None:
    """One stderr warning naming the fix, once per process."""
    global _WARNED
    if _WARNED:
        return
    _WARNED = True
    sys.stderr.write(
        "warning: the store keeper predates the plan-doc writer; the plan "
        "projection was skipped. Restart the keeper on a current "
        "fno-agents-worker - `fno doctor` names the lag.\n"
    )


def _call(op: str, params: dict) -> "dict | None":
    """One plan_docs request; degrades to a no-op, never raises.

    A StoreUnavailable, or a reply naming an unknown method (a stale keeper),
    prints one stderr warning naming `fno doctor` and returns None.
    """
    # The keeper runs in its own cwd; a relative path means the caller's.
    # Stamp events belong in the caller's project journal, as before the port.
    if params.get("plan_path"):
        params = {
            **params,
            "plan_path": os.path.abspath(params["plan_path"]),
            "events_path": str(paths.project_events_json()),
        }
    try:
        return _client_for(paths.graph_json()).request(
            "plan_docs", {"op": op, **params}
        )
    except StoreUnavailable as exc:
        if "unknown store method" in str(exc):
            _warn_stale_keeper()
            return None
        sys.stderr.write(
            f"warning: graph store unavailable; plan projection skipped "
            f"({exc}); run `fno doctor`\n"
        )
        return None
    except RuntimeError as exc:
        # A stale worker answers unknown-method as a plain store error.
        if "unknown store method" in str(exc):
            _warn_stale_keeper()
            return None
        raise


def project_graph_nodes(
    entries: "list[dict[str, Any]]",
    node_ids: "list[str]",
    root: "str | None" = None,
    *,
    mirror_keys_for: "tuple[str, frozenset[str]] | None" = None,
    force_status_off_terminal_for: "str | None" = None,
    clear_keys_for: "tuple[str, frozenset[str]] | None" = None,
) -> int:
    """Project each named node's mirror fields onto its linked plan.

    The shared converger primitive behind the mutating verbs and the plan sync
    sweep; returns the count of docs rewritten (0 on a degraded no-op). The
    keeper reads its own graph; `entries` stays in the signature so the call
    sites do not change.
    """
    ids = [i for i in dict.fromkeys(node_ids) if i]
    if not ids:
        return 0
    params: dict[str, Any] = {"ids": ids, "root": root}
    if mirror_keys_for is not None:
        params["mirror_keys_for"] = {"id": mirror_keys_for[0], "keys": sorted(mirror_keys_for[1])}
    if force_status_off_terminal_for is not None:
        params["force_status_off_terminal_for"] = force_status_off_terminal_for
    if clear_keys_for is not None:
        params["clear_keys_for"] = {"id": clear_keys_for[0], "keys": sorted(clear_keys_for[1])}
    result = _call("project", params)
    if result is None:
        return 0
    for warning in result.get("warnings") or []:
        sys.stderr.write(f"{warning}\n")
    return int(result.get("rewritten") or 0)


def stamp_plan(plan_path, session_id, urls=None, expected_url_count=None, dry_run=False) -> int:
    """Stamp a plan with shipping metadata; returns the writer's exit code."""
    result = _call(
        "stamp",
        {
            "plan_path": plan_path,
            "session_id": session_id,
            "urls": urls or [],
            "expected_url_count": expected_url_count,
            "dry_run": dry_run,
        },
    )
    return _exit(result)


def graduate_plan(plan_path, dry_run=False) -> int:
    """Graduate a stamped plan to done when the URL count is met."""
    return _exit(_call("graduate", {"plan_path": plan_path, "dry_run": dry_run}))


def _exit(result: "dict | None") -> int:
    """The writer's exit code; a failure's message goes to stderr."""
    if result is None:
        return 1
    if result.get("exit") and result.get("message"):
        sys.stderr.write(f"{result['message']}\n")
    return int(result.get("exit") or 0)


def set_expected_count(plan_path, count, dry_run=False) -> "tuple[int, str]":
    """Authoritatively write expected_url_count. Returns (exit, message)."""
    result = _call("set_expected", {"plan_path": plan_path, "count": count, "dry_run": dry_run})
    if result is None:
        return 1, "graph store unavailable"
    return int(result.get("exit") or 0), str(result.get("message") or "")


def epic_waves(epic_id: str) -> "tuple[dict[str, int], int]":
    """An epic's wave strata: (wave_by_child_id, max_wave). ({}, -1) degraded."""
    result = _call("waves", {"epic_id": epic_id})
    if result is None:
        return {}, -1
    waves = {k: int(v) for k, v in (result.get("wave_by_id") or {}).items()}
    return waves, int(result.get("max_wave", -1))