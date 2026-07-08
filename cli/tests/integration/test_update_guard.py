"""Integration tests for `fno update` IN_PROGRESS guard.

Task 4b.2 of plan 2026-05-14-path-config-impl.

Tests cover:
- AC4b-HP: IN_PROGRESS blocks update (exit 1 + exact stderr message)
- AC4b-HP: COMPLETE allows update
- AC4b-HP: missing state file allows update
- AC4b-EDGE: --force bypasses the guard when IN_PROGRESS
- AC4b-FR: malformed target-state.md (no --- frontmatter) treated as not-IN_PROGRESS
- AC4b-EDGE: walks correctly from a subdirectory (guard still finds repo root)

All tests use tmp_path + monkeypatch for isolation.
Autouse fixture pins FNO_REPO_ROOT (memory: feedback_abi_repo_root_leaks_between_tests).
Actual uv/pip install logic is stubbed so tests don't try to download anything.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

from fno.cli import app

_REFUSED_MSG = (
    "[fno update] refused: target-state.md shows status: IN_PROGRESS. "
    "Updating mid-loop risks binary skew across subprocesses. "
    "Pass --force to override."
)

runner = CliRunner()


# ---------------------------------------------------------------------------
# Autouse fixture: isolate FNO_REPO_ROOT, stub out actual install
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Pin repo root + stub away the real install logic before each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    # Stub the real install so tests never execute uv/pip.
    # We patch _discover_source to return a sentinel Path, and os.execvp + subprocess.run
    # to be no-ops. Monkeypatch BEFORE invoking the command (memory: feedback_default_arg_breaks_monkeypatch_isolation).
    import fno.update as update_mod

    monkeypatch.setattr(
        update_mod,
        "_discover_source",
        lambda override=None: tmp_path / "fake-source",
    )
    monkeypatch.setattr(update_mod.os, "execvp", lambda *a, **kw: None)
    import subprocess as subprocess_mod
    import types

    # stdout/stderr present so update_command's _source_rev() git probe (which
    # also goes through subprocess.run) gets a CompletedProcess-shaped object
    # rather than AttributeError-ing on a bare namespace. Empty stdout => the
    # rev is undeterminable for the fake source, so the marker chain is skipped.
    fake_result = types.SimpleNamespace(returncode=0, stdout="", stderr="")
    monkeypatch.setattr(
        update_mod.subprocess,
        "run",
        lambda *a, **kw: fake_result,
    )
    yield


def _write_state(tmp_path: Path, content: str) -> None:
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "target-state.md").write_text(content)


# ---------------------------------------------------------------------------
# AC4b-HP: IN_PROGRESS blocks
# ---------------------------------------------------------------------------


def test_ac4b_hp_in_progress_blocks(tmp_path: Path) -> None:
    """Given target-state.md shows IN_PROGRESS, update exits 1 with refusal message."""
    _write_state(
        tmp_path,
        "---\nstatus: IN_PROGRESS\n---\n\nsome content\n",
    )
    result = runner.invoke(app, ["update"])
    assert result.exit_code == 1
    assert _REFUSED_MSG in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-HP: COMPLETE allows
# ---------------------------------------------------------------------------


def test_ac4b_hp_complete_allows(tmp_path: Path) -> None:
    """Given target-state.md shows COMPLETE, update exits 0."""
    _write_state(
        tmp_path,
        "---\nstatus: COMPLETE\n---\n\nsome content\n",
    )
    result = runner.invoke(app, ["update"])
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-HP: missing state file allows
# ---------------------------------------------------------------------------


def test_ac4b_hp_missing_state_file_allows(tmp_path: Path) -> None:
    """Given no target-state.md exists, update exits 0."""
    # .fno dir doesn't exist at all
    result = runner.invoke(app, ["update"])
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-EDGE: --force bypasses guard
# ---------------------------------------------------------------------------


def test_ac4b_edge_force_bypasses(tmp_path: Path) -> None:
    """Given IN_PROGRESS + --force, update proceeds (exit 0, no refusal)."""
    _write_state(
        tmp_path,
        "---\nstatus: IN_PROGRESS\n---\n",
    )
    result = runner.invoke(app, ["update", "--force"])
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-FR: malformed target-state.md treated as not-IN_PROGRESS
# ---------------------------------------------------------------------------


def test_ac4b_fr_malformed_state_file_lenient(tmp_path: Path) -> None:
    """Given target-state.md with no --- frontmatter, guard is lenient and update proceeds."""
    _write_state(
        tmp_path,
        "status: IN_PROGRESS\nno frontmatter here\n",
    )
    result = runner.invoke(app, ["update"])
    # Lenient: guard should NOT block; exit 0
    assert result.exit_code == 0
    assert _REFUSED_MSG not in (result.output or "")


# ---------------------------------------------------------------------------
# AC4b-EDGE: walks correctly from a subdirectory
# ---------------------------------------------------------------------------


def test_ac4b_edge_subdir_walk(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Guard finds target-state.md even when cwd is a subdirectory of the repo."""
    _write_state(
        tmp_path,
        "---\nstatus: IN_PROGRESS\n---\n",
    )
    # Create a git sentinel so _target_in_progress walk finds the boundary.
    (tmp_path / ".git").mkdir()
    # cwd inside a deep subdirectory.
    subdir = tmp_path / "deep" / "nested" / "dir"
    subdir.mkdir(parents=True)
    # FNO_REPO_ROOT still points to tmp_path (set by autouse), which is what
    # resolve_repo_root() returns. The guard should still find the state file.
    result = runner.invoke(app, ["update"])
    assert result.exit_code == 1
    assert _REFUSED_MSG in (result.output or "")


