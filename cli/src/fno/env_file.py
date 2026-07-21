"""Read a single named variable from a ``.env``-style file.

Shared by provider key resolution (`agents.model_routing`) and status-sink URL
resolution (`status_fanout`): both need to read a secret from ``~/.fno/.env``
when it is not in the process env, with the process env winning.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional


def read_var_from_env_file(path_str: str, key_name: str) -> Optional[str]:
    """Read ``key_name`` from a ``.env``-style file. Missing file / key is not
    fatal: returns None so the caller falls back.

    Tolerates an optional ``export`` prefix and whitespace around ``=``.
    RuntimeError is caught alongside OSError/ValueError because
    ``Path.expanduser()`` raises it when the home dir cannot be resolved."""
    try:
        text = Path(path_str).expanduser().read_text(encoding="utf-8")
    except (OSError, ValueError, RuntimeError):
        return None
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("#") or "=" not in line:
            continue
        if line.startswith("export "):
            line = line[len("export "):].strip()
        name, _, value = line.partition("=")
        if name.strip() == key_name:
            return value.strip().strip('"').strip("'") or None
    return None
