"""fno setup migrate-paths

Idempotent migration. Detects existing install by probing the schema's
path-key enumeration; writes a settings.yaml that reproduces it; sets
obsidian.enabled if internal/ symlink resolves to a vault.

Auto-fires on first fno invocation after upgrade when sentinel missing.

Design decisions:
- Iterates PathsBlock.model_fields (Pydantic v2 API), not a hand-rolled list.
  Adding a new field in Phase 01 automatically gets a detection probe here.
- Sentinel + settings.yaml written under one FileLock (joint invariant).
  Per memory feedback_joint_invariant_atomic_write: both fields encode
  one invariant and must be written in one lock acquisition.
- FileLock uses a separate .lock suffix to avoid deadlock.
  Per memory feedback_lock_suffix_collision_deadlock.
- Backup of pre-existing hand-edited settings.yaml before overwrite.
"""
from __future__ import annotations

import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import yaml

from fno.config import PathsBlock, SettingsModel, ConfigBlock, ObsidianBlock


def detect_install(cwd: Optional[Path] = None) -> dict[str, Any]:
    """Probe filesystem for existing-install signals.

    Returns a dict with subset of keys:
      - "obsidian": {"enabled": True, "vault": str} if internal/ symlink found
      - One key per PathsBlock field name if a non-default value is detected

    Iterates PathsBlock.model_fields (Pydantic v2 API), not a hand-rolled list.
    Decision 12: adding a new key in Phase 01 adds it to migration automatically.
    """
    detected: dict[str, Any] = {}
    probe_dir = cwd if cwd is not None else Path.cwd()

    # Probe each PathsBlock field for legacy or non-default locations.
    # Currently all defaults derive from state_dir, so we only need to check
    # whether the canonical default exists. Future phases can add per-field
    # heuristics here without changing the function signature.
    for field_name in PathsBlock.model_fields:
        # Placeholder: no legacy locations yet - detection for individual fields
        # will be added in Phase 04 when the paths.sh migration arrives.
        pass

    # Obsidian detection: check internal/ symlink in cwd
    internal = probe_dir / "internal"
    if internal.is_symlink():
        try:
            target = internal.resolve()
            # Heuristic: the vault is the parent of the linked directory,
            # which should contain a .obsidian/ directory.
            vault_candidate = target.parent
            if (vault_candidate / ".obsidian").is_dir():
                detected["obsidian"] = {"enabled": True, "vault": str(vault_candidate)}
            # Also check: the target itself might be the vault root
            elif (target / ".obsidian").is_dir():
                detected["obsidian"] = {"enabled": True, "vault": str(target)}
        except (OSError, RuntimeError):
            pass  # broken symlink or loop - skip obsidian detection

    return detected


def _deep_merge(base: dict[str, Any], overlay: dict[str, Any]) -> dict[str, Any]:
    """Recursively merge overlay into base, returning a new dict.

    For scalar/list values, overlay wins. For nested dicts, recurse so
    that keys present only in base are preserved.
    """
    result = dict(base)
    for key, overlay_val in overlay.items():
        base_val = result.get(key)
        if isinstance(base_val, dict) and isinstance(overlay_val, dict):
            result[key] = _deep_merge(base_val, overlay_val)
        else:
            result[key] = overlay_val
    return result


def _build_detected_dict(detected: dict[str, Any]) -> dict[str, Any]:
    """Build only the keys that the migration wants to write, based on detected."""
    config_kwargs: dict[str, Any] = {}

    obsidian_data = detected.get("obsidian")
    if obsidian_data and obsidian_data.get("enabled"):
        config_kwargs["obsidian"] = ObsidianBlock(
            enabled=True,
            vault=obsidian_data.get("vault"),
        )

    config = ConfigBlock(**config_kwargs)
    model = SettingsModel(schema_version=1, config=config)

    # Return only the schema keys this migration manages; callers will
    # deep-merge this on top of any existing settings.yaml so extra keys
    # (config.providers, do_target, etc.) are preserved.
    return {
        "schema_version": model.schema_version,
        "config": {
            "state_dir": model.config.state_dir,
            "plans_dir": model.config.plans_dir,
            "paths": {k: v for k, v in model.config.paths.model_dump().items()},
            "obsidian": model.config.obsidian.model_dump(),
            "project": model.config.project.model_dump(),
        },
    }