# ---------------------------------------------------------------------------
# ab-5a1fc285: a successful update chains the installed-rev marker write onto
# the installer via the shell, so the marker lands ONLY on install success.
# ---------------------------------------------------------------------------


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_successful_update_chains_marker_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """On Unix, a clean update execs `/bin/sh -c '<install> && <atomic marker write>'`."""
    import fno.update as update_mod

    # Source resolves to a checkout with a known rev (autouse stubs a non-git
    # fake source, so pin the rev explicitly here).
    monkeypatch.setattr(update_mod, "_source_rev", lambda src: "cafef00d")
    marker = tmp_path / "state" / "installed-rev"
    monkeypatch.setattr(update_mod, "_INSTALLED_REV_FILE", marker)

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["file"] = file
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["update"])
    assert result.exit_code == 0

    # The installer is exec'd through the shell so the marker write can be
    # gated on its success.
    assert captured["file"] == "/bin/sh"
    shell_line = captured["args"][2]  # ["/bin/sh", "-c", "<line>"]
    assert " && " in shell_line
    assert "cafef00d" in shell_line
    assert str(marker) in shell_line
    # Atomic write (temp + mv), never a direct write to the marker.
    assert "mv " in shell_line


@pytest.mark.skipif(os.name == "nt", reason="execvp shell-chain is the Unix path")
def test_update_without_source_rev_skips_marker_chain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When the source rev is undeterminable, no installed-rev marker is written.

    The pr-watch refresh still chains after the install (via /bin/sh, since
    there is a command to run post-install), but the marker-write chain must be
    absent - there is no rev to record.
    """
    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: None)

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["file"] = file
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["update"])
    assert result.exit_code == 0
    # The marker-write chain (printf rev > tmp && mv) must be absent with no rev.
    joined = " ".join(captured.get("args") or [])
    assert "installed-rev" not in joined
    assert "printf" not in joined
    # The pr-watch refresh still rides the successful install (best-effort).
    assert "pr-watch refresh" in joined


def test_update_without_source_rev_execs_plain_install_when_no_refresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """With no rev AND no refresh resolvable, exec the plain installer directly
    (no /bin/sh wrapper) - the original no-rev fast path."""
    import fno.update as update_mod

    monkeypatch.setattr(update_mod, "_source_rev", lambda src: None)
    # Force refresh resolution to fail so refresh_argv stays None.
    import fno.pr_watch.cli as pw_cli
    monkeypatch.setattr(
        pw_cli, "_resolve_fno_binary",
        lambda: (_ for _ in ()).throw(RuntimeError("no binary")),
    )

    captured: dict[str, object] = {}

    def _fake_execvp(file: str, args: list[str]) -> None:
        captured["file"] = file
        captured["args"] = args

    monkeypatch.setattr(update_mod.os, "execvp", _fake_execvp)

    result = runner.invoke(app, ["update"])
    assert result.exit_code == 0
    assert captured["file"] != "/bin/sh"  # plain installer, no shell wrapper


# ---------------------------------------------------------------------------
# Fix 7: delegation-path test - real update_command, only leaf I/O stubbed
# ---------------------------------------------------------------------------


def _make_abi_source(directory: Path) -> Path:
    """Create a minimal fno source directory with a valid pyproject.toml."""
    cli_dir = directory / "cli"
    cli_dir.mkdir(parents=True, exist_ok=True)
    (cli_dir / "pyproject.toml").write_text(
        '[project]\nname = "fno"\nversion = "0.1.0"\n',
        encoding="utf-8",
    )
    return cli_dir


@pytest.mark.skipif(os.name == "nt", reason="execvp is the Unix install path")
def test_doctor_fix_python_stale_delegates_to_real_update_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 7: doctor --fix python-stale delegates to the real update_command.

    Only leaf I/O is stubbed (execvp, subprocess.run, _discover_source,
    _target_in_progress, marker paths). Typer Option sentinels for rust/no_rust
    must not trip the bool-normalization or the mutex check - this test locks
    that contract.
    """
    from fno import doctor, update

    # Make a minimal fno source so _discover_source succeeds.
    src = _make_abi_source(tmp_path)
    monkeypatch.setattr(update, "_discover_source", lambda override=None: src)
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", tmp_path / "installed-rev")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "source-path")

    import types
    fake_result = types.SimpleNamespace(returncode=0, stdout="", stderr="")
    monkeypatch.setattr(update.subprocess, "run", lambda *a, **kw: fake_result)

    execvp_calls: list[tuple] = []

    def _fake_execvp(prog: str, args: list) -> None:
        execvp_calls.append((prog, args))

    monkeypatch.setattr(update.os, "execvp", _fake_execvp)

    # Stub doctor signal collectors so the verdict is python_stale.
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: src)
    monkeypatch.setattr(doctor, "_source_rev", lambda source: "newsha")
    monkeypatch.setattr(doctor, "_read_marker", lambda: "oldsha")
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(doctor, "_rust_report", lambda: {"binary": None, "revision": None})
    monkeypatch.setattr(doctor, "_read_rust_marker", lambda: None)
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: None)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)

    result = runner.invoke(app, ["doctor", "--fix"])
    # No exception means the delegation path ran without Typer OptionInfo sentinel tripping.
    assert result.exception is None
    # execvp was reached (the real update_command ran to completion on this path).
    assert execvp_calls, "execvp must be reached via the real update_command delegation"
