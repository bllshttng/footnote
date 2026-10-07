"""The registry table door for Python callers.

The registry lives in the graph database. ``registry.json`` is a migration
fence, never a file to parse, so every Python read and write goes through the
native ``registry-commit`` verb.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Optional


class RegistryDoorError(RuntimeError):
    """The native registry door refused or could not run."""


def read_registry_document(path: Optional[Path] = None) -> tuple[dict[str, Any], int]:
    """Return the whole registry document and its table revision."""
    from fno import paths, rust_binary

    target = Path(path) if path is not None else Path(paths.agents_registry_path())
    try:
        answer = rust_binary.verb_call(
            "registry-commit", {"path": str(target.absolute()), "op": "read"}
        )
    except rust_binary.VerbUnavailable as exc:
        raise RegistryDoorError(f"registry read refused {target}: {exc}") from exc
    document = answer.get("document")
    revision = answer.get("revision")
    if not isinstance(document, dict) or not isinstance(revision, int):
        raise RegistryDoorError(f"registry read returned no document for {target}")
    return document, revision


def read_registry_rows(path: Optional[Path] = None) -> list[dict[str, Any]]:
    """Tolerant row read: an unreadable registry reads as no rows."""
    try:
        document, _ = read_registry_document(path)
    except RegistryDoorError:
        return []
    rows = document.get("agents")
    return [row for row in rows if isinstance(row, dict)] if isinstance(rows, list) else []


def commit_registry_document(
    path: Path, payload: dict[str, Any], revision: int
) -> None:
    """Write ``payload`` if the table is still at ``revision``."""
    from fno import rust_binary

    try:
        answer = rust_binary.verb_call(
            "registry-commit",
            {**payload, "path": str(Path(path).absolute()), "revision": revision},
        )
    except rust_binary.VerbUnavailable as exc:
        raise RegistryDoorError(f"registry-commit refused {path}: {exc}") from exc
    if answer.get("status") != "written":
        detail = answer.get("message") or answer.get("reason") or "unknown refusal"
        raise RegistryDoorError(f"registry-commit refused {path}: {detail}")
