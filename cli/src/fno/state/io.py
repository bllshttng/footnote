"""Frontmatter IO and atomic write helpers for fno state files.

State files use a simple YAML frontmatter format:

    ---
    key: value
    ---
    body text

read_frontmatter(path) -> (dict, body_str)
write_frontmatter(path, data, body) -> None  (via atomic_write)
atomic_write(path, content) -> None          (filelock + tempfile + os.replace)
"""
from __future__ import annotations

import os
import tempfile
from pathlib import Path
from typing import Dict, Any, Tuple

import filelock
import yaml


class _StringPreservingTimestampLoader(yaml.SafeLoader):
    """SafeLoader that keeps ISO timestamps as strings.

    yaml.SafeLoader auto-coerces `2026-05-21T00:00:00Z` to a datetime.datetime,
    which then fails schemas declaring the field as `Optional[str]`. Schemas
    in this repo treat ISO timestamps as strings (the on-disk shape), so we
    strip the timestamp implicit resolver while preserving int/bool/float
    coercion.
    """


_StringPreservingTimestampLoader.yaml_implicit_resolvers = {
    k: [r for r in v if r[0] != "tag:yaml.org,2002:timestamp"]
    for k, v in _StringPreservingTimestampLoader.yaml_implicit_resolvers.items()
}


def read_frontmatter(path: Path) -> Tuple[Dict[str, Any], str]:
    """Parse YAML frontmatter from a state file.

    Args:
        path: Path to the state file.

    Returns:
        Tuple of (frontmatter_dict, body_string).
        If no frontmatter delimiters found, returns ({}, full_content).

    Raises:
        FileNotFoundError: if path does not exist.
    """
    path = Path(path)
    if not path.exists():
        raise FileNotFoundError(f"state file not found: {path}")

    text = path.read_text(encoding="utf-8")
    return _parse_frontmatter(text)


def _parse_frontmatter(text: str) -> Tuple[Dict[str, Any], str]:
    """Split text into (frontmatter_dict, body) without third-party libs."""
    if not text.startswith("---"):
        return {}, text

    # Find the closing ---
    # Skip the opening --- line
    rest = text[3:]
    if rest.startswith("\n"):
        rest = rest[1:]
    elif rest.startswith("\r\n"):
        rest = rest[2:]

    # Find closing ---
    end_marker = "\n---"
    idx = rest.find(end_marker)
    if idx == -1:
        # No closing delimiter - treat as no frontmatter
        return {}, text

    yaml_block = rest[:idx]
    body = rest[idx + len(end_marker):]
    # Strip the newline immediately after the closing ---
    if body.startswith("\n"):
        body = body[1:]
    elif body.startswith("\r\n"):
        body = body[2:]

    data = yaml.load(yaml_block, Loader=_StringPreservingTimestampLoader) or {}
    return data, body


def write_frontmatter(path: Path, data: Dict[str, Any], body: str) -> None:
    """Serialize data as YAML frontmatter and write atomically.

    Args:
        path: Destination file path.
        data: Dict to serialize as YAML frontmatter.
        body: Body text to append after the closing --- delimiter.
    """
    yaml_block = yaml.dump(
        data,
        default_flow_style=False,
        allow_unicode=True,
        sort_keys=False,
    )
    content = f"---\n{yaml_block}---\n{body}"
    atomic_write(Path(path), content)


def atomic_write(path: Path, content: str) -> None:
    """Write content to path atomically using filelock + tempfile + os.replace.

    Safe for concurrent processes: the lock prevents interleaved writes,
    and os.replace provides atomic rename on POSIX.

    Args:
        path: Destination file path.
        content: String content to write (UTF-8 encoded).
    """
    path = Path(path)
    lock_path = str(path) + ".lock"

    with filelock.FileLock(lock_path, timeout=10):
        tmp_path = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w",
                dir=path.parent,
                prefix=f".{path.name}.",
                suffix=".tmp",
                delete=False,
                encoding="utf-8",
            ) as tmp:
                tmp.write(content)
                tmp_path = Path(tmp.name)
            os.replace(tmp_path, path)
            tmp_path = None  # successfully replaced, don't delete
        finally:
            if tmp_path is not None and tmp_path.exists():
                tmp_path.unlink(missing_ok=True)
