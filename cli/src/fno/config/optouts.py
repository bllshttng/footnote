"""Merge-gating configuration values that require a live opt-out claim.

This module is platform data: the measured key set and the raw-dict helpers
its consumers share. The claims-backed half of the feature, the read-time
revocation guard and the write-time lease, lives in
``fno.claims.optout_lease`` because this package may not import the claims
layer.
"""

from __future__ import annotations

from typing import Any

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


def optout_release_command(key: str, scope_flag: str = "") -> str:
    """The command a holder runs to release the opt-out at ``key``."""
    if key == "review.optional_apps":
        return f"fno config unset {key}{scope_flag}"
    return f"fno config set {key} true{scope_flag}"


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
