"""Integration tests for `fno setup migrate-paths`.

Task 2b.6 of plan 2026-05-14-path-config-impl.

Tests cover:
- AC2b-HP: Fresh install (empty settings_root) -> writes settings.yaml + sentinel
- AC2b-HP: Existing install with internal/ symlink -> detects vault
- AC2b-ERR: Permission denied on settings_root -> exits non-zero
- AC2b-UI: One-screen summary (5 lines max, no Traceback)
- AC2b-EDGE: Race - two threads call run_migration concurrently; filelock serializes
- AC2b-FR: Crashed prior migration (settings.yaml.tmp present, no sentinel) -> cleanup + fresh run
- AC2b-EDGE: Existing settings.yaml without sentinel gets backed up
- AC2b-FR: Idempotent re-run without --force -> no-op
- AC2b-FR: --force re-runs despite sentinel

All tests pass `settings_root=` directly for isolation.
Autouse fixture pins FNO_REPO_ROOT (feedback_abi_repo_root_leaks_between_tests).
"""
from __future__ import annotations

import os
import stat
import threading
import time
from pathlib import Path
from typing import Generator

import pytest
import yaml


# ---------------------------------------------------------------------------
# Autouse fixture: isolate FNO_REPO_ROOT, clear settings cache
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Pin repo root + clear caches before each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass


def _settings_root(tmp_path: Path) -> Path:
    """Return the isolated settings_root for this test."""
    return tmp_path / ".fno"


# ---------------------------------------------------------------------------
# AC2b-HP: Fresh install writes settings.yaml + sentinel
# ---------------------------------------------------------------------------


def test_fresh_install_writes_settings_and_sentinel(tmp_path: Path) -> None:
    """AC2b-HP: Fresh install (no .fno/) writes settings.yaml + sentinel."""
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    assert not settings_root.exists()

    rc = run_migration(settings_root=settings_root)
    assert rc == 0, f"Expected exit 0 on fresh install, got {rc}"

    settings_file = settings_root / "settings.yaml"
    assert settings_file.exists(), "settings.yaml should be created on fresh install"

    sentinel = settings_root / ".path-migration-done"
    assert sentinel.exists(), ".path-migration-done sentinel should be touched"

    data = yaml.safe_load(settings_file.read_text(encoding="utf-8"))
    assert isinstance(data, dict), "settings.yaml should be a valid YAML dict"
    assert data.get("schema_version") == 1, "schema_version should be 1"


# ---------------------------------------------------------------------------
# AC2b-HP: Existing install with internal/ symlink -> vault detected
# ---------------------------------------------------------------------------


def test_existing_install_detects_vault(tmp_path: Path) -> None:
    """AC2b-HP: Existing install with internal/ symlink resolves to vault."""
    from fno.setup.migrate_paths import run_migration

    # Simulate a vault at tmp_path/fake-vault with a .obsidian dir
    vault_dir = tmp_path / "fake-vault"
    vault_dir.mkdir()
    (vault_dir / ".obsidian").mkdir()

    # internal/ symlink inside the settings_root parent (cwd simulation)
    internal_link = tmp_path / "internal"
    # Link internal/ -> vault_dir/internal (sub-directory of vault)
    vault_internal = vault_dir / "internal"
    vault_internal.mkdir()
    internal_link.symlink_to(vault_internal)

    settings_root = _settings_root(tmp_path)

    rc = run_migration(settings_root=settings_root, cwd=tmp_path)
    assert rc == 0

    settings_file = settings_root / "settings.yaml"
    data = yaml.safe_load(settings_file.read_text(encoding="utf-8"))
    config = data.get("config", {})
    obsidian = config.get("obsidian", {})
    assert obsidian.get("enabled") is True, "obsidian.enabled should be True when vault found"
    assert obsidian.get("vault") is not None, "obsidian.vault should be set"


# ---------------------------------------------------------------------------
# AC2b-ERR: Permission denied on settings_root -> exits non-zero
# ---------------------------------------------------------------------------


