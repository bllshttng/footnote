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
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

# Marker used to recognize footnote's own hook on a re-run (idempotency) without
# colliding with a user's hooks. The command path always ends with this.
_HOOK_SUFFIX = "hooks/session-start.sh"
_GEMINI_HOOK_NAME = "fno-session-start"


def _wrapped_command(command: str, cli: str) -> str:
    """Prefix the hook command with an explicit ``FNO_PLATFORM`` so the wrapper
    detects the right platform. Codex/Gemini do NOT set their plugin-root env
    var when running a user-config hook, so without this the wrapper falls
    through to ``generic`` and emits the wrong output shape (PR #11 review)."""
    return f"env FNO_PLATFORM={cli} {command}"


@dataclass
class HookInstallResult:
    cli: str  # "gemini" | "codex"
    path: Path
    changed: bool  # True if either config file was changed this call
    already_present: bool  # True if footnote's hook was already wired
    backup: Optional[Path] = None
    needs_trust: bool = False  # Codex: user must approve the hook before it runs
    note: Optional[str] = None
    legacy_backup: Optional[Path] = None
    error: Optional[str] = None


@dataclass(frozen=True)
class CodexHookDiagnostics:
    """Read-only view of Codex's current user-level hook layers."""

    config_path: Path
    hooks_json_path: Path
    state: str  # neither | toml-only | json-only | both | malformed
    toml_commands: tuple[str, ...] = ()
    json_commands: tuple[str, ...] = ()
    toml_footnote_commands: tuple[str, ...] = ()
    json_footnote_commands: tuple[str, ...] = ()
    toml_foreign_commands: tuple[str, ...] = ()
    json_foreign_commands: tuple[str, ...] = ()
    toml_footnote_state_keys: tuple[str, ...] = ()
    toml_footnote_state_recorded: tuple[bool, ...] = ()
    errors: tuple[str, ...] = ()

    @property
    def has_toml_hooks(self) -> bool:
        return bool(self.toml_commands)

    @property
    def has_json_hooks(self) -> bool:
        return bool(self.json_commands)

    @property
    def all_toml_footnote_hooks_verified(self) -> bool:
        # A stored trusted_hash is only an approval record. Until this module
        # recomputes and compares Codex's exact local hash contract, it cannot
        # truthfully claim the command is verified.
        return False


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
    # Preserve the original file's permissions. mkstemp creates 0o600, which
    # would otherwise tighten an existing 0o644 user config (PR #11 review).
    try:
        prev_mode: Optional[int] = path.stat().st_mode
    except OSError:
        prev_mode = None
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
        if prev_mode is not None:
            os.chmod(tmp, prev_mode)
        os.replace(tmp, path)
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def _is_footnote_codex_command(command: str) -> bool:
    """Classify only footnote-owned Codex SessionStart commands."""
    if _HOOK_SUFFIX not in command:
        return False
    return (
        "FNO_PLATFORM=codex" in command
        or "/footnote/hooks/session-start.sh" in command
    )


def _all_hook_commands(data: Any, *, source: str) -> tuple[str, ...]:
    """Extract commands from every hook event array, excluding trust state."""
    if not isinstance(data, dict):
        raise ValueError(f"{source} root is not an object")
    hooks = data.get("hooks", {}) or {}
    if not isinstance(hooks, dict):
        raise ValueError(f"{source} `hooks` is not an object")

    commands: list[str] = []
    for event, groups in hooks.items():
        if event == "state":
            continue
        if groups is None:
            continue
        if not isinstance(groups, list):
            raise ValueError(f"{source} `hooks.{event}` is not an array")
        for group in groups:
            if not isinstance(group, dict):
                raise ValueError(f"{source} {event} group is not an object")
            entries = group.get("hooks", []) or []
            if not isinstance(entries, list):
                raise ValueError(f"{source} {event} `hooks` is not an array")
            for hook in entries:
                if not isinstance(hook, dict):
                    raise ValueError(f"{source} {event} hook is not an object")
                command = hook.get("command")
                if command is not None and not isinstance(command, str):
                    raise ValueError(f"{source} {event} command is not a string")
                if command:
                    commands.append(command)
    return tuple(commands)