def _render_settings_yaml(detected: dict[str, Any], existing_path: Optional[Path] = None) -> str:
    """Build migration YAML, deep-merging detected values on top of any existing content.

    If ``existing_path`` exists, its current YAML is loaded first as the base
    so that keys outside the schema (config.providers, do_target, etc.) survive
    the migration intact. The detected/schema dict wins only for the keys it
    sets (overlay semantics for dicts).
    """
    detected_dict = _build_detected_dict(detected)

    if existing_path is not None and existing_path.is_file():
        try:
            raw_existing = yaml.safe_load(existing_path.read_text(encoding="utf-8"))
        except (OSError, yaml.YAMLError):
            raw_existing = None
        if isinstance(raw_existing, dict):
            merged = _deep_merge(raw_existing, detected_dict)
            return yaml.safe_dump(merged, sort_keys=True, allow_unicode=True)

    return yaml.safe_dump(detected_dict, sort_keys=True, allow_unicode=True)


def write_settings_yaml(
    detected: dict[str, Any],
    settings_path: Path,
    existing_path: Optional[Path] = None,
) -> None:
    """Build SettingsModel from detected dict, write atomically via filelock.

    Caller must hold the outer FileLock before calling this. The inner
    atomic_write from state/io.py creates its own lock on a different
    suffix (.lock); this is safe because the outer lock is on <path>.lock
    and atomic_write acquires <path>.lock internally - but since we hold it
    exclusively from outside, the inner lock in atomic_write will re-enter.

    Actually: we call atomic_write which uses its own FileLock internally.
    To avoid the deadlock (outer lock on X.lock + inner lock on X.lock),
    we do NOT use atomic_write here. Instead we use the tempfile+replace
    pattern directly, since the outer FileLock from run_migration already
    serializes. This matches feedback_lock_suffix_collision_deadlock.
    """
    import tempfile

    # If the caller provides an explicit existing_path (e.g. the backup file after
    # rename), use that as the source of existing keys. Otherwise fall back to
    # settings_path itself (force=True path, where the file is still in place).
    _existing = existing_path if existing_path is not None else settings_path
    content = _render_settings_yaml(detected, existing_path=_existing)
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    # Write directly (our caller holds the outer FileLock on settings_path.lock)
    tmp_path = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            dir=settings_path.parent,
            prefix=f".{settings_path.name}.",
            suffix=".tmp",
            delete=False,
            encoding="utf-8",
        ) as tmp:
            tmp.write(content)
            tmp_path = Path(tmp.name)
        os.replace(tmp_path, settings_path)
        tmp_path = None
    finally:
        if tmp_path is not None and tmp_path.exists():
            tmp_path.unlink(missing_ok=True)


def print_summary(detected: dict[str, Any], settings_path: Path) -> None:
    """Print a stable one-screen migration summary (5 lines max) to stderr.

    Writing to stderr (not stdout) keeps stdout clean for commands consumed via
    command substitution (e.g. ``source "$(fno paths shell-stub)"``).
    Migration is a side-effect; its output belongs on stderr.
    """
    obsidian = detected.get("obsidian", {})
    vault_info = (
        f"vault at {obsidian['vault']}" if obsidian.get("enabled") else "fresh install"
    )
    print("[setup] path migration complete", file=sys.stderr)
    print(f"  detected install: {vault_info}", file=sys.stderr)
    print(f"  settings written to: {settings_path}", file=sys.stderr)
    print(f"  obsidian enabled: {obsidian.get('enabled', False)}", file=sys.stderr)
    print(f"  sentinel: {settings_path.parent / '.path-migration-done'}", file=sys.stderr)


