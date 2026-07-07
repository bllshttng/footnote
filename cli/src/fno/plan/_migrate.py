"""fno.plan._migrate - relocate a folder plan into a single-doc plan.

Relocation, not reformat: the folder's ``00-INDEX.md`` (frontmatter + body)
moves verbatim to the head of the new doc, so stamp frontmatter and the
``## Execution Strategy`` YAML are byte-preserved. Each ``NN-*.md`` phase body
is inlined in filename order (its own frontmatter stripped); an existing
``COMPLETION.md`` folds in as a ``## Completion Log`` section. The original
folder is preserved beside the new ``<name>.md`` with an ``-archived`` suffix
(rollback = rename). Idempotent: a folder already migrated is a no-op notice.

The write is crash-safe: the new doc lands via a temp file + atomic replace,
and the folder is renamed only after the doc is on disk. Any mid-write failure
leaves the original folder untouched with no partial doc at the target path.
"""
from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path

from fno.plan._doc import _split_frontmatter
from fno.plan._locate import PlanNotFound, locate_plan

# Statuses whose plan_path rewrite would arm the active-backlog auto-dispatcher
# (or belongs to a live session). ``--update-node`` refuses these; only
# terminal / paused nodes (done, deferred, superseded, blocked) may be
# repointed. One stale-plan auto-dispatch incident kills the bulk path.
_DISPATCH_ARMED_STATUSES = frozenset({"ready", "idea", "claimed"})

_ARCHIVED_SUFFIX = "-archived"


class MigrateError(Exception):
    """Raised when a folder plan cannot be migrated. ``kind`` classifies it."""

    def __init__(self, message: str, kind: str = "error") -> None:
        super().__init__(message)
        self.kind = kind


@dataclass(frozen=True)
class MigrateResult:
    new_doc_path: Path | None
    archived_dir: Path | None
    phase_count: int
    folded_completion: bool
    node_updated: bool
    skipped: bool
    message: str


def _phase_files(folder: Path) -> list[Path]:
    """Return the ``NN-*.md`` phase files in numeric order, excluding 00-INDEX."""
    phases = [
        p
        for p in folder.iterdir()
        if p.is_file()
        and p.name != "00-INDEX.md"
        and re.match(r"^\d{2}-.*\.md$", p.name)
    ]
    return sorted(phases, key=lambda p: p.name)


def _strip_frontmatter(text: str) -> str:
    """Drop a leading YAML frontmatter block, returning the body only."""
    _, body = _split_frontmatter(text)
    return body


def _build_single_doc(folder: Path) -> tuple[str, int, bool]:
    """Assemble the single-doc text from a folder plan.

    Returns ``(content, phase_count, folded_completion)``. The 00-INDEX text is
    copied verbatim so frontmatter and the Execution Strategy YAML are
    byte-identical; phase bodies are inlined frontmatter-stripped in order; a
    COMPLETION.md (if present) becomes a ``## Completion Log`` section.
    """
    index_text = (folder / "00-INDEX.md").read_text(encoding="utf-8")
    parts = [index_text.rstrip("\n")]

    phases = _phase_files(folder)
    for phase in phases:
        body = _strip_frontmatter(phase.read_text(encoding="utf-8")).strip("\n")
        parts.append(f"<!-- phase: {phase.name} -->\n\n{body}")

    completion = folder / "COMPLETION.md"
    folded = completion.exists()
    if folded:
        comp_body = _strip_frontmatter(
            completion.read_text(encoding="utf-8")
        ).strip("\n")
        parts.append(f"## Completion Log\n\n{comp_body}")

    return "\n\n".join(parts) + "\n", len(phases), folded


def _node_status(node_id: str) -> str | None:
    """Derived ``_status`` of *node_id*, or None if the node is absent."""
    from fno.graph._constants import GRAPH_JSON
    from fno.graph._intake import _find_node
    from fno.graph.statuses import recompute_statuses
    from fno.graph.store import read_graph

    entries = recompute_statuses(read_graph(GRAPH_JSON))
    node = _find_node(entries, node_id)
    return node.get("_status") if node else None


