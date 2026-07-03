"""Install footnote's prompt-provenance renderers into the user's shell.

Opt-in only (wired into ``fno setup wizard``, mirrors ``recommended_rules.py``).
fno-spawned panes export ``FNO_NODE``/``FNO_SLUG``/``FNO_PLAN`` (x-84a8); those
env vars are prompt-engine-agnostic. This module ships two renderers so the
feature does not assume the user runs starship (it is OSS):

- ``starship-fno.toml`` - a starship custom module (for starship users).
- ``prompt-fno.sh`` - a portable bash/zsh ``$PS1`` segment sourced from the
  user's shell rc (no prompt engine required; the universal path).

Both installers APPEND idempotently (starship: the ``[custom.fno_node]`` header;
shell: the ``source`` line for our snippet) and never rewrite the user's config.
fno's own mux status row is the config-free surface and is a separate follow-up;
these two cover users who want provenance in their own prompt. On a bare ``pip
install fno`` with no snippet shipped, each installer installs nothing (clean).
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


# --- Portable shell-rc renderer (the starship-free path) -------------------

def default_shell_snippet_source() -> Optional[Path]:
    """Resolve the shipped portable ``prompt-fno.sh`` (colocated package data,
    same as the starship snippet). ``None`` if absent."""
    try:
        from importlib.resources import files

        p = Path(str(files("fno") / "recommended_rules" / "prompt-fno.sh"))
        return p if p.is_file() else None
    except (ImportError, ModuleNotFoundError, TypeError, ValueError, OSError):
        return None


def default_shell_rc() -> Path:
    """The user's shell rc, picked from ``$SHELL`` (zsh -> ``~/.zshrc``, else
    ``~/.bashrc``). ``os.path.expanduser`` so an unset ``HOME`` degrades."""
    home = Path(os.path.expanduser("~"))
    return home / (".zshrc" if "zsh" in os.environ.get("SHELL", "") else ".bashrc")


def install_shell_source_line(snippet_path: Path, rc_path: Path) -> StarshipResult:
    """Append ``source "<snippet_path>"`` to ``rc_path`` (idempotent).

    The snippet path itself is the idempotency marker, so a re-run (or an
    already-hand-added line) is detected and skipped. Only a single ``source``
    line is added - the user's rc is never rewritten. Missing snippet ->
    ``missing-snippet``."""
    snippet_path = Path(snippet_path)
    rc_path = Path(rc_path)
    if not snippet_path.is_file():
        return StarshipResult("missing-snippet", rc_path)

    line = f'source "{snippet_path}"'
    existing = rc_path.read_text() if rc_path.exists() else ""
    if str(snippet_path) in existing:
        return StarshipResult("already", rc_path)

    sep = "" if not existing else ("" if existing.endswith("\n") else "\n")
    rc_path.parent.mkdir(parents=True, exist_ok=True)
    with rc_path.open("a", encoding="utf-8") as fh:
        fh.write(f"{sep}# footnote pane provenance (portable prompt segment)\n{line}\n")
    return StarshipResult("appended", rc_path)


def summarize_shell(result: StarshipResult) -> str:
    verb = {
        "appended": f"added the provenance source line to {result.target}",
        "already": f"already sourced in {result.target} (left as-is)",
        "missing-snippet": "shell snippet not found (nothing installed)",
    }.get(result.action, result.action)
    return f"  shell prompt: {verb}"
