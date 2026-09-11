"""What to call the human. One resolver, so no site spells the fallback twice.

fno.config imports UserBlock from this module, so a top-level
``from fno.config import ...`` here would close an import cycle: import
config lazily inside a function, never at module top.
"""

from __future__ import annotations

import subprocess
from functools import lru_cache
from pathlib import Path

from pydantic import BaseModel, ConfigDict


class UserBlock(BaseModel):
    """How the machine addresses the human (nested under ``config.user``).

    ``name`` is a form of address, never an identity and never an authority.
    Empty is the unset state, and ``display_name`` falls back to
    ``git config user.name`` so a fresh install already says something better
    than a role noun.
    """

    model_config = ConfigDict(extra="ignore")

    name: str = ""


@lru_cache(maxsize=8)
def display_name_at(root: Path) -> str:
    """The configured name AT ``root``, else the git identity there, else "you".

    The path is an ARGUMENT, and it is the cache key. A zero-argument cached
    read would be a global keyed on nothing: the first caller in a process
    fixes the answer for every caller after it (x-3d21 R5), so the cache keys
    on the repo root like ``fleet_has_crown_at`` keys on its registry path.
    """
    from fno.config import load_settings_for_repo

    configured = (load_settings_for_repo(root).user.name or "").strip()
    if configured:
        return configured
    try:
        out = subprocess.run(
            ["git", "config", "user.name"],
            capture_output=True,
            text=True,
            timeout=5,
            cwd=root,
        )
    except (OSError, subprocess.SubprocessError):
        return "you"
    return out.stdout.strip() or "you"


def display_name() -> str:
    """``display_name_at`` at this session's repo root."""
    from fno.paths import resolve_repo_root

    return display_name_at(resolve_repo_root())