def run_migration(
    force: bool = False,
    settings_root: Optional[Path] = None,
    cwd: Optional[Path] = None,
) -> int:
    """Main entry point. Returns exit code.

    Args:
        force: Re-run even if sentinel exists.
        settings_root: Override ~/.fno/ (for testing).
        cwd: Override cwd for internal/ symlink probing (for testing).

    Returns:
        0 on success or no-op, non-zero on error.
    """
    from filelock import FileLock, Timeout

    if settings_root is None:
        settings_root = Path.home() / ".fno"

    settings_path = settings_root / "settings.yaml"
    sentinel = settings_root / ".path-migration-done"

    # Fast-path: sentinel exists and not forced
    if sentinel.exists() and not force:
        return 0

    # Ensure settings_root exists before trying to create the lock file.
    # Do this outside the lock so the error is clear if mkdir fails.
    try:
        settings_root.mkdir(parents=True, exist_ok=True)
    except PermissionError as exc:
        print(
            f"[setup] migration failed: cannot create {settings_root}: {exc}",
            file=sys.stderr,
        )
        return 1
    except OSError as exc:
        print(
            f"[setup] migration failed: {exc}",
            file=sys.stderr,
        )
        return 1

    lock_path = str(settings_path) + ".lock"

    try:
        with FileLock(lock_path, timeout=30):
            # Double-check after acquiring the lock (second reader in race)
            if sentinel.exists() and not force:
                return 0

            # Clean up stale .tmp from crashed prior migration.
            # NamedTemporaryFile uses prefix=".settings.yaml." suffix=".tmp",
            # producing .settings.yaml.<random>.tmp - not the old "settings.yaml.tmp".
            # Glob both patterns to handle either form left by a crash.
            # (must be done INSIDE the lock per feedback_pure_read_vs_lazy_clear_writes)
            for stale_tmp in settings_root.glob(f".{settings_path.name}.*.tmp"):
                stale_tmp.unlink(missing_ok=True)
            # Legacy cleanup for the old hardcoded name (defensive)
            legacy_tmp = settings_root / f"{settings_path.name}.tmp"
            if legacy_tmp.exists():
                legacy_tmp.unlink(missing_ok=True)

            # Backup existing hand-edited settings.yaml (no sentinel = hand-edited).
            # IMPORTANT: read existing keys FROM the backup path after rename so that
            # write_settings_yaml can deep-merge them into the new file.
            # (Finding C: backup happens BEFORE write, so settings_path no longer exists
            # when _render_settings_yaml tries to open it. Pass backup_path explicitly.)
            existing_content_path: Optional[Path] = None
            if settings_path.exists() and not force:
                ts = datetime.now(tz=timezone.utc).strftime("%Y%m%d-%H%M%S")
                backup_path = settings_path.parent / f"settings.yaml.bak.{ts}"
                settings_path.rename(backup_path)
                existing_content_path = backup_path
                print(
                    f"[setup] backed up prior settings.yaml to {backup_path}",
                    file=sys.stderr,
                )

            # Detect install
            detected = detect_install(cwd=cwd)

            # Write settings.yaml (inside the lock, no inner atomic_write lock)
            write_settings_yaml(detected, settings_path, existing_path=existing_content_path)

            # Touch sentinel INSIDE the same lock (joint invariant with settings_path)
            # Per feedback_joint_invariant_atomic_write: both must be set atomically.
            sentinel.touch()

    except Timeout:
        print(
            "[setup] migration timed out waiting for lock; another process may be running.",
            file=sys.stderr,
        )
        return 1
    except PermissionError as exc:
        print(f"[setup] migration failed: permission denied: {exc}", file=sys.stderr)
        return 1
    except OSError as exc:
        print(f"[setup] migration failed: {exc}", file=sys.stderr)
        return 1

    print_summary(detected, settings_path)
    return 0
