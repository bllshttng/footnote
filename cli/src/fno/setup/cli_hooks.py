"""Install footnote's SessionStart hook into Codex / Gemini user config.

The Claude Code plugin wires SessionStart hooks via its plugin manifest, but
Codex and Gemini read hooks from user-level config that footnote cannot ship as
a repo file:

  * Gemini: ``~/.gemini/settings.json`` -> ``hooks.SessionStart`` (JSON).
  * Codex:  ``~/.codex/config.toml`` -> ``[[hooks.SessionStart]]`` (TOML).

Both point at the SAME entry script, ``<plugin_root>/hooks/session-start.sh``,
which detects the platform and emits the unified
``hookSpecificOutput.additionalContext`` contract all three CLIs now share.

Every writer here is idempotent (re-running is a no-op once footnote's hook is
present), backs the file up before writing, and never clobbers a user's other
hooks/settings (it merges, preserving everything else).

Codex caveat: Codex treats an unmanaged hook as *untrusted* until the user
explicitly approves it, so writing the config is necessary but not sufficient -
the user must trust the hook in Codex before it runs. ``install_codex_hook``
returns ``needs_trust=True`` so the caller can surface that instruction.
"""
from __future__ import annotations

import json
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

# Marker used to recognize footnote's own hook on a re-run (idempotency) without
# colliding with a user's hooks. The command path always ends with this.
_HOOK_SUFFIX = "hooks/session-start.sh"
_GEMINI_HOOK_NAME = "fno-session-start"


@dataclass
class HookInstallResult:
    cli: str  # "gemini" | "codex"
    path: Path
    changed: bool  # True if the file was written this call
    already_present: bool  # True if footnote's hook was already wired
    backup: Optional[Path] = None
    needs_trust: bool = False  # Codex: user must approve the hook before it runs
    note: Optional[str] = None


def _backup(path: Path) -> Optional[Path]:
    """Copy ``path`` to ``<path>.fno-bak`` before mutating. Returns the backup
    path, or None if there was nothing to back up."""
    if not path.exists():
        return None
    bak = path.with_name(path.name + ".fno-bak")
    shutil.copy2(path, bak)
    return bak


def _atomic_write(path: Path, text: str) -> None:
    """Write ``text`` to ``path`` atomically (temp + replace), creating parents."""
    import os
    import tempfile

    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
        os.replace(tmp, path)
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


# ---------------------------------------------------------------------------
# Gemini (~/.gemini/settings.json)
# ---------------------------------------------------------------------------


def install_gemini_hook(
    command: str, *, settings_path: Path
) -> HookInstallResult:
    """Merge footnote's SessionStart hook into a Gemini settings.json.

    Adds a ``hooks.SessionStart`` group whose command is ``command`` and whose
    ``name`` is the footnote marker. Idempotent (keyed on the name); preserves
    any other hooks and settings.
    """
    data: dict[str, Any] = {}
    if settings_path.exists():
        try:
            loaded = json.loads(settings_path.read_text(encoding="utf-8"))
            if isinstance(loaded, dict):
                data = loaded
        except json.JSONDecodeError as exc:
            return HookInstallResult(
                cli="gemini",
                path=settings_path,
                changed=False,
                already_present=False,
                note=f"settings.json is malformed ({exc}); left unchanged",
            )

    hooks = data.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        return HookInstallResult(
            cli="gemini", path=settings_path, changed=False, already_present=False,
            note="`hooks` is not an object; left unchanged",
        )
    session_start = hooks.setdefault("SessionStart", [])
    if not isinstance(session_start, list):
        return HookInstallResult(
            cli="gemini", path=settings_path, changed=False, already_present=False,
            note="`hooks.SessionStart` is not an array; left unchanged",
        )

    # Idempotency: footnote's hook already present (by name or command suffix)?
    for group in session_start:
        if not isinstance(group, dict):
            continue
        for h in group.get("hooks", []) or []:
            if not isinstance(h, dict):
                continue
            if h.get("name") == _GEMINI_HOOK_NAME or str(
                h.get("command", "")
            ).endswith(_HOOK_SUFFIX):
                return HookInstallResult(
                    cli="gemini", path=settings_path, changed=False,
                    already_present=True,
                )

    session_start.append(
        {
            "matcher": "startup",
            "hooks": [
                {
                    "name": _GEMINI_HOOK_NAME,
                    "type": "command",
                    "command": command,
                    "timeout": 10000,
                }
            ],
        }
    )

    backup = _backup(settings_path)
    _atomic_write(settings_path, json.dumps(data, indent=2) + "\n")
    return HookInstallResult(
        cli="gemini", path=settings_path, changed=True, already_present=False,
        backup=backup,
    )


# ---------------------------------------------------------------------------
# Codex (~/.codex/config.toml)
# ---------------------------------------------------------------------------

# Appended verbatim. Codex parses HookEventsToml under a `[hooks]` table; the
# SessionStart matcher is omitted so the hook fires on every session start
# (matching Codex's own SessionStart example, which carries no matcher).
_CODEX_BLOCK_TEMPLATE = """\
# Added by `fno setup cli-hooks` - footnote SessionStart context injection.
# Codex treats this as an UNMANAGED hook: approve/trust it in Codex before it
# runs. Remove this block to uninstall.
[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = {command}
"""


def _codex_hook_present(text: str) -> bool:
    """True if footnote's session-start hook is already wired in the TOML.

    Parses with tomllib when available; falls back to a substring check on the
    command suffix so a malformed-but-present config still reads as present.
    """
    try:
        import tomllib

        parsed = tomllib.loads(text)
        groups = (parsed.get("hooks", {}) or {}).get("SessionStart", []) or []
        for group in groups:
            for h in (group.get("hooks", []) or []):
                if str(h.get("command", "")).endswith(_HOOK_SUFFIX):
                    return True
        return False
    except Exception:
        return _HOOK_SUFFIX in text


def install_codex_hook(command: str, *, config_path: Path) -> HookInstallResult:
    """Append footnote's SessionStart hook to a Codex config.toml.

    Appends a ``[[hooks.SessionStart]]`` block (preserving every existing line +
    comment) rather than reserializing, so a user's TOML and comments survive.
    Idempotent: a re-run with footnote's hook already present is a no-op.
    Always reports ``needs_trust=True`` because Codex will not run an unmanaged
    hook until the user approves it.
    """
    existing = ""
    if config_path.exists():
        existing = config_path.read_text(encoding="utf-8")
        if _codex_hook_present(existing):
            return HookInstallResult(
                cli="codex", path=config_path, changed=False, already_present=True,
                needs_trust=True,
            )

    # TOML strings must be quoted; basic-string escape the command path.
    quoted = '"' + command.replace("\\", "\\\\").replace('"', '\\"') + '"'
    block = _CODEX_BLOCK_TEMPLATE.format(command=quoted)
    sep = "" if existing == "" or existing.endswith("\n\n") else (
        "\n" if existing.endswith("\n") else "\n\n"
    )
    new_text = existing + sep + block

    backup = _backup(config_path)
    _atomic_write(config_path, new_text)
    return HookInstallResult(
        cli="codex", path=config_path, changed=True, already_present=False,
        backup=backup, needs_trust=True,
    )