def _session_start_commands(data: Any, *, source: str) -> tuple[str, ...]:
    """Extract commands from the shared TOML/JSON SessionStart shape."""
    return tuple(
        command
        for _group_index, _hook_index, command in _session_start_hook_records(
            data, source=source
        )
    )


def _session_start_hook_records(
    data: Any, *, source: str
) -> tuple[tuple[int, int, str], ...]:
    """Extract ``(group index, hook index, command)`` SessionStart records."""
    if not isinstance(data, dict):
        raise ValueError(f"{source} root is not an object")
    hooks = data.get("hooks", {})
    if hooks is None:
        hooks = {}
    if not isinstance(hooks, dict):
        raise ValueError(f"{source} `hooks` is not an object")
    groups = hooks.get("SessionStart", [])
    if groups is None:
        groups = []
    if not isinstance(groups, list):
        raise ValueError(f"{source} `hooks.SessionStart` is not an array")

    records: list[tuple[int, int, str]] = []
    for group_index, group in enumerate(groups):
        if not isinstance(group, dict):
            raise ValueError(f"{source} SessionStart group is not an object")
        entries = group.get("hooks", [])
        if entries is None:
            entries = []
        if not isinstance(entries, list):
            raise ValueError(f"{source} SessionStart `hooks` is not an array")
        for hook_index, hook in enumerate(entries):
            if not isinstance(hook, dict):
                raise ValueError(f"{source} SessionStart hook is not an object")
            command = hook.get("command")
            if command is not None and not isinstance(command, str):
                raise ValueError(f"{source} SessionStart command is not a string")
            if command:
                records.append((group_index, hook_index, command))
    return tuple(records)


def _codex_hook_trust(
    data: dict[str, Any],
    *,
    config_path: Path,
    records: tuple[tuple[int, int, str], ...],
) -> tuple[tuple[str, ...], tuple[bool, ...]]:
    """Return Codex state keys and trust-entry presence for footnote hooks."""
    hooks = data.get("hooks", {}) or {}
    state = hooks.get("state", {}) or {}
    if not isinstance(state, dict):
        raise ValueError("config.toml `hooks.state` is not an object")

    path = config_path.expanduser().absolute()
    keys = tuple(
        f"{path}:session_start:{group_index}:{hook_index}"
        for group_index, hook_index, command in records
        if _is_footnote_codex_command(command)
    )
    return keys, tuple(key in state for key in keys)


