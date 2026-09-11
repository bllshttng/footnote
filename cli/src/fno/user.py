"""What to call the human. One resolver, so no site spells the fallback twice.

fno.config imports UserBlock from this module, so a top-level
``from fno.config import ...`` here would close an import cycle: import
config lazily inside a function, never at module top.
"""

from __future__ import annotations

import subprocess
from functools import lru_cache

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


@lru_cache(maxsize=1)
def display_name() -> str:
    """The configured name, else the git identity, else "you"."""
    from fno.config import load_settings

    configured = (load_settings().user.name or "").strip()
    if configured:
        return configured
    try:
        out = subprocess.run(
            ["git", "config", "user.name"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return "you"
    return out.stdout.strip() or "you"
