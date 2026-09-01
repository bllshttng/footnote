"""Merge-gating configuration values that require a live opt-out claim."""

from __future__ import annotations

import logging
import os
from typing import Any

from fno.claims import ClaimState, claim_status
from fno.claims.io import claims_dir, claims_root_for

_LOG = logging.getLogger(__name__)

# A key belongs here only when its opt-out weakens a merge decision. The
# reader and writer both use this measured set so a new switch cannot silently
# acquire a different lifetime rule.
MERGE_GATING_OPTOUTS: dict[str, object] = {
    "review.self_review_required": False,
    "review.optional_apps": [],
    "auto_merge.require_checks_pass": False,
}

# Defaults are kept here so revocation does not import the model while the model
# imports the registry. ``None`` for optional_apps is intentional: the model
# expands an absent value to its built-in optional reviewer list.
MERGE_GATING_OPTOUT_DEFAULTS: dict[str, object] = {
    "review.self_review_required": True,
    "review.optional_apps": "built-in optional reviewer list",
    "auto_merge.require_checks_pass": True,
}


def _raw_leaf(raw: dict[str, Any], key: str) -> tuple[bool, Any]:
    node: Any = raw
    for part in key.split("."):
        if not isinstance(node, dict) or part not in node:
            return False, None
        node = node[part]
    return True, node


def _drop_raw_leaf(raw: dict[str, Any], key: str) -> None:
    parts = key.split(".")
    node: Any = raw
    parents: list[tuple[dict[str, Any], str]] = []
    for part in parts[:-1]:
        if not isinstance(node, dict) or not isinstance(node.get(part), dict):
            return
        parents.append((node, part))
        node = node[part]
    if not isinstance(node, dict):
        return
    node.pop(parts[-1], None)
    for parent, part in reversed(parents):
        if isinstance(parent.get(part), dict) and not parent[part]:
            del parent[part]
        else:
            break


def _claim_state(key: str) -> str:
    """Read the opt-out instrument, distinguishing unreadable from absent."""
    root = claims_root_for(f"config-optout:{key}")
    try:
        directory = claims_dir(root)
        try:
            directory.stat()
        except FileNotFoundError:
            pass
        except OSError:
            return "unreadable"
        else:
            try:
                with os.scandir(directory):
                    pass
            except OSError:
                return "unreadable"
        return str(
            claim_status(f"config-optout:{key}", root=root).get(
                "state", "unreadable"
            )
        )
    except Exception:  # noqa: BLE001 - unreadable instrument revokes the opt-out
        return "unreadable"


def revoke_unbacked_optouts(raw: dict[str, Any]) -> dict[str, Any]:
    """Drop merge-gating opt-outs unless their global claim is LIVE.

    This is deliberately a read-time guard. A failed or delayed reaper leaves
    the file value inert, so a fresh process still requires the gate.
    """
    for key, optout in MERGE_GATING_OPTOUTS.items():
        present, value = _raw_leaf(raw, key)
        # Explicit presence matters for optional_apps: absent means the model's
        # built-in list, while [] is the actual opt-out.
        if not present or value != optout:
            continue
        state = _claim_state(key)
        if state == ClaimState.LIVE.value:
            continue
        _drop_raw_leaf(raw, key)
        _LOG.warning(
            "merge-gating opt-out %s revoked: claim instrument is %s; "
            "resolved default is %r",
            key,
            state,
            MERGE_GATING_OPTOUT_DEFAULTS[key],
        )
    return raw
