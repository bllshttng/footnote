"""Tests for fno watch install/uninstall/status Typer subapp (Task 5.3).

Tests exercise the top-level `app` so the `watch` subapp wiring is verified.
All launchctl invocations are mocked; no real launchd state is touched.
FNO_LAUNCH_AGENTS_DIR env override routes plist writes to tmp_path.
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from unittest.mock import MagicMock

import pytest
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

def _write_settings(repo_root: Path, enabled: bool) -> None:
    """Write a minimal settings.yaml with config.inbox.watch.enabled."""
    abilities_dir = repo_root / ".fno"
    abilities_dir.mkdir(parents=True, exist_ok=True)
    settings = abilities_dir / "settings.yaml"
    settings.write_text(
        f"config:\n  inbox:\n    watch:\n      enabled: {'true' if enabled else 'false'}\n",
        encoding="utf-8",
    )


def _fake_run_noop(*args, **kwargs) -> MagicMock:
    """Stub for subprocess.run that does nothing and returns rc=0."""
    m = MagicMock()
    m.returncode = 0
    m.stdout = ""
    m.stderr = ""
    return m


def _fake_run_rc1(*args, **kwargs) -> MagicMock:
    """Stub for subprocess.run that returns rc=1 (daemon not loaded)."""
    m = MagicMock()
    m.returncode = 1
    m.stdout = ""
    m.stderr = "Could not find service"
    return m


# ---------------------------------------------------------------------------
# AC1-HP: install writes plist and calls launchctl load
# ---------------------------------------------------------------------------

def test_install_writes_plist_and_loads(tmp_path, monkeypatch):
    """AC1-HP: install creates plist in FNO_LAUNCH_AGENTS_DIR and calls launchctl load."""
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()
    _write_settings(repo_root, enabled=True)

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()

    calls = []

    def fake_run(cmd, **kwargs):
        calls.append(cmd)
        m = MagicMock()
        m.returncode = 0
        return m

    monkeypatch.setattr("subprocess.run", fake_run)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "install"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}:\n{result.output}"

    plist_path = launch_agents_dir / "com.fno.watch.myproject.plist"
    assert plist_path.exists(), f"plist not created at {plist_path}"
    content = plist_path.read_text(encoding="utf-8")
    assert "myproject" in content, "project name not in plist content"

    load_calls = [c for c in calls if "load" in c]
    assert len(load_calls) == 1, f"Expected 1 launchctl load call, got: {load_calls}"
    assert str(plist_path) in load_calls[0], "plist path not passed to launchctl load"

    assert "installed" in result.output


# ---------------------------------------------------------------------------
# Codex PR #428: uninstall must also clean up a pre-rename legacy watcher
# ---------------------------------------------------------------------------

def test_uninstall_removes_legacy_label(tmp_path, monkeypatch):
    """uninstall unloads + removes a pre-rename com.bllshttng watcher (no orphan)."""
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()
    # Simulate a watcher installed before the de-branding rename.
    legacy_plist = launch_agents_dir / "com.bllshttng.fno.watch.myproject.plist"
    legacy_plist.write_text("<plist/>", encoding="utf-8")

    calls = []

    def fake_run(cmd, **kwargs):
        calls.append(cmd)
        m = MagicMock()
        m.returncode = 0
        return m

    monkeypatch.setattr("subprocess.run", fake_run)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "uninstall"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}:\n{result.output}"
    assert not legacy_plist.exists(), "legacy plist should be removed"
    unload_calls = [c for c in calls if "unload" in c]
    assert any(str(legacy_plist) in c for c in unload_calls), (
        f"expected launchctl unload of legacy plist, got: {unload_calls}"
    )
    assert "removed legacy watcher" in result.output
    assert "already uninstalled" not in result.output


# ---------------------------------------------------------------------------
# AC2-ERR: install refuses when watch.enabled is false
# ---------------------------------------------------------------------------

def test_install_refuses_when_disabled(tmp_path, monkeypatch):
    """AC2-ERR: install exits 1 with stderr message when watch.enabled=false."""
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()
    _write_settings(repo_root, enabled=False)

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()

    calls = []

    def fake_run(cmd, **kwargs):
        calls.append(cmd)
        m = MagicMock()
        m.returncode = 0
        return m

    monkeypatch.setattr("subprocess.run", fake_run)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "install"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
    )

    assert result.exit_code == 1, f"Expected 1, got {result.exit_code}:\n{result.output}"

    # Message appears in output (typer.testing merges stdout/stderr)
    combined = result.output + (result.stderr if hasattr(result, "stderr") and result.stderr else "")
    assert "config.inbox.watch.enabled must be true" in combined, (
        f"Expected error message not found in: {combined!r}"
    )

    plist_path = launch_agents_dir / "com.fno.watch.myproject.plist"
    assert not plist_path.exists(), "plist should NOT be created when disabled"

    assert len(calls) == 0, f"launchctl should not be called when disabled, got: {calls}"


# ---------------------------------------------------------------------------
# AC1-HP-2: uninstall removes plist and calls launchctl unload
# ---------------------------------------------------------------------------

def test_uninstall_removes_plist_and_unloads(tmp_path, monkeypatch):
    """AC1-HP-2: uninstall calls launchctl unload and deletes the plist file."""
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()

    # Pre-create the plist file
    plist_path = launch_agents_dir / "com.fno.watch.myproject.plist"
    plist_path.write_text("<plist/>", encoding="utf-8")

    calls = []

    def fake_run(cmd, **kwargs):
        calls.append(cmd)
        m = MagicMock()
        m.returncode = 0
        return m

    monkeypatch.setattr("subprocess.run", fake_run)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "uninstall"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}:\n{result.output}"
    assert not plist_path.exists(), "plist should be deleted after uninstall"

    unload_calls = [c for c in calls if "unload" in c]
    assert len(unload_calls) == 1, f"Expected 1 launchctl unload call, got: {unload_calls}"
    assert str(plist_path) in unload_calls[0], "plist path not passed to launchctl unload"

    assert "uninstalled" in result.output


# ---------------------------------------------------------------------------
# AC1-HP-3: status reports loaded: yes with last log line
# ---------------------------------------------------------------------------

def test_status_loaded(tmp_path, monkeypatch):
    """AC1-HP-3: status shows loaded: yes and last log line when launchctl rc=0."""
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()

    # Pre-create a log file with a known last line
    abilities_dir = repo_root / ".fno"
    abilities_dir.mkdir()
    log_file = abilities_dir / "abi-watch.log"
    log_file.write_text(
        "2026-05-05T18:00:00Z starting drain\n"
        "2026-05-05T18:01:00Z drained 3 messages\n",
        encoding="utf-8",
    )

    def fake_run(cmd, **kwargs):
        m = MagicMock()
        m.returncode = 0
        m.stdout = '{"PID": 1234}'
        m.stderr = ""
        return m

    monkeypatch.setattr("subprocess.run", fake_run)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "status"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}:\n{result.output}"
    assert "loaded: yes" in result.output, f"'loaded: yes' missing from: {result.output!r}"
    assert "drained 3 messages" in result.output, f"last log line missing from: {result.output!r}"


# ---------------------------------------------------------------------------
# AC-NEW: status reports loaded: no when launchctl rc=1
# ---------------------------------------------------------------------------

def test_install_surfaces_launchctl_failure(tmp_path, monkeypatch):
    """AC3-ERR: install exits 2 and prints launchctl stderr when launchctl load fails.

    Also verifies the plist is rolled back (deleted) on failure so re-running
    install starts clean.

    Caught by sigma-review HIGH (silent-failure-hunter + code-reviewer).
    """
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()
    _write_settings(repo_root, enabled=True)

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()

    def fake_run_fail(cmd, **kwargs):
        m = MagicMock()
        m.returncode = 1
        m.stdout = ""
        m.stderr = "plist syntax error at line 42"
        return m

    monkeypatch.setattr("subprocess.run", fake_run_fail)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "install"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
    )

    assert result.exit_code == 2, f"Expected exit code 2, got {result.exit_code}:\n{result.output}"

    # Error message must be surfaced
    combined = result.output + (result.stderr if hasattr(result, "stderr") and result.stderr else "")
    assert "plist syntax error" in combined or "launchctl load failed" in combined, (
        f"Expected launchctl error in output, got: {combined!r}"
    )

    # Plist must be rolled back
    plist_path = launch_agents_dir / "com.fno.watch.myproject.plist"
    assert not plist_path.exists(), "plist should be removed on launchctl load failure (rollback)"


def test_uninstall_surfaces_launchctl_failure(tmp_path, monkeypatch):
    """AC3-ERR-2: uninstall exits 2 and prints launchctl stderr when launchctl unload fails.

    The plist should NOT be deleted when unload fails (leave it for diagnostics).

    Caught by sigma-review HIGH (silent-failure-hunter + code-reviewer).
    """
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()

    plist_path = launch_agents_dir / "com.fno.watch.myproject.plist"
    plist_path.write_text("<plist/>", encoding="utf-8")

    def fake_run_fail(cmd, **kwargs):
        m = MagicMock()
        m.returncode = 1
        m.stdout = ""
        m.stderr = "service not found"
        return m

    monkeypatch.setattr("subprocess.run", fake_run_fail)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "uninstall"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
    )

    assert result.exit_code == 2, f"Expected exit code 2, got {result.exit_code}:\n{result.output}"

    combined = result.output + (result.stderr if hasattr(result, "stderr") and result.stderr else "")
    assert "service not found" in combined or "launchctl unload failed" in combined, (
        f"Expected launchctl error in output, got: {combined!r}"
    )


def test_status_not_loaded(tmp_path, monkeypatch):
    """AC-NEW: status shows loaded: no when launchctl list returns rc=1."""
    repo_root = tmp_path / "myproject"
    repo_root.mkdir()

    launch_agents_dir = tmp_path / "LaunchAgents"
    launch_agents_dir.mkdir()

    def fake_run(cmd, **kwargs):
        m = MagicMock()
        m.returncode = 1
        m.stdout = ""
        m.stderr = "Could not find service"
        return m

    monkeypatch.setattr("subprocess.run", fake_run)
    monkeypatch.chdir(repo_root)

    result = runner.invoke(
        app,
        ["watch", "status"],
        env={"FNO_LAUNCH_AGENTS_DIR": str(launch_agents_dir)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}:\n{result.output}"
    assert "loaded: no" in result.output, f"'loaded: no' missing from: {result.output!r}"