def _set_node_plan_path(node_id: str, new_path: str) -> None:
    """Repoint *node_id*'s plan_path under the graph lock."""
    from fno.graph._constants import GRAPH_JSON
    from fno.graph._intake import _find_node
    from fno.graph.store import locked_mutate_graph

    def _mutate(entries: list[dict]) -> list[dict]:
        node = _find_node(entries, node_id)
        if node is None:
            raise MigrateError(f"node not found: {node_id}", kind="node-missing")
        node["plan_path"] = new_path
        return entries

    locked_mutate_graph(GRAPH_JSON, _mutate)


def migrate_folder(
    folder: str | Path,
    *,
    update_node: str | None = None,
) -> MigrateResult:
    """Migrate a folder plan to a single-doc plan.

    Args:
        folder: the folder-plan directory (must contain 00-INDEX.md).
        update_node: when set, repoint this graph node's plan_path to the new
            doc after a successful migration. Refuses if the node sits at a
            dispatch-armed status (ready/idea/claimed) - checked BEFORE any
            write, so a refusal leaves both the folder and plan_path untouched.

    Raises:
        MigrateError: on a missing/invalid folder, a dispatch-armed
            ``--update-node`` target, or a mid-write failure.
    """
    folder = Path(folder)

    # Idempotency: an already-archived folder, or one whose target doc already
    # exists beside it, is a no-op notice.
    if folder.name.endswith(_ARCHIVED_SUFFIX):
        return MigrateResult(
            None, None, 0, False, False, True,
            f"already archived (no-op): {folder}",
        )
    target = folder.parent / f"{folder.name}.md"
    if target.exists():
        return MigrateResult(
            None, None, 0, False, False, True,
            f"already migrated (no-op): {target} exists beside {folder.name}",
        )

    # Validate shape via the canonical locator (same 00-INDEX.md contract).
    try:
        resolved = locate_plan(folder)
    except PlanNotFound as exc:
        raise MigrateError(str(exc), kind="not-found") from exc
    if resolved.kind != "folder":
        raise MigrateError(
            f"not a folder plan (no 00-INDEX.md): {folder}", kind="not-found"
        )

    # Auto-dispatch firewall: refuse a dispatch-armed --update-node BEFORE any
    # write so the folder and plan_path stay untouched (AC1-ERR).
    if update_node is not None:
        status = _node_status(update_node)
        if status is None:
            raise MigrateError(
                f"--update-node: node not found: {update_node}",
                kind="node-missing",
            )
        if status in _DISPATCH_ARMED_STATUSES:
            raise MigrateError(
                f"--update-node refused: node {update_node} is '{status}'. "
                f"Rewriting plan_path on a {status} node arms the "
                f"active-backlog dispatcher (a stale plan would launch a "
                f"build within ~1 min). Triage first: archive/stamp the dead "
                f"node or move it off a dispatchable status, then migrate.",
                kind="dispatch-armed",
            )

    # Build the doc fully in memory; a read failure here writes nothing.
    content, phase_count, folded = _build_single_doc(folder)

    archived_dir = folder.parent / f"{folder.name}{_ARCHIVED_SUFFIX}"
    if archived_dir.exists():
        raise MigrateError(
            f"archive target already exists: {archived_dir}", kind="collision"
        )

    # Atomic-ish landing: temp doc -> replace to target (doc lands) -> rename
    # folder. On folder-rename failure, unlink the target so no partial doc
    # survives and the folder is left untouched (AC2-ERR).
    tmp = folder.parent / f".{folder.name}.md.tmp"
    tmp.write_text(content, encoding="utf-8")
    try:
        os.replace(tmp, target)
    except OSError:
        tmp.unlink(missing_ok=True)
        raise
    try:
        os.rename(folder, archived_dir)
    except OSError as exc:
        target.unlink(missing_ok=True)
        raise MigrateError(
            f"doc written but folder rename failed; rolled back: {exc}",
            kind="rename-failed",
        ) from exc

    node_updated = False
    if update_node is not None:
        _set_node_plan_path(update_node, str(target))
        node_updated = True

    msg = (
        f"migrated {folder.name} -> {target.name} "
        f"({phase_count} phase(s){', +completion log' if folded else ''}); "
        f"folder archived at {archived_dir.name}"
    )
    if node_updated:
        msg += f"; node {update_node} plan_path repointed"
    return MigrateResult(target, archived_dir, phase_count, folded, node_updated, False, msg)