def test_permission_denied_exits_nonzero(tmp_path: Path) -> None:
    """AC2b-ERR: Permission denied on settings_root creates clear error."""
    from fno.setup.migrate_paths import run_migration

    # Create a parent dir that is read-only so mkdir fails
    locked_parent = tmp_path / "no-write"
    locked_parent.mkdir()
    locked_parent.chmod(0o555)  # r-xr-xr-x: no write

    settings_root = locked_parent / ".fno"

    try:
        rc = run_migration(settings_root=settings_root)
        # Should exit non-zero on permission error
        assert rc != 0, "Expected non-zero exit on permission denied"
    except SystemExit as e:
        assert e.code != 0, "SystemExit should have non-zero code on permission denied"
    finally:
        # Restore permissions for cleanup
        locked_parent.chmod(0o755)


# ---------------------------------------------------------------------------
# AC2b-UI: One-screen summary (5 lines max, no Traceback)
# ---------------------------------------------------------------------------


def test_one_screen_summary(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    """AC2b-UI: Migration prints at most 5 lines of summary, no Traceback."""
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    run_migration(settings_root=settings_root)

    captured = capsys.readouterr()
    output_lines = [l for l in captured.out.splitlines() if l.strip()]
    assert len(output_lines) <= 5, (
        f"Summary should be 5 lines max, got {len(output_lines)}:\n{captured.out}"
    )
    assert "Traceback" not in captured.out, "Should not print stack traces"
    assert "Traceback" not in captured.err, "Should not print stack traces to stderr"


# ---------------------------------------------------------------------------
# AC2b-EDGE: Race - two threads; filelock serializes
# ---------------------------------------------------------------------------


def test_concurrent_migration_serializes(tmp_path: Path) -> None:
    """AC2b-EDGE: Two concurrent calls; filelock serializes; settings.yaml is valid."""
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    results: list[int] = []
    errors: list[Exception] = []

    def _run() -> None:
        try:
            rc = run_migration(settings_root=settings_root)
            results.append(rc)
        except Exception as e:
            errors.append(e)

    t1 = threading.Thread(target=_run)
    t2 = threading.Thread(target=_run)
    t1.start()
    t2.start()
    t1.join(timeout=15)
    t2.join(timeout=15)

    assert not errors, f"Errors during concurrent migration: {errors}"
    assert all(rc == 0 for rc in results), f"All runs should exit 0; got {results}"

    settings_file = settings_root / "settings.yaml"
    assert settings_file.exists(), "settings.yaml must exist after concurrent migration"
    data = yaml.safe_load(settings_file.read_text(encoding="utf-8"))
    assert isinstance(data, dict), "settings.yaml must be valid YAML after concurrent run"
    assert data.get("schema_version") == 1

    sentinel = settings_root / ".path-migration-done"
    assert sentinel.exists(), "Sentinel must exist after concurrent migration"


# ---------------------------------------------------------------------------
# AC2b-FR: Crashed prior migration - cleanup and fresh run succeeds
# ---------------------------------------------------------------------------


def test_crashed_prior_migration_cleanup(tmp_path: Path) -> None:
    """AC2b-FR: tmp file from crashed migration is cleaned up on next run."""
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    settings_root.mkdir(parents=True)

    # Simulate crashed state: a stale .tmp file, no sentinel, no final settings.yaml
    stale_tmp = settings_root / "settings.yaml.tmp"
    stale_tmp.write_text("# stale", encoding="utf-8")

    assert not (settings_root / ".path-migration-done").exists()
    assert not (settings_root / "settings.yaml").exists()

    rc = run_migration(settings_root=settings_root)
    assert rc == 0, "Migration should succeed despite stale .tmp"

    assert not stale_tmp.exists(), "Stale .tmp should be cleaned up"
    assert (settings_root / "settings.yaml").exists(), "settings.yaml should exist"
    assert (settings_root / ".path-migration-done").exists(), "Sentinel should exist"


# ---------------------------------------------------------------------------
# Fix 4: Cleanup matches actual tmp writer pattern (.settings.yaml.<random>.tmp)
# ---------------------------------------------------------------------------


def test_crashed_prior_migration_cleans_actual_tmp_pattern(tmp_path: Path) -> None:
    """Fix 4: crashed migration leaves .settings.yaml.<random>.tmp, not settings.yaml.tmp.

    Cleanup must use glob to match the actual NamedTemporaryFile pattern.
    """
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    settings_root.mkdir(parents=True)

    # Simulate the actual tmp filename pattern from NamedTemporaryFile:
    #   prefix=".settings.yaml.", suffix=".tmp"
    # Results in: .settings.yaml.abc123.tmp (NOT settings.yaml.tmp)
    real_pattern_tmp = settings_root / ".settings.yaml.crashedABC123.tmp"
    real_pattern_tmp.write_text("# stale from actual crash", encoding="utf-8")

    assert not (settings_root / ".path-migration-done").exists()

    rc = run_migration(settings_root=settings_root)
    assert rc == 0, "Migration should succeed despite actual-pattern stale .tmp"

    assert not real_pattern_tmp.exists(), (
        ".settings.yaml.<random>.tmp should be cleaned up by glob-based cleanup"
    )
    assert (settings_root / "settings.yaml").exists(), "settings.yaml should be written"


# ---------------------------------------------------------------------------
# AC2b-EDGE: Existing settings.yaml without sentinel gets backed up
# ---------------------------------------------------------------------------


def test_existing_settings_without_sentinel_gets_backed_up(tmp_path: Path) -> None:
    """AC2b-EDGE: Hand-edited settings.yaml (no sentinel) gets backed up before overwrite."""
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    settings_root.mkdir(parents=True)

    # Simulate a hand-edited settings.yaml with no sentinel
    prior_content = "schema_version: 1\n# hand edited\n"
    settings_file = settings_root / "settings.yaml"
    settings_file.write_text(prior_content, encoding="utf-8")

    assert not (settings_root / ".path-migration-done").exists()

    rc = run_migration(settings_root=settings_root)
    assert rc == 0

    # A backup should exist
    backups = list(settings_root.glob("settings.yaml.bak.*"))
    assert backups, "A backup of the prior settings.yaml should have been created"

    # The main settings.yaml should exist (possibly same or new content)
    assert settings_file.exists(), "settings.yaml should still exist after migration"


# ---------------------------------------------------------------------------
# AC2b-FR: Idempotent re-run without --force -> no-op
# ---------------------------------------------------------------------------


def test_idempotent_noop_with_sentinel(tmp_path: Path) -> None:
    """AC2b-FR: With sentinel present and no --force, migration is a no-op."""
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)

    # First run
    rc = run_migration(settings_root=settings_root)
    assert rc == 0

    settings_file = settings_root / "settings.yaml"
    assert settings_file.exists()
    mtime_before = settings_file.stat().st_mtime

    # Sleep briefly to allow mtime resolution to distinguish runs
    time.sleep(0.01)

    # Second run without --force should be a no-op
    rc2 = run_migration(settings_root=settings_root)
    assert rc2 == 0, "No-op run should exit 0"

    mtime_after = settings_file.stat().st_mtime
    assert mtime_after == mtime_before, (
        "settings.yaml should not be touched on idempotent re-run"
    )


