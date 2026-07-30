"""Error-aware filesystem enumeration for harness session stores."""

from __future__ import annotations

import os
import stat
from collections.abc import Callable
from pathlib import Path
from typing import Optional


def path_exists_strict(path: Path) -> bool:
    """Return false only for absence; propagate permission and I/O failures."""
    try:
        os.stat(path)
    except FileNotFoundError:
        return False
    return True


def scan_files(
    root: Path,
    *,
    max_depth: Optional[int] = None,
    include: Optional[Callable[[str], bool]] = None,
) -> list[Path]:
    """Enumerate files without letting directory read errors look like emptiness.

    ``pathlib.glob`` and ``rglob`` may silently skip unreadable children. Identity
    selection and collision measurement need a complete corpus, so this walker
    returns an empty list only for an absent root and propagates every directory
    enumeration error.
    """
    try:
        root_stat = os.stat(root)
    except FileNotFoundError:
        return []
    if not stat.S_ISDIR(root_stat.st_mode):
        raise NotADirectoryError(str(root))

    found: list[Path] = []

    def visit(directory: Path, depth: int) -> None:
        with os.scandir(directory) as entries:
            snapshot = list(entries)
        for entry in snapshot:
            if entry.is_dir(follow_symlinks=False):
                if max_depth is None or depth < max_depth:
                    visit(Path(entry.path), depth + 1)
                continue
            if entry.is_file(follow_symlinks=False) and (
                include is None or include(entry.name)
            ):
                found.append(Path(entry.path))

    visit(root, 0)
    return found