def inspect_codex_hooks(
    *, config_path: Path, hooks_json_path: Path
) -> CodexHookDiagnostics:
    """Inspect Codex TOML and legacy JSON hook layers without mutating either.

    Syntax, schema, and read failures are returned in ``errors`` rather than
    escaping. A malformed layer makes the aggregate state ``malformed`` while
    commands from a healthy layer remain available to the caller.
    """
    toml_commands: tuple[str, ...] = ()
    json_commands: tuple[str, ...] = ()
    toml_session_commands: tuple[str, ...] = ()
    json_session_commands: tuple[str, ...] = ()
    toml_footnote_state_keys: tuple[str, ...] = ()
    toml_footnote_state_recorded: tuple[bool, ...] = ()
    errors: list[str] = []

    if config_path.exists():
        try:
            parsed_toml = tomllib.loads(config_path.read_text(encoding="utf-8"))
            toml_records = _session_start_hook_records(parsed_toml, source="config.toml")
            toml_session_commands = tuple(command for _, _, command in toml_records)
            toml_commands = _all_hook_commands(parsed_toml, source="config.toml")
            toml_footnote_state_keys, toml_footnote_state_recorded = _codex_hook_trust(
                parsed_toml,
                config_path=config_path,
                records=toml_records,
            )
        except (OSError, UnicodeError, tomllib.TOMLDecodeError, ValueError) as exc:
            errors.append(f"{config_path}: {exc}")

    if hooks_json_path.exists():
        try:
            parsed_json = json.loads(hooks_json_path.read_text(encoding="utf-8"))
            json_session_commands = _session_start_commands(
                parsed_json, source="hooks.json"
            )
            json_commands = _all_hook_commands(parsed_json, source="hooks.json")
        except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
            errors.append(f"{hooks_json_path}: {exc}")

    if errors:
        state = "malformed"
    elif toml_commands and json_commands:
        state = "both"
    elif toml_commands:
        state = "toml-only"
    elif json_commands:
        state = "json-only"
    else:
        state = "neither"

    toml_footnote = tuple(
        c for c in toml_session_commands if _is_footnote_codex_command(c)
    )
    json_footnote = tuple(
        c for c in json_session_commands if _is_footnote_codex_command(c)
    )
    return CodexHookDiagnostics(
        config_path=config_path,
        hooks_json_path=hooks_json_path,
        state=state,
        toml_commands=toml_commands,
        json_commands=json_commands,
        toml_footnote_commands=toml_footnote,
        json_footnote_commands=json_footnote,
        toml_foreign_commands=tuple(c for c in toml_commands if c not in toml_footnote),
        json_foreign_commands=tuple(c for c in json_commands if c not in json_footnote),
        toml_footnote_state_keys=toml_footnote_state_keys,
        toml_footnote_state_recorded=toml_footnote_state_recorded,
        errors=tuple(errors),
    )


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

    # No matcher: Gemini lifecycle matchers are exact strings, so "startup"
    # would miss the `resume` and `clear` SessionStart sources. Omitting the
    # matcher fires on all of them (parity with Claude's empty matcher and
    # Codex's matcher-less group; PR #11 review).
    session_start.append(
        {
            "hooks": [
                {
                    "name": _GEMINI_HOOK_NAME,
                    "type": "command",
                    "command": _wrapped_command(command, "gemini"),
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
        parsed = tomllib.loads(text)
        return any(
            _is_footnote_codex_command(command)
            for command in _session_start_commands(parsed, source="config.toml")
        )
    except Exception:
        return _HOOK_SUFFIX in text or "FNO_PLATFORM=codex" in text


def _migrate_legacy_codex_hooks(
    hooks_json_path: Path,
) -> tuple[Optional[Path], int, bool]:
    """Remove footnote-owned JSON SessionStart hooks after backing up.

    Returns ``(backup, removed_count, foreign_session_start_remains)``. The
    caller invokes this only after the inspector has established valid JSON.
    """
    data = json.loads(hooks_json_path.read_text(encoding="utf-8"))
    hooks = data.get("hooks", {})
    groups = hooks.get("SessionStart", [])
    kept_groups: list[Any] = []
    removed = 0

    for group in groups:
        entries = group.get("hooks", [])
        kept_entries = []
        for hook in entries:
            command = hook.get("command")
            if isinstance(command, str) and _is_footnote_codex_command(command):
                removed += 1
            else:
                kept_entries.append(hook)
        if kept_entries:
            kept_group = dict(group)
            kept_group["hooks"] = kept_entries
            kept_groups.append(kept_group)

    if not removed:
        foreign_remains = bool(_all_hook_commands(data, source="hooks.json"))
        return None, 0, foreign_remains

    migrated = dict(data)
    migrated_hooks = dict(hooks)
    if kept_groups:
        migrated_hooks["SessionStart"] = kept_groups
    else:
        migrated_hooks.pop("SessionStart", None)
    migrated["hooks"] = migrated_hooks

    backup = _backup(hooks_json_path)
    wholly_footnote_owned = (
        not migrated_hooks
        and set(migrated).issubset({"description", "hooks"})
    )
    if wholly_footnote_owned:
        hooks_json_path.unlink()
    else:
        _atomic_write(hooks_json_path, json.dumps(migrated, indent=2) + "\n")
    return backup, removed, bool(
        _all_hook_commands(migrated, source="hooks.json")
    )


def _codex_diagnostic_note(
    diagnostics: CodexHookDiagnostics,
    *, migrated: bool = False,
    foreign_remains: bool = False,
) -> Optional[str]:
    if diagnostics.errors:
        return "malformed Codex hook config; left malformed input unchanged: " + "; ".join(
            diagnostics.errors
        )
    if foreign_remains:
        return (
            f"legacy hooks remain in {diagnostics.hooks_json_path}; foreign SessionStart "
            f"hooks were preserved and need manual consolidation into "
            f"{diagnostics.config_path} (TOML is preferred)"
        )
    if diagnostics.state == "both":
        action = "footnote-owned legacy JSON hooks were migrated; " if migrated else ""
        if diagnostics.json_foreign_commands:
            return (
                f"{action}both Codex hook layers contain hooks: "
                f"{diagnostics.config_path} and {diagnostics.hooks_json_path}; TOML is "
                "preferred. Legacy JSON contains foreign hooks and needs manual "
                "consolidation; it was left unchanged"
            )
        return (
            f"{action}both Codex hook layers contain SessionStart hooks: "
            f"{diagnostics.config_path} and {diagnostics.hooks_json_path}; TOML is "
            "preferred. Re-run with --migrate-legacy-hooks-json to remove only "
            "footnote-owned legacy JSON hooks"
        )
    if migrated:
        return f"migrated footnote-owned legacy SessionStart hooks from {diagnostics.hooks_json_path}"
    return None


def install_codex_hook(
    command: str,
    *,
    config_path: Path,
    hooks_json_path: Optional[Path] = None,
    migrate_legacy_hooks_json: bool = False,
) -> HookInstallResult:
    """Append footnote's SessionStart hook to a Codex config.toml.

    Appends a ``[[hooks.SessionStart]]`` block (preserving every existing line +
    comment) rather than reserializing, so a user's TOML and comments survive.
    Idempotent: a re-run with footnote's hook already present is a no-op.
    Always reports ``needs_trust=True`` because Codex will not run an unmanaged
    hook until the user approves it.
    """
    legacy_path = hooks_json_path or config_path.with_name("hooks.json")
    before = inspect_codex_hooks(config_path=config_path, hooks_json_path=legacy_path)
    if before.errors:
        message = _codex_diagnostic_note(before)
        return HookInstallResult(
            cli="codex", path=config_path, changed=False, already_present=False,
            needs_trust=False, note=message, error=message,
        )

    existing = config_path.read_text(encoding="utf-8") if config_path.exists() else ""
    already_present = _codex_hook_present(existing)
    backup: Optional[Path] = None
    changed = False

    if not already_present:
        # TOML basic strings share JSON's escaping rules, so json.dumps produces a
        # valid quoted string (PR #11 review). Wrap with FNO_PLATFORM so the hook
        # detects the codex platform regardless of Codex's env.
        quoted = json.dumps(_wrapped_command(command, "codex"))
        block = _CODEX_BLOCK_TEMPLATE.format(command=quoted)
        sep = "" if existing == "" or existing.endswith("\n\n") else (
            "\n" if existing.endswith("\n") else "\n\n"
        )
        new_text = existing + sep + block

        backup = _backup(config_path)
        _atomic_write(config_path, new_text)
        changed = True

    legacy_backup: Optional[Path] = None
    migrated = False
    foreign_remains = False
    if migrate_legacy_hooks_json and legacy_path.exists():
        legacy_backup, removed, foreign_remains = _migrate_legacy_codex_hooks(legacy_path)
        migrated = removed > 0
        changed = changed or migrated

    after = inspect_codex_hooks(config_path=config_path, hooks_json_path=legacy_path)
    note = _codex_diagnostic_note(
        after,
        migrated=migrated,
        foreign_remains=foreign_remains,
    )

    return HookInstallResult(
        cli="codex", path=config_path, changed=changed,
        already_present=already_present, backup=backup, needs_trust=True,
        note=note, legacy_backup=legacy_backup,
    )