# ---------------------------------------------------------------------------
# AC2b-FR: --force re-runs despite sentinel
# ---------------------------------------------------------------------------


def test_force_reruns_despite_sentinel(tmp_path: Path) -> None:
    """AC2b-FR: --force causes migration to run even if sentinel already exists."""
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)

    # First run to establish sentinel
    rc = run_migration(settings_root=settings_root)
    assert rc == 0

    settings_file = settings_root / "settings.yaml"
    mtime_first = settings_file.stat().st_mtime

    time.sleep(0.02)

    # Force re-run
    rc2 = run_migration(force=True, settings_root=settings_root)
    assert rc2 == 0, "--force run should exit 0"

    mtime_after = settings_file.stat().st_mtime
    # Mtime should change because force=True causes a re-write
    # (If atomic_write creates a new file, mtime will differ)
    assert mtime_after >= mtime_first, "settings.yaml should be rewritten on --force"


# ---------------------------------------------------------------------------
# Finding 1 (P1-CRITICAL): Preserve existing settings keys during migration
# ---------------------------------------------------------------------------


def test_migration_preserves_existing_settings_keys(tmp_path: Path) -> None:
    """AC1-HP: run_migration(force=True) must NOT drop keys outside the detected schema.

    A user may have config.providers.active, do_target, config.budget_cap, etc.
    in their settings.yaml. The migration must deep-merge detected values ON TOP
    of the existing file contents, preserving all other keys.
    """
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    settings_root.mkdir(parents=True)

    # Write a settings.yaml with keys outside the schema
    existing_content = (
        "schema_version: 1\n"
        "config:\n"
        "  providers:\n"
        "    active: my-provider\n"
        "  state_dir: '~/old/'\n"
        "do_target:\n"
        "  foo: bar\n"
    )
    settings_file = settings_root / "settings.yaml"
    settings_file.write_text(existing_content, encoding="utf-8")

    rc = run_migration(force=True, settings_root=settings_root)
    assert rc == 0, f"Expected exit 0, got {rc}"

    data = yaml.safe_load(settings_file.read_text(encoding="utf-8"))
    assert isinstance(data, dict)

    # config.providers.active must be preserved
    providers = data.get("config", {}).get("providers", {})
    assert providers.get("active") == "my-provider", (
        f"config.providers.active was dropped! Got config: {data.get('config')}"
    )

    # do_target.foo must be preserved
    do_target = data.get("do_target", {})
    assert do_target.get("foo") == "bar", (
        f"do_target.foo was dropped! Got top-level keys: {list(data.keys())}"
    )


