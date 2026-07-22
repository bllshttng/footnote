"""fno.plan._folder_audit - precondition check for folder-reader removal.

Counts folder plans (dirs holding ``00-INDEX.md``) whose owning graph node is
NOT terminal. "Owning" is a basename join on ``plan_path`` (the fno->fno
rename + ``internal/`` symlink mean absolute roots differ between the graph's
recorded ``plan_path`` and the vault's resolved plans dir - x-429f established
this join is the only reliable match). A folder plan with no owning node, or
one owned by a terminal node, does not block: frontmatter status is proven
stale (many ``ready`` folder plans have ``done`` nodes), so this must key off
the graph, never off frontmatter.

Fails toward defer: an unreadable graph or an unscannable plans dir is treated
as blocking (returns None from :func:`scan`, never a false-clean zero).
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

TERMINAL_STATUSES = frozenset({"done", "superseded", "archived", "cancelled"})


@dataclass(frozen=True)
class NonTerminalOwner:
    node_id: str
    status: str
    plan_path: str
    folder: str


def _folder_plan_dirs(plans_root: Path) -> list[Path]:
    """Every directory under *plans_root* that directly contains 00-INDEX.md."""
    return [p.parent for p in plans_root.rglob("00-INDEX.md")]


def scan(plans_root: Path, graph_entries: list[dict]) -> list[NonTerminalOwner] | None:
    """Return non-terminal folder-plan owners, or None if the scan is unreliable.

    None signals "fail toward defer" to the caller (unreadable vault); an
    empty list is a genuine zero.
    """
    if not plans_root.is_dir():
        return None

    try:
        folder_dirs = _folder_plan_dirs(plans_root)
    except OSError:
        return None

    folder_basenames = {d.name for d in folder_dirs}

    owners: list[NonTerminalOwner] = []
    for entry in graph_entries:
        plan_path = entry.get("plan_path") or ""
        if not plan_path:
            continue
        base = Path(plan_path.rstrip("/")).name
        if base not in folder_basenames:
            continue
        status = entry.get("status") or ""
        if status in TERMINAL_STATUSES:
            continue
        owners.append(
            NonTerminalOwner(
                node_id=entry.get("id", ""),
                status=status,
                plan_path=plan_path,
                folder=base,
            )
        )
    return owners
