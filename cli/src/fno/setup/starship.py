"""Install footnote's starship provenance module into the user's starship.toml.

Opt-in only (wired into ``fno setup wizard``, mirrors ``recommended_rules.py``).
fno-spawned panes export ``FNO_NODE``/``FNO_SLUG``/``FNO_PLAN`` (x-84a8); the
shipped ``starship-fno.toml`` snippet renders the node in the pane's prompt,
gated on ``FNO_NODE`` so it appears only inside an fno pane.

The installer APPENDS the snippet to ``~/.config/starship.toml`` and is
idempotent: a second run detects the ``[custom.fno_node]`` header already present
and skips. It never rewrites the user's existing config - only appends. On a
bare ``pip install fno`` with no snippet shipped it installs nothing (clean).
"""
from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

# The starship module header; its presence in the target marks "already installed"
# (idempotency) so a re-run does not append a duplicate module.
_MODULE_MARKER = "[custom.fno_node]"


def default_snippet_source() -> Optional[Path]:
    """Resolve the shipped starship snippet: package data colocated under the
    ``fno`` package (same pattern as the recommended-rules pack). ``None`` only
    if the package data is somehow absent."""
    try:
        from importlib.resources import files

        p = Path(str(files("fno") / "recommended_rules" / "starship-fno.toml"))
        return p if p.is_file() else None
    except (ImportError, ModuleNotFoundError, TypeError, ValueError, OSError):
        return None


def default_starship_config() -> Path:
    """The user's starship config path (honors ``STARSHIP_CONFIG``), defaulting
    to ``~/.config/starship.toml``. ``os.path.expanduser`` (not ``Path.home``)
    so an unset ``HOME`` degrades instead of raising (PR #146 gemini)."""
    override = os.environ.get("STARSHIP_CONFIG")
    if override:
        return Path(override)
    return Path(os.path.expanduser("~")) / ".config" / "starship.toml"


@dataclass
class StarshipResult:
    action: str  # appended | already | missing-snippet
    target: Path


def install_starship_module(
    snippet_path: Path, target_toml: Path
) -> StarshipResult:
    """Append the starship snippet to ``target_toml`` (idempotent).

    - Missing snippet -> ``missing-snippet`` (nothing written).
    - Marker already in the target -> ``already`` (nothing written).
    - Otherwise append the snippet (creating the file/parents if absent) and
      return ``appended``.
    """
    snippet_path = Path(snippet_path)
    target_toml = Path(target_toml)
    if not snippet_path.is_file():
        return StarshipResult("missing-snippet", target_toml)

    snippet = snippet_path.read_text()
    existing = target_toml.read_text() if target_toml.exists() else ""
    if _MODULE_MARKER in existing:
        return StarshipResult("already", target_toml)

    # A blank line separates our module from whatever precedes it, regardless of
    # how the user's file ends (no trailing newline / one / already blank).
    if not existing:
        sep = ""
    elif existing.endswith("\n\n"):
        sep = ""
    elif existing.endswith("\n"):
        sep = "\n"
    else:
        sep = "\n\n"

    target_toml.parent.mkdir(parents=True, exist_ok=True)
    with target_toml.open("a", encoding="utf-8") as fh:
        fh.write(sep + snippet)
    return StarshipResult("appended", target_toml)


def summarize(result: StarshipResult) -> str:
    verb = {
        "appended": f"appended the fno provenance module to {result.target}",
        "already": f"already present in {result.target} (left as-is)",
        "missing-snippet": "starship snippet not found (nothing installed)",
    }.get(result.action, result.action)
    return f"  starship: {verb}"