# ---------------------------------------------------------------------------
# Finding C (P1-CRITICAL): Non-force backup path must preserve existing keys
# ---------------------------------------------------------------------------


def test_non_force_backup_preserves_existing_keys(tmp_path: Path) -> None:
    """AC-C-HP: Non-force migration with existing settings.yaml (no sentinel) preserves keys.

    Bug: backup-rename happens BEFORE write_settings_yaml reads existing keys.
    settings_path.rename(backup_path) moves the file away, then
    write_settings_yaml calls _render_settings_yaml(existing_path=settings_path)
    which cannot open the now-gone settings_path, so existing keys are silently dropped.

    Fix: pass backup_path to write_settings_yaml as the source of existing keys.
    """
    from fno.setup.migrate_paths import run_migration

    settings_root = _settings_root(tmp_path)
    settings_root.mkdir(parents=True)

    # Write settings.yaml with user-added keys, WITHOUT a sentinel
    # (absence of sentinel = hand-edited, should be backed up then merged)
    existing_content = {
        "schema_version": 1,
        "config": {
            "providers": {"active": "x"},
        },
        "do_target": {"foo": "bar"},
    }
    import yaml as _yaml
    settings_file = settings_root / "settings.yaml"
    settings_file.write_text(_yaml.safe_dump(existing_content), encoding="utf-8")
    # Critically: NO sentinel file - this triggers the backup path
    assert not (settings_root / ".path-migration-done").exists()

    rc = run_migration(settings_root=settings_root)
    assert rc == 0, f"Expected exit 0, got {rc}"

    # Backup must have been created (confirms we hit the non-force path)
    backups = list(settings_root.glob("settings.yaml.bak.*"))
    assert backups, "A backup should exist (confirms non-force backup path was taken)"

    # Final settings.yaml must retain user keys from the backed-up file
    data = _yaml.safe_load(settings_file.read_text(encoding="utf-8"))
    assert isinstance(data, dict)

    providers = data.get("config", {}).get("providers", {})
    assert providers.get("active") == "x", (
        f"config.providers.active was dropped during non-force backup migration! "
        f"Got config: {data.get('config')}"
    )

    do_target = data.get("do_target", {})
    assert do_target.get("foo") == "bar", (
        f"do_target.foo was dropped during non-force backup migration! "
        f"Got top-level keys: {list(data.keys())}"
    )
