"""Unit tests for fno update source-discovery logic.

Covers the path-resolution surface only. The actual install (`os.execvp` into
`uv tool install`) is a system-level effect and is not exercised here; it has
no logic to test beyond "did we pick the right command name", which is covered
by the inspectable `--dry-run` path.
"""
from __future__ import annotations

import subprocess
import types
from pathlib import Path

import pytest
import typer
from typer.testing import CliRunner

from fno import update
from fno.cli import app


@pytest.fixture(autouse=True)
def _isolate_triad_install_dirs(monkeypatch: pytest.MonkeyPatch) -> None:
    """Safety net: never let a test's ``_sync_triad`` reach REAL install locations.

    ``_triad_install_dirs`` reads the live PATH / HOME (``~/.cargo/bin`` etc.), so a
    test that drives ``_refresh_rust_bins`` down its fresh/rebuilt path would copy
    its tmp fixture bins over the developer's actual fno-agents install. Default
    every test to an empty install-dir list; the dedicated ``_sync_triad`` tests
    override this with their own tmp dirs.
    """
    monkeypatch.setattr(update, "_triad_install_dirs", lambda: [])


def _write_pyproject(directory: Path, name: str = "fno") -> None:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "pyproject.toml").write_text(
        f'[project]\nname = "{name}"\nversion = "0.1.0"\n',
        encoding="utf-8",
    )


def test_looks_like_abi_source_true_for_abilities_pyproject(tmp_path: Path) -> None:
    _write_pyproject(tmp_path / "cli")
    assert update._looks_like_abi_source(tmp_path / "cli") is True


def test_looks_like_abi_source_false_for_other_pyproject(tmp_path: Path) -> None:
    _write_pyproject(tmp_path / "cli", name="something-else")
    assert update._looks_like_abi_source(tmp_path / "cli") is False


def test_looks_like_abi_source_false_when_missing(tmp_path: Path) -> None:
    assert update._looks_like_abi_source(tmp_path / "nonexistent") is False


def test_looks_like_abi_source_false_when_name_outside_project_table(
    tmp_path: Path,
) -> None:
    """Guard against false-match: `name = "fno"` outside [project] must not count."""
    (tmp_path / "pyproject.toml").write_text(
        '[project]\nname = "something-else"\nversion = "0.1.0"\n\n'
        '[tool.example]\nname = "fno"\n',
        encoding="utf-8",
    )
    assert update._looks_like_abi_source(tmp_path) is False


def test_looks_like_abi_source_false_on_malformed_toml(tmp_path: Path) -> None:
    (tmp_path / "pyproject.toml").write_text(
        "this is not [valid toml = at all",
        encoding="utf-8",
    )
    assert update._looks_like_abi_source(tmp_path) is False


def test_discover_source_with_override_validates(tmp_path: Path) -> None:
    src = tmp_path / "cli"
    _write_pyproject(src)
    resolved = update._discover_source(override=src)
    assert resolved == src.resolve()


def test_discover_source_with_invalid_override_raises(tmp_path: Path) -> None:
    bad = tmp_path / "not-fno"
    _write_pyproject(bad, name="another-package")
    with pytest.raises(update.SourceNotFoundError, match="does not contain"):
        update._discover_source(override=bad)


def test_discover_source_falls_back_to_env_var(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    src = tmp_path / "cli"
    _write_pyproject(src)
    monkeypatch.setenv("FNO_SOURCE", str(src))
    # Point cache and candidates at empty dirs so env var is the only hit.
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "nonexistent-cache")
    monkeypatch.setattr(update, "_CANDIDATE_PATHS", (tmp_path / "candidate",))
    resolved = update._discover_source()
    assert resolved == src.resolve()


def test_discover_source_falls_back_to_cache_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    src = tmp_path / "cli"
    _write_pyproject(src)
    cache = tmp_path / "cache" / "source-path"
    cache.parent.mkdir()
    cache.write_text(f"{src}\n", encoding="utf-8")
    monkeypatch.delenv("FNO_SOURCE", raising=False)
    monkeypatch.setattr(update, "_CACHE_FILE", cache)
    monkeypatch.setattr(update, "_CANDIDATE_PATHS", (tmp_path / "candidate",))
    resolved = update._discover_source()
    assert resolved == src.resolve()


def test_discover_source_falls_back_to_candidates(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    src = tmp_path / "candidate-cli"
    _write_pyproject(src)
    monkeypatch.delenv("FNO_SOURCE", raising=False)
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "no-cache")
    monkeypatch.setattr(update, "_CANDIDATE_PATHS", (src,))
    resolved = update._discover_source()
    assert resolved == src.resolve()


def test_discover_source_raises_when_nothing_matches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("FNO_SOURCE", raising=False)
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "no-cache")
    monkeypatch.setattr(update, "_CANDIDATE_PATHS", (tmp_path / "candidate",))
    with pytest.raises(update.SourceNotFoundError, match="Could not locate"):
        update._discover_source()


def test_discover_source_priority_override_beats_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Explicit --source should always win over env var, cache, and candidates."""
    override_src = tmp_path / "override-cli"
    env_src = tmp_path / "env-cli"
    _write_pyproject(override_src)
    _write_pyproject(env_src)
    monkeypatch.setenv("FNO_SOURCE", str(env_src))
    resolved = update._discover_source(override=override_src)
    assert resolved == override_src.resolve()


def test_cache_source_path_writes_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cache = tmp_path / "subdir" / "source-path"
    monkeypatch.setattr(update, "_CACHE_FILE", cache)
    update._cache_source_path(Path("/some/source/path"))
    assert cache.is_file()
    assert cache.read_text(encoding="utf-8").strip() == "/some/source/path"


def test_cache_source_path_silent_on_oserror(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Cache failures must not propagate - missing cache just means re-discovery."""
    # Point cache at a path whose parent cannot be created (a regular file).
    blocker = tmp_path / "blocker"
    blocker.write_text("not a directory")
    monkeypatch.setattr(update, "_CACHE_FILE", blocker / "child" / "source-path")
    # Should not raise.
    update._cache_source_path(Path("/some/source"))


# ---------------------------------------------------------------------------
# Fix 7: OSError reading target-state.md must fail SAFE (return True = IN_PROGRESS)
# ---------------------------------------------------------------------------


def test_target_in_progress_returns_true_on_oserror(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """Fix 7: PermissionError reading target-state.md must return True (fail safe).

    The guard's purpose is to prevent fno update during an active target loop.
    An unreadable state file must default to True (treat as IN_PROGRESS) so
    the guard doesn't silently open the gate on a filesystem error.
    """
    import logging

    # Set up a real target-state.md path that raises PermissionError on read
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    abilities_dir = repo_root / ".fno"
    abilities_dir.mkdir()
    state_file = abilities_dir / "target-state.md"
    state_file.write_text("---\nstatus: IN_PROGRESS\n---\n")
    state_file.chmod(0o000)  # unreadable

    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))

    try:
        with caplog.at_level(logging.WARNING, logger="fno.update"):
            result = update._target_in_progress()

        assert result is True, (
            "OSError reading target-state.md must return True (fail safe, not fail open)"
        )
        assert any(
            "permission" in record.message.lower()
            or "oserror" in record.message.lower()
            or "warning" in record.levelname.lower()
            for record in caplog.records
        ), f"Expected warning log on OSError, got: {[r.message for r in caplog.records]}"
    finally:
        state_file.chmod(0o644)


# ---------------------------------------------------------------------------
# installed-rev marker (ab-5a1fc285): _source_rev / _write_installed_rev /
# _install_then_mark. The marker lets `fno doctor` compare the installed rev
# against the source HEAD without a network call.
# ---------------------------------------------------------------------------


def _init_git_repo(directory: Path) -> str:
    """Create a git repo with one commit; return its HEAD sha."""
    directory.mkdir(parents=True, exist_ok=True)
    env = {
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@e",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@e",
    }
    import os as _os
    import subprocess as _sp

    run_env = {**_os.environ, **env}
    _sp.run(["git", "init", "-q"], cwd=directory, check=True, env=run_env)
    (directory / "f.txt").write_text("x", encoding="utf-8")
    _sp.run(["git", "add", "."], cwd=directory, check=True, env=run_env)
    _sp.run(["git", "commit", "-qm", "init"], cwd=directory, check=True, env=run_env)
    head = _sp.run(
        ["git", "rev-parse", "HEAD"],
        cwd=directory,
        check=True,
        capture_output=True,
        text=True,
        env=run_env,
    )
    return head.stdout.strip()


def test_source_rev_returns_head_for_git_checkout(tmp_path: Path) -> None:
    src = tmp_path / "src"
    head = _init_git_repo(src)
    assert update._source_rev(src) == head


def test_source_rev_none_for_non_git_dir(tmp_path: Path) -> None:
    """A dir that is not a git checkout yields None, not a crash (Failure Modes)."""
    plain = tmp_path / "plain"
    plain.mkdir()
    assert update._source_rev(plain) is None


def test_write_installed_rev_writes_atomically(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    marker = tmp_path / "state" / "installed-rev"
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", marker)
    update._write_installed_rev("abc123")
    assert marker.is_file()
    assert marker.read_text(encoding="utf-8").strip() == "abc123"
    # No temp file left behind after the atomic rename.
    leftovers = list(marker.parent.glob(".installed-rev.*.tmp"))
    assert leftovers == []


def test_write_installed_rev_silent_on_oserror(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Marker write failures must not propagate (best-effort, mirrors cache write)."""
    blocker = tmp_path / "blocker"
    blocker.write_text("not a directory")
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", blocker / "child" / "installed-rev")
    update._write_installed_rev("deadbeef")  # must not raise


def test_install_then_mark_gates_marker_on_install_success(tmp_path: Path) -> None:
    """The shell line writes the marker ONLY after a zero install exit (&&)."""
    marker = tmp_path / "state" / "installed-rev"
    line = update._install_then_mark(
        ["uv", "tool", "install", "--reinstall", "/some src"],
        "abc123",
        marker=marker,
        pid=4242,
    )
    # Install runs first, gated by && before the marker write.
    assert "uv tool install --reinstall" in line
    assert " && " in line
    assert line.index("uv tool install") < line.index("printf")
    # Atomic: write a temp then mv into place (never write the marker directly).
    assert ".installed-rev.4242.tmp" in line
    assert "mv " in line
    assert str(marker) in line
    # The rev is the payload written.
    assert "abc123" in line


def test_install_then_mark_runs_as_valid_shell_marker_write(tmp_path: Path) -> None:
    """Executing the line with a true-install stub actually lands the marker."""
    import subprocess as _sp

    marker = tmp_path / "state" / "installed-rev"
    # Replace the install command with `true` so only the marker chain runs.
    line = update._install_then_mark(["true"], "feedface", marker=marker, pid=99)
    _sp.run(["/bin/sh", "-c", line], check=True)
    assert marker.read_text(encoding="utf-8").strip() == "feedface"


def test_install_then_mark_skips_marker_on_install_failure(tmp_path: Path) -> None:
    """A non-zero install (`false`) must leave no marker behind AND propagate the
    failure (the install gates the marker write)."""
    import subprocess as _sp

    marker = tmp_path / "state" / "installed-rev"
    line = update._install_then_mark(["false"], "feedface", marker=marker, pid=7)
    result = _sp.run(["/bin/sh", "-c", line], check=False)
    assert result.returncode != 0
    assert not marker.exists()


def test_install_then_mark_marker_failure_preserves_success(tmp_path: Path) -> None:
    """Codex review: a SUCCESSFUL install whose marker write fails must still
    exit 0 - the diagnostic marker must not override a real reinstall."""
    import subprocess as _sp

    # Make the marker's parent un-creatable: a regular FILE sits where the dir
    # should be, so `mkdir -p` inside the chain fails.
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir", encoding="utf-8")
    marker = blocker / "state" / "installed-rev"
    line = update._install_then_mark(["true"], "feedface", marker=marker, pid=8)
    result = _sp.run(["/bin/sh", "-c", line], check=False)
    assert result.returncode == 0, "successful install must survive a marker-write failure"
    assert not marker.exists()


def test_install_then_mark_post_install_runs_after_success(tmp_path: Path) -> None:
    """post_install runs after a successful install; its output proves ordering."""
    import subprocess as _sp

    marker = tmp_path / "state" / "installed-rev"
    sentinel = tmp_path / "refreshed"
    line = update._install_then_mark(
        ["true"], "feedface", marker=marker, pid=1,
        post_install=f"touch {sentinel}",
    )
    _sp.run(["/bin/sh", "-c", line], check=True)
    assert marker.read_text().strip() == "feedface"
    assert sentinel.exists(), "post_install must run after the install"


def test_install_then_mark_post_install_skipped_on_install_failure(tmp_path: Path) -> None:
    """A failed install short-circuits before post_install (never refresh onto a
    failed update) and still propagates the failure."""
    import subprocess as _sp

    marker = tmp_path / "state" / "installed-rev"
    sentinel = tmp_path / "refreshed"
    line = update._install_then_mark(
        ["false"], "feedface", marker=marker, pid=2,
        post_install=f"touch {sentinel}",
    )
    result = _sp.run(["/bin/sh", "-c", line], check=False)
    assert result.returncode != 0
    assert not sentinel.exists(), "post_install must not run when install fails"


def test_install_then_mark_post_install_failure_preserves_success(tmp_path: Path) -> None:
    """A post_install failure is best-effort (|| true): a successful install
    still exits 0 even when the refresh command fails."""
    import subprocess as _sp

    marker = tmp_path / "state" / "installed-rev"
    line = update._install_then_mark(
        ["true"], "feedface", marker=marker, pid=3,
        post_install="false",
    )
    result = _sp.run(["/bin/sh", "-c", line], check=False)
    assert result.returncode == 0, "a refresh failure must not fail the update"
    assert marker.read_text().strip() == "feedface"


def test_install_then_mark_omits_post_install_when_none(tmp_path: Path) -> None:
    """No post_install -> the line is byte-identical to the pre-feature shape."""
    marker = tmp_path / "state" / "installed-rev"
    base = update._install_then_mark(["true"], "r", marker=marker, pid=5)
    with_none = update._install_then_mark(
        ["true"], "r", marker=marker, pid=5, post_install=None
    )
    assert base == with_none


def test_write_installed_rev_cleans_temp_on_replace_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Gemini review: if os.replace fails after the temp is written, no orphaned
    .installed-rev.*.tmp is left behind."""
    marker = tmp_path / "state" / "installed-rev"
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", marker)

    def _boom(src, dst):  # noqa: ANN001
        raise OSError("simulated replace failure")

    monkeypatch.setattr(update.os, "replace", _boom)
    update._write_installed_rev("abc123")  # must not raise
    assert not marker.exists()
    leftovers = list(marker.parent.glob(".installed-rev.*.tmp"))
    assert leftovers == [], f"orphaned temp file(s) left behind: {leftovers}"


# ---------------------------------------------------------------------------
# Rust bins refresh (Task 1.1): helpers + _refresh_rust_bins + CLI wiring
# ---------------------------------------------------------------------------

runner = CliRunner()

GIT_ENV = {
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@e",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@e",
}


def _init_git_repo_with_crate(directory: Path) -> tuple[str, str]:
    """Create a git repo with both a root commit and a crates/fno-agents commit.

    Returns (head_rev, crate_rev) where crate_rev is the commit that touched
    crates/, which _rust_subtree_rev should return.
    """
    import os as _os
    import subprocess as _sp

    directory.mkdir(parents=True, exist_ok=True)
    run_env = {**_os.environ, **GIT_ENV}
    _sp.run(["git", "init", "-q"], cwd=directory, check=True, env=run_env)
    # Initial commit (not touching crates/)
    (directory / "f.txt").write_text("x", encoding="utf-8")
    _sp.run(["git", "add", "."], cwd=directory, check=True, env=run_env)
    _sp.run(["git", "commit", "-qm", "init"], cwd=directory, check=True, env=run_env)
    # Commit that touches crates/fno-agents/
    crate_dir = directory / "crates" / "fno-agents"
    crate_dir.mkdir(parents=True, exist_ok=True)
    (crate_dir / "Cargo.toml").write_text(
        '[package]\nname = "fno-agents"\nversion = "0.1.0"\n',
        encoding="utf-8",
    )
    _sp.run(["git", "add", "."], cwd=directory, check=True, env=run_env)
    _sp.run(["git", "commit", "-qm", "add crate"], cwd=directory, check=True, env=run_env)
    crate_rev = _sp.run(
        ["git", "rev-parse", "HEAD"],
        cwd=directory, check=True, capture_output=True, text=True, env=run_env,
    ).stdout.strip()
    # HEAD == crate_rev here since it's the last commit
    head_rev = crate_rev
    return head_rev, crate_rev


def _make_abi_source(directory: Path) -> Path:
    """Create a minimal abi-source directory with a valid pyproject.toml."""
    cli_dir = directory / "cli"
    cli_dir.mkdir(parents=True, exist_ok=True)
    (cli_dir / "pyproject.toml").write_text(
        '[project]\nname = "fno"\nversion = "0.1.0"\n',
        encoding="utf-8",
    )
    return cli_dir


# --- _rust_subtree_rev ---

def test_rust_subtree_rev_returns_commit_touching_crates(tmp_path: Path) -> None:
    """_rust_subtree_rev returns the last commit that touched crates/."""
    repo = tmp_path / "repo"
    _head, crate_rev = _init_git_repo_with_crate(repo)
    cli_dir = repo / "cli"
    cli_dir.mkdir(exist_ok=True)
    result = update._rust_subtree_rev(cli_dir)
    assert result == crate_rev


def test_rust_subtree_rev_none_for_non_git(tmp_path: Path) -> None:
    """Returns None for a plain directory (not a git repo)."""
    plain = tmp_path / "plain" / "cli"
    plain.mkdir(parents=True)
    assert update._rust_subtree_rev(plain) is None


def test_rust_subtree_rev_none_when_no_crates_commits(tmp_path: Path) -> None:
    """Returns None when crates/ has never been committed."""
    import os as _os
    import subprocess as _sp
    repo = tmp_path / "repo"
    repo.mkdir()
    run_env = {**_os.environ, **GIT_ENV}
    _sp.run(["git", "init", "-q"], cwd=repo, check=True, env=run_env)
    (repo / "f.txt").write_text("x")
    _sp.run(["git", "add", "."], cwd=repo, check=True, env=run_env)
    _sp.run(["git", "commit", "-qm", "init"], cwd=repo, check=True, env=run_env)
    cli_dir = repo / "cli"
    cli_dir.mkdir()
    result = update._rust_subtree_rev(cli_dir)
    assert result is None


# --- _read_rust_marker / _write_rust_marker ---

def test_read_rust_marker_returns_none_when_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "no-such-file")
    assert update._read_rust_marker() is None


def test_read_rust_marker_returns_content(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    marker = tmp_path / "installed-rust-rev"
    marker.write_text("abc123\n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker)
    assert update._read_rust_marker() == "abc123"


def test_read_rust_marker_returns_none_for_whitespace_only(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    marker = tmp_path / "installed-rust-rev"
    marker.write_text("   \n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker)
    assert update._read_rust_marker() is None


def test_write_rust_marker_atomic(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    marker = tmp_path / "state" / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker)
    assert update._write_rust_marker("deadbeef") is True
    assert marker.read_text(encoding="utf-8").strip() == "deadbeef"
    leftovers = list(marker.parent.glob("*.tmp"))
    assert leftovers == []


def test_write_rust_marker_silent_on_oserror(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", blocker / "child" / "installed-rust-rev")
    assert update._write_rust_marker("abc") is False  # must not raise


def test_write_rust_marker_cleans_temp_on_replace_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    marker = tmp_path / "state" / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker)

    def _boom(src, dst):  # noqa: ANN001
        raise OSError("simulated")

    monkeypatch.setattr(update.os, "replace", _boom)
    assert update._write_rust_marker("abc") is False  # must not raise
    assert not marker.exists()
    leftovers = list(marker.parent.glob("*.tmp"))
    assert leftovers == []


# --- _cargo_installed_bin ---

def test_cargo_installed_bin_returns_path_when_present(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    cargo_bin = tmp_path / "cargo" / "bin"
    cargo_bin.mkdir(parents=True)
    bin_name = "fno-agents.exe" if __import__("os").name == "nt" else "fno-agents"
    (cargo_bin / bin_name).write_text("fake binary")
    monkeypatch.setenv("CARGO_HOME", str(tmp_path / "cargo"))
    result = update._cargo_installed_bin()
    assert result is not None
    assert result.name == bin_name


def test_cargo_installed_bin_returns_none_when_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("CARGO_HOME", str(tmp_path / "nonexistent-cargo"))
    assert update._cargo_installed_bin() is None


# --- _refresh_rust_bins: gating outcomes (AC1-UI) ---

@pytest.mark.parametrize("outcome,setup", [
    ("skipped-no-crate", "no_crate"),
    ("skipped-no-binary", "no_binary"),
    ("skipped-no-rev", "no_rev"),
    ("fresh", "fresh"),
    ("skipped-no-cargo", "no_cargo"),
])
def test_refresh_rust_bins_gating_outcomes(
    outcome: str,
    setup: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture,
) -> None:
    """AC1-UI: each gating outcome prints exactly one identifying line and returns the right string."""
    source = tmp_path / "cli"
    source.mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    if setup == "no_crate":
        # No crates/fno-agents directory
        pass
    elif setup == "no_binary":
        # crate dir exists but no cargo binary
        (source.parent / "crates" / "fno-agents").mkdir(parents=True)
        monkeypatch.setattr(update, "_cargo_installed_bin", lambda: None)
    elif setup == "no_rev":
        # crate + binary exist but rev undeterminable
        (source.parent / "crates" / "fno-agents").mkdir(parents=True)
        fake_bin = tmp_path / "fake-fno-agents"
        fake_bin.write_text("x")
        monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
        monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: None)
    elif setup == "fresh":
        # binary self-reports the source crates/ rev (gate on the binary), and the
        # daemon + worker siblings are present so the fresh fast path is taken.
        (source.parent / "crates" / "fno-agents").mkdir(parents=True)
        fake_bin = tmp_path / "fake-fno-agents"
        fake_bin.write_text("x")
        for _n in update._triad_names():
            (tmp_path / _n).write_text("x")
        monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
        monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "abc123" * 2)
        monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: "abc123" * 2)
    elif setup == "no_cargo":
        # binary + rev exist but cargo not on PATH
        (source.parent / "crates" / "fno-agents").mkdir(parents=True)
        fake_bin = tmp_path / "fake-fno-agents"
        fake_bin.write_text("x")
        monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
        monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "stale123stale123")
        # Binary self-reports stale -> would rebuild, but cargo is absent.
        monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: None)
        # Patch through update module so _refresh_rust_bins sees it
        monkeypatch.setattr(update.shutil, "which", lambda name: None)

    result = update._refresh_rust_bins(source)
    assert result == outcome

    captured = capsys.readouterr()
    all_output = captured.out + captured.err
    assert all_output.strip(), f"outcome={outcome}: expected at least one output line, got none"


# --- AC1-HP: stale marker + binary + cargo -> "refreshed" ---

def test_ac1_hp_refresh_rust_bins_refreshed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """AC1-HP: stale marker + binary present + cargo on PATH -> returns 'refreshed',
    cargo cmd is correct, marker file updated to subtree rev."""
    source = tmp_path / "cli"
    source.mkdir()
    crate_dir = source.parent / "crates" / "fno-agents"
    crate_dir.mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    marker_file.write_text("oldrev\n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    subtree_rev = "a" * 40
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree_rev)

    recorded_calls: list[list[str]] = []
    state = {"built": False}

    def _fake_run(cmd, **kwargs):
        recorded_calls.append(list(cmd))
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    # gate + post-deploy verify interrogate the binary. Stale (None) before
    # cargo runs -> rebuild; the deployed binary reports the source rev -> verify OK.
    monkeypatch.setattr(
        update, "_installed_bin_crates_rev",
        lambda b, **kw: subtree_rev if state["built"] else None,
    )

    result = update._refresh_rust_bins(source)
    assert result == "refreshed"

    # Verify cargo command: must include --root pinned to the tested binary's root
    cargo_calls = [c for c in recorded_calls if c and c[0] == "cargo"]
    assert len(cargo_calls) == 1
    cmd = cargo_calls[0]
    assert cmd[:4] == ["cargo", "install", "--path", str(crate_dir)]
    assert "--bins" in cmd
    assert "--root" in cmd
    root_idx = cmd.index("--root")
    # fake_bin is at tmp_path / "fake-fno-agents" (no bin/ subdir), so
    # parent.parent == tmp_path.parent; use the actual value from the function.
    assert cmd[root_idx + 1] == str(fake_bin.parent.parent)

    # Marker updated
    assert marker_file.read_text(encoding="utf-8").strip() == subtree_rev


def test_ac1_err_stale_daemon_forces_rebuild(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """AC1-ERR: a PRESENT daemon self-reporting a different (older) crates_rev
    than the fresh client is NOT same-build, so the fresh fast path is skipped
    and cargo rebuilds the whole triad. This is the stale-present-sibling gap the
    daemon/worker version verb closes -- a presence-only check would miss it."""
    source = tmp_path / "cli"
    source.mkdir()
    crate_dir = source.parent / "crates" / "fno-agents"
    crate_dir.mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fno-agents"
    fake_bin.write_text("x")
    for _n in update._triad_names():
        (tmp_path / _n).write_text("x")  # all three PRESENT (presence check would pass)
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    subtree_rev = "a" * 40
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree_rev)

    state = {"built": False}
    cargo_calls: list[list[str]] = []

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            cargo_calls.append(list(cmd))
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)

    def _fake_rev(b, **kw):
        # Post-rebuild the whole triad reports the source rev (verify passes);
        # pre-rebuild the daemon sibling is stale (None) while client + worker are fresh.
        if state["built"]:
            return subtree_rev
        return None if b.name.endswith("-daemon") else subtree_rev

    monkeypatch.setattr(update, "_installed_bin_crates_rev", _fake_rev)

    result = update._refresh_rust_bins(source)
    assert result == "refreshed"
    assert len(cargo_calls) >= 1, "stale daemon must force a cargo rebuild, not the fresh path"


def test_refresh_rust_bins_also_installs_mux_front_door(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """When crates/fno is present, the rust leg installs the mux front door too:
    two cargo installs (fno-agents + fno), both into the same --root. Without a
    crates/fno dir the mux leg is a no-op (covered by the existing tests, which
    stage only crates/fno-agents and still see exactly one cargo call)."""
    source = tmp_path / "cli"
    source.mkdir()
    agents_crate = source.parent / "crates" / "fno-agents"
    agents_crate.mkdir(parents=True)
    mux_crate = source.parent / "crates" / "fno"
    mux_crate.mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    marker_file.write_text("oldrev\n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "a" * 40)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)

    recorded_calls: list[list[str]] = []
    state = {"built": False}

    def _fake_run(cmd, **kwargs):
        recorded_calls.append(list(cmd))
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    monkeypatch.setattr(
        update, "_installed_bin_crates_rev",
        lambda b, **kw: ("a" * 40) if state["built"] else None,
    )

    result = update._refresh_rust_bins(source)
    assert result == "refreshed"

    cargo_paths = [
        c[c.index("--path") + 1]
        for c in recorded_calls
        if c and c[0] == "cargo" and "--path" in c
    ]
    assert str(agents_crate) in cargo_paths, cargo_paths
    assert str(mux_crate) in cargo_paths, "the mux front door (crates/fno) must be installed too"


def test_refresh_rust_bins_installs_mux_on_fresh_marker_when_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Marker fresh (agents bins current) but the mux binary ABSENT -> still
    install the mux and return 'fresh'. This is the dead-end fix: a fresh-marker
    `fno update` must self-heal a stranded front door (the fno->fno-py rename
    lands fno-py while the mux was never installed), not no-op."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    mux_crate = source.parent / "crates" / "fno"
    mux_crate.mkdir(parents=True)
    subtree_rev = "a" * 40
    marker_file = tmp_path / "installed-rust-rev"
    marker_file.write_text(subtree_rev + "\n", encoding="utf-8")  # marker == subtree -> fresh
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    for _n in update._triad_names():
        (tmp_path / _n).write_text("x")  # triad siblings present (fresh fast path)
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree_rev)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: subtree_rev)  # fresh
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: None)  # mux ABSENT

    recorded: list[list[str]] = []
    monkeypatch.setattr(
        update.subprocess, "run",
        lambda cmd, **kw: (recorded.append(list(cmd)), types.SimpleNamespace(returncode=0))[1],
    )

    result = update._refresh_rust_bins(source)
    assert result == "fresh"
    mux_installs = [
        c for c in recorded
        if c[:2] == ["cargo", "install"] and str(mux_crate) in c
    ]
    assert mux_installs, "a fresh-marker update must install the mux when it is absent"


def test_refresh_rust_bins_fresh_marker_mux_present_installs_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Marker fresh AND the mux present-and-same-build (self-reports the source
    rev) -> the fast path stays fast: no cargo install at all. The mux heal only
    fires when the mux is missing OR stale."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    (source.parent / "crates" / "fno").mkdir(parents=True)
    subtree_rev = "a" * 40
    marker_file = tmp_path / "installed-rust-rev"
    marker_file.write_text(subtree_rev + "\n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    for _n in update._triad_names():
        (tmp_path / _n).write_text("x")  # triad siblings present (fresh fast path)
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree_rev)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: subtree_rev)  # fresh
    fake_mux = tmp_path / "fake-mux"
    fake_mux.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: fake_mux)  # mux PRESENT

    recorded: list[list[str]] = []
    monkeypatch.setattr(
        update.subprocess, "run",
        lambda cmd, **kw: (recorded.append(list(cmd)), types.SimpleNamespace(returncode=0))[1],
    )

    result = update._refresh_rust_bins(source)
    assert result == "fresh"
    assert not [c for c in recorded if c[:2] == ["cargo", "install"]], recorded


def test_refresh_rust_bins_fresh_marker_stale_mux_reinstalls(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fresh triad but the PRESENT mux self-reports an OLDER crates_rev -> reinstall
    just the mux. The mux install is best-effort, so a prior failed build can leave
    a stale `fno` beside a fresh triad; a presence-only heal would never repair it.
    Now that the mux bakes its own rev, `fno update` sees the stale front door."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    mux_crate = source.parent / "crates" / "fno"
    mux_crate.mkdir(parents=True)
    subtree_rev = "a" * 40
    marker_file = tmp_path / "installed-rust-rev"
    marker_file.write_text(subtree_rev + "\n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    for _n in update._triad_names():
        (tmp_path / _n).write_text("x")  # triad siblings present (fresh fast path)
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree_rev)
    fake_mux = tmp_path / "fno"
    fake_mux.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: fake_mux)  # mux PRESENT

    # The triad reports fresh; the mux (name "fno") reports an OLDER rev -> stale.
    def _rev(b, **kw):
        return "b" * 40 if Path(b).name == "fno" else subtree_rev

    monkeypatch.setattr(update, "_installed_bin_crates_rev", _rev)

    recorded: list[list[str]] = []
    monkeypatch.setattr(
        update.subprocess, "run",
        lambda cmd, **kw: (recorded.append(list(cmd)), types.SimpleNamespace(returncode=0))[1],
    )

    result = update._refresh_rust_bins(source)
    assert result == "fresh"
    mux_installs = [
        c for c in recorded
        if c[:2] == ["cargo", "install"] and str(mux_crate) in c
    ]
    assert mux_installs, "a present-but-stale mux must be reinstalled on the fresh path"


# --- legacy marker-write failure is a SUCCESSFUL refresh (post-deploy verify passed) ---

def test_refresh_rust_bins_marker_write_failure_still_refreshed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """Cargo succeeds and post-deploy verify passes, but the LEGACY marker write
    fails -> 'refreshed' (the bins are repaired; no verdict reads the marker).
    Reporting 'refreshed-no-marker' would make `fno doctor --fix` exit 1 on a
    successful repair."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir")
    monkeypatch.setattr(
        update, "_RUST_MARKER_FILE", blocker / "child" / "installed-rust-rev"
    )

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "b" * 40)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)
    state = {"built": False}

    def _fake_run(cmd, **kw):
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0 if cmd and cmd[0] == "cargo" else 1)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    monkeypatch.setattr(
        update, "_installed_bin_crates_rev",
        lambda b, **kw: ("b" * 40) if state["built"] else None,
    )

    result = update._refresh_rust_bins(source)
    assert result == "refreshed"
    captured = capsys.readouterr()
    assert "marker" in captured.err


def test_refresh_rust_bins_force_no_rev_returns_no_marker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """force=True with an undeterminable rev refreshes the bins but cannot
    record a marker; the next doctor run will still report rust stale, so
    the outcome must say so."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: None)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)
    monkeypatch.setattr(
        update.subprocess, "run",
        lambda cmd, **kw: types.SimpleNamespace(returncode=0 if cmd[0] == "cargo" else 1),
    )

    result = update._refresh_rust_bins(source, force=True)
    assert result == "refreshed-no-marker"
    assert not marker_file.exists()


# --- AC1-HP CLI-level: rust leg fires before execvp ---

def test_ac1_hp_cli_rust_fires_before_execvp(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP CLI: runner.invoke with --source fires rust leg before execvp."""
    repo = tmp_path / "repo"
    _head, crate_rev = _init_git_repo_with_crate(repo)
    cli_src = _make_abi_source(repo)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", tmp_path / "installed-rev")
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "source-path")
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)
    # Stub rev helpers directly so subprocess.run stub does not need stdout
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: crate_rev)
    monkeypatch.setattr(update, "_source_rev", lambda s: crate_rev)
    # Hermetic cargo-bin gate: CI runners have no ~/.cargo/bin/fno-agents,
    # so relying on the real filesystem here fails in CI and silently
    # passes on dev machines that happen to have the binary (PR #438).
    fake_bin = tmp_path / "cargo-home" / "bin" / "fno-agents"
    fake_bin.parent.mkdir(parents=True)
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)

    call_order: list[str] = []
    recorded_cargo: list[list[str]] = []
    state = {"built": False}

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            call_order.append("cargo")
            recorded_cargo.append(list(cmd))
            state["built"] = True
        elif cmd and cmd[0] == "pgrep":
            pass  # daemon advisory
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    # stale before cargo -> rebuild; deployed binary reports source rev -> verify OK.
    monkeypatch.setattr(
        update, "_installed_bin_crates_rev",
        lambda b, **kw: crate_rev if state["built"] else None,
    )

    def _fake_execvp(prog, args):
        call_order.append("execvp")

    monkeypatch.setattr(update.os, "execvp", _fake_execvp)
    # Patch shutil.which through the update module so _refresh_rust_bins sees it
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)

    result = runner.invoke(app, ["update", "--source", str(cli_src)])
    assert result.exit_code == 0 or result.exit_code is None, result.output

    # rust before python
    assert "cargo" in call_order
    assert "execvp" in call_order
    cargo_idx = call_order.index("cargo")
    execvp_idx = call_order.index("execvp")
    assert cargo_idx < execvp_idx


# --- AC1-ERR: cargo rc 1 -> "failed", warning to stderr, python update proceeds ---

def test_ac1_err_cargo_failure_warning_and_python_proceeds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """AC1-ERR: cargo rc 1 -> returns 'failed', stderr warning, marker NOT written."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "b" * 40)

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            return types.SimpleNamespace(returncode=1)
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)

    result = update._refresh_rust_bins(source)
    assert result == "failed"

    captured = capsys.readouterr()
    assert "WARNING" in captured.err or "failed" in captured.err.lower()
    assert "1" in captured.err  # exit code mentioned
    assert not marker_file.exists()


def test_ac1_err_cargo_oserror_warns_and_continues(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """AC1-ERR variant (PR #438 Gemini): cargo raising OSError at exec time
    (TOCTOU after the which() check, permission error, exec format error)
    -> 'failed', stderr warning, marker NOT written, never a crash."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "b" * 40)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            raise OSError("simulated exec failure")
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)

    result = update._refresh_rust_bins(source)
    assert result == "failed"

    captured = capsys.readouterr()
    assert "failed to execute" in captured.err
    assert not marker_file.exists()


def test_ac1_err_cli_execvp_still_called_after_cargo_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-ERR CLI: cargo failure does NOT abort the python install leg."""
    repo = tmp_path / "repo"
    _head, crate_rev = _init_git_repo_with_crate(repo)
    cli_src = _make_abi_source(repo)
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", tmp_path / "installed-rev")
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "source-path")
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)
    # Stub rev helpers directly so subprocess.run stub does not need stdout
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: crate_rev)
    monkeypatch.setattr(update, "_source_rev", lambda s: crate_rev)
    # Hermetic cargo-bin gate (PR #438): without this stub the rust leg
    # skips on CI runners (no cargo-installed binary) and the test only
    # passes vacuously; with it, the cargo-failure path runs everywhere.
    fake_bin = tmp_path / "cargo-home" / "bin" / "fno-agents"
    fake_bin.parent.mkdir(parents=True)
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)

    execvp_called = []

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            return types.SimpleNamespace(returncode=1)
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    monkeypatch.setattr(update.os, "execvp", lambda prog, args: execvp_called.append(prog))
    # Patch through the update module so _refresh_rust_bins sees it
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)

    runner.invoke(app, ["update", "--source", str(cli_src)])
    assert execvp_called, "execvp must be called even when cargo fails"


# --- AC1-EDGE ---

def test_ac1_edge_fresh_marker_skips_cargo(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE (a): marker == subtree -> 'fresh', subprocess.run NOT called for cargo."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    rev = "c" * 40
    marker_file.write_text(f"{rev}\n")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: tmp_path / "fake-bin")
    (tmp_path / "fake-bin").write_text("x")
    for _n in update._triad_names():
        (tmp_path / _n).write_text("x")  # triad siblings present (fresh fast path)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: rev)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: rev)  # fresh

    cargo_called = []

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            cargo_called.append(cmd)
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)

    result = update._refresh_rust_bins(source)
    assert result == "fresh"
    assert cargo_called == [], "cargo must not be invoked when the binary is fresh"


def test_ac1_edge_force_overrides_fresh_marker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE (b): --rust forces build even when marker is fresh."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    rev = "d" * 40
    marker_file.write_text(f"{rev}\n")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)
    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: rev)

    cargo_called = []
    state = {"built": False}

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            cargo_called.append(cmd)
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    # force rebuilds even though fresh; post-deploy verify must still see the rev.
    monkeypatch.setattr(
        update, "_installed_bin_crates_rev",
        lambda b, **kw: rev if state["built"] else rev,
    )

    result = update._refresh_rust_bins(source, force=True)
    assert result == "refreshed"
    assert len(cargo_called) == 1


def test_ac1_edge_force_installs_when_no_binary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE (b): --rust force=True installs even when no binary exists yet."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)
    state = {"built": False}
    post_bin = tmp_path / "cargo-home" / "bin" / "fno-agents"
    # First-install: no binary at gate time; cargo install creates it, so the
    # post-deploy verify finds and interrogates the freshly built bin.
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: post_bin if state["built"] else None)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "e" * 40)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: "e" * 40 if state["built"] else None)

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)

    result = update._refresh_rust_bins(source, force=True)
    assert result == "refreshed"


def test_ac1_edge_no_rust_flag_skips_refresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE (c): --no-rust -> _refresh_rust_bins never called."""
    repo = tmp_path / "repo"
    _init_git_repo_with_crate(repo)
    cli_src = _make_abi_source(repo)
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", tmp_path / "installed-rev")
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "source-path")
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    tripwire_called = []

    def _tripwire(source, *, force=False, dry_run=False):
        tripwire_called.append(True)
        return "refreshed"

    monkeypatch.setattr(update, "_refresh_rust_bins", _tripwire)
    monkeypatch.setattr(update.os, "execvp", lambda prog, args: None)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)

    runner.invoke(app, ["update", "--source", str(cli_src), "--no-rust"])
    assert tripwire_called == [], "--no-rust must prevent _refresh_rust_bins from being called"


def test_ac1_edge_dry_run_shows_both_would_run_lines(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE (d): --dry-run prints both 'Would run:' lines, executes neither."""
    repo = tmp_path / "repo"
    _head, crate_rev = _init_git_repo_with_crate(repo)
    cli_src = _make_abi_source(repo)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)
    monkeypatch.setattr(update, "_INSTALLED_REV_FILE", tmp_path / "installed-rev")
    monkeypatch.setattr(update, "_CACHE_FILE", tmp_path / "source-path")
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    # Provide a cargo binary so the rust leg does not short-circuit to skip
    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    # Stub _rust_subtree_rev directly so subprocess.run stub does not need stdout
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: crate_rev)

    def _fake_which(name):
        if name == "cargo":
            return "/usr/bin/cargo"
        if name == "uv":
            return "/usr/bin/uv"
        return None

    # Patch through update module so both _refresh_rust_bins and update_command see it
    monkeypatch.setattr(update.shutil, "which", _fake_which)

    cargo_actually_ran = []

    def _fake_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            cargo_actually_ran.append(cmd)
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    execvp_called = []
    monkeypatch.setattr(update.os, "execvp", lambda prog, args: execvp_called.append(prog))

    result = runner.invoke(app, ["update", "--source", str(cli_src), "--dry-run"])
    output = result.output
    # Both Would-run lines should appear
    would_run_count = output.count("Would run:")
    assert would_run_count >= 2, f"Expected >=2 'Would run:' lines, got {would_run_count}. Output:\n{output}"
    # Neither actually ran
    assert cargo_actually_ran == [], "cargo must not execute under --dry-run"
    assert execvp_called == [], "execvp must not execute under --dry-run"


def test_ac1_edge_rust_and_no_rust_together_exits_2(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE (e): --rust --no-rust together -> exit code 2."""
    repo = tmp_path / "repo"
    _init_git_repo_with_crate(repo)
    cli_src = _make_abi_source(repo)
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    result = runner.invoke(app, ["update", "--source", str(cli_src), "--rust", "--no-rust"])
    assert result.exit_code == 2


# --- AC1-FR: failed outcome preserves old marker; retry with rc-0 updates it ---

# ---------------------------------------------------------------------------
# Fix C3: _refresh_rust_bins pins --root to the tested binary's cargo root
# ---------------------------------------------------------------------------


def test_c3_hp_refresh_root_pinned_to_detected_binary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix C3-HP: when a cargo binary is detected, --root equals binary.parent.parent.

    cargo install --root <root> ensures the refresh lands in the same directory
    that _cargo_installed_bin() tested. Without --root, CARGO_INSTALL_ROOT can
    split the tested location from the install destination and the marker claims
    fresh while the tested binary stays stale.
    """
    source = tmp_path / "cli"
    source.mkdir()
    crate_dir = source.parent / "crates" / "fno-agents"
    crate_dir.mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    marker_file.write_text("oldrev\n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    # fake_bin simulates a binary installed under a non-default CARGO_HOME
    fake_root = tmp_path / "my-cargo"
    fake_bin = fake_root / "bin" / "fno-agents"
    fake_bin.parent.mkdir(parents=True)
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "a" * 40)

    recorded_calls: list[list[str]] = []
    state = {"built": False}

    def _fake_run(cmd, **kwargs):
        recorded_calls.append(list(cmd))
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: ("a" * 40) if state["built"] else None)

    result = update._refresh_rust_bins(source)
    assert result == "refreshed"

    cargo_calls = [c for c in recorded_calls if c and c[0] == "cargo"]
    assert len(cargo_calls) == 1
    cmd = cargo_calls[0]
    # --root must be present and equal to fake_bin.parent.parent
    assert "--root" in cmd
    root_idx = cmd.index("--root")
    assert cmd[root_idx + 1] == str(fake_root), (
        f"Expected --root={fake_root}, got {cmd[root_idx + 1]}"
    )


def test_c3_hp_root_equals_detected_bin_parent_parent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix C3-HP variant: --root == detected_bin.parent.parent, not a hardcoded path."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    marker_file.write_text("old\n", encoding="utf-8")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    # Place the binary three levels deep to test parent.parent
    fake_bin = tmp_path / "arbitrary" / "bin" / "fno-agents"
    fake_bin.parent.mkdir(parents=True)
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "b" * 40)

    recorded_cmds: list[list[str]] = []
    state = {"built": False}

    def _fake_run(cmd, **kwargs):
        recorded_cmds.append(list(cmd))
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: ("b" * 40) if state["built"] else None)

    update._refresh_rust_bins(source)
    cargo_calls = [c for c in recorded_cmds if c and c[0] == "cargo"]
    assert cargo_calls
    cmd = cargo_calls[0]
    assert "--root" in cmd
    root_idx = cmd.index("--root")
    expected_root = str(tmp_path / "arbitrary")
    assert cmd[root_idx + 1] == expected_root


def test_c3_edge_force_no_binary_uses_cargo_home_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix C3-EDGE: force=True with no detected binary -> --root equals CARGO_HOME default.

    This is the first-install case: detection returns None, so the install root
    must equal the same default path that _cargo_installed_bin() uses to probe,
    keeping detection and install location coherent.
    """
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)
    fake_cargo_home = tmp_path / "cargo-home"
    monkeypatch.setenv("CARGO_HOME", str(fake_cargo_home))
    state = {"built": False}
    post_bin = fake_cargo_home / "bin" / "fno-agents"
    # First-install: no binary at gate; cargo creates it -> post-deploy verify finds it.
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: post_bin if state["built"] else None)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "e" * 40)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: "e" * 40 if state["built"] else None)

    recorded_cmds: list[list[str]] = []

    def _fake_run(cmd, **kwargs):
        recorded_cmds.append(list(cmd))
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)

    result = update._refresh_rust_bins(source, force=True)
    assert result == "refreshed"

    cargo_calls = [c for c in recorded_cmds if c and c[0] == "cargo"]
    assert cargo_calls
    cmd = cargo_calls[0]
    assert "--root" in cmd
    root_idx = cmd.index("--root")
    assert cmd[root_idx + 1] == str(fake_cargo_home), (
        f"Expected --root={fake_cargo_home} (CARGO_HOME), got {cmd[root_idx + 1]}"
    )


def test_ac1_fr_failed_preserves_marker_retry_updates(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-FR: after 'failed', old marker content is intact; rc-0 retry updates it."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    marker_file = tmp_path / "installed-rust-rev"
    old_rev = "f" * 40
    marker_file.write_text(f"{old_rev}\n")
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", marker_file)

    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    new_rev = "g" * 40
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: new_rev)
    # binary reads stale until a cargo build succeeds, then reports new_rev.
    state = {"built": False}
    monkeypatch.setattr(
        update, "_installed_bin_crates_rev",
        lambda b, **kw: new_rev if state["built"] else None,
    )

    # First call: cargo fails
    def _fail_run(cmd, **kwargs):
        return types.SimpleNamespace(returncode=1)

    monkeypatch.setattr(update.subprocess, "run", _fail_run)
    result1 = update._refresh_rust_bins(source)
    assert result1 == "failed"
    assert marker_file.read_text(encoding="utf-8").strip() == old_rev

    # Second call: cargo succeeds
    def _ok_run(cmd, **kwargs):
        if cmd and cmd[0] == "cargo":
            state["built"] = True
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _ok_run)
    result2 = update._refresh_rust_bins(source)
    assert result2 == "refreshed"
    assert marker_file.read_text(encoding="utf-8").strip() == new_rev


# ---------------------------------------------------------------------------
# gate on the binary (version --json), triad sync, post-deploy verify
# ---------------------------------------------------------------------------


def _fake_version_run(stdout=None, *, returncode=0, raise_exc=None):
    """A subprocess.run stub emitting a canned `version --json` result."""
    def _run(cmd, **kwargs):
        if raise_exc is not None:
            raise raise_exc
        return types.SimpleNamespace(returncode=returncode, stdout=stdout)
    return _run


def test_installed_bin_crates_rev_fresh(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: a clean, non-dirty binary yields its crates_rev."""
    b = tmp_path / "fno-agents"
    b.write_text("x")
    monkeypatch.setattr(
        update.subprocess, "run",
        _fake_version_run('{"crates_rev": "abc123", "git_rev": "def456", "dirty": false}'),
    )
    assert update._installed_bin_crates_rev(b) == "abc123"


def test_installed_bin_crates_rev_dirty_is_stale(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Invariant: a dirty build never reads fresh (fail toward rebuild)."""
    b = tmp_path / "fno-agents"
    b.write_text("x")
    monkeypatch.setattr(
        update.subprocess, "run",
        _fake_version_run('{"crates_rev": "abc123", "dirty": true}'),
    )
    assert update._installed_bin_crates_rev(b) is None


def test_installed_bin_crates_rev_unknown_is_stale(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Boundary: crates_rev == "unknown" (non-git build) is stale, never fresh."""
    b = tmp_path / "fno-agents"
    b.write_text("x")
    monkeypatch.setattr(
        update.subprocess, "run",
        _fake_version_run('{"crates_rev": "unknown", "dirty": false}'),
    )
    assert update._installed_bin_crates_rev(b) is None


def test_installed_bin_crates_rev_timeout_is_stale(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR: a hung binary (timeout) classifies stale, never crashes update."""
    b = tmp_path / "fno-agents"
    b.write_text("x")
    monkeypatch.setattr(
        update.subprocess, "run",
        _fake_version_run(raise_exc=subprocess.TimeoutExpired(cmd="x", timeout=1)),
    )
    assert update._installed_bin_crates_rev(b) is None


def test_installed_bin_crates_rev_garbage_is_stale(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR: unparseable output is stale, never a crash."""
    b = tmp_path / "fno-agents"
    b.write_text("x")
    monkeypatch.setattr(update.subprocess, "run", _fake_version_run("not json at all"))
    assert update._installed_bin_crates_rev(b) is None


def test_installed_bin_crates_rev_nonzero_exit_is_stale(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A non-zero exit (old binary without the verb) is stale."""
    b = tmp_path / "fno-agents"
    b.write_text("x")
    monkeypatch.setattr(
        update.subprocess, "run",
        _fake_version_run('{"crates_rev": "abc"}', returncode=3),
    )
    assert update._installed_bin_crates_rev(b) is None


def test_post_deploy_verify_mismatch_halts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """AC2-HP / AC2-ERR: cargo exits 0 but the deployed binary still self-reports
    a stale crates_rev -> update HALTS (typer.Exit) naming both revs."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    subtree = "a" * 40
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)
    monkeypatch.setattr(update.subprocess, "run", lambda cmd, **kw: types.SimpleNamespace(returncode=0))
    # gate: stale (rebuild); verify: STILL stale (the deploy did not land).
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: "oldoldold")

    with pytest.raises(typer.Exit):
        update._refresh_rust_bins(source)
    err = capsys.readouterr().err
    assert "post-deploy verify FAILED" in err
    # A real rev mismatch IS the "did not land" case - keep naming both revs.
    assert "oldoldold" in err and "did not land" in err


def test_no_rev_reason_missing_binary(tmp_path: Path) -> None:
    assert "missing from the install root" in update._no_rev_reason(
        tmp_path / "absent", tmp_path
    )
    assert "missing from the install root" in update._no_rev_reason(None, tmp_path)


@pytest.mark.parametrize(
    "outcome, expected",
    [
        (types.SimpleNamespace(returncode=3, stdout=""), "exited 3"),
        (types.SimpleNamespace(returncode=0, stdout="{not json"), "unparseable"),
        (types.SimpleNamespace(returncode=0, stdout="[1, 2]"), "unexpected"),
        (
            types.SimpleNamespace(returncode=0, stdout='{"dirty": true}'),
            "dirty crates/",
        ),
        (
            types.SimpleNamespace(returncode=0, stdout='{"dirty": false}'),
            "no rev stamp",
        ),
    ],
)
def test_no_rev_reason_distinguishes_causes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, outcome, expected: str
) -> None:
    """The five causes that collapse into a None rev must not share a message:
    only the dirty-tree one is fixed by committing."""
    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update.subprocess, "run", lambda *a, **kw: outcome)
    assert expected in update._no_rev_reason(fake_bin, tmp_path)


def test_no_rev_reason_timeout(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")

    def _hang(*a, **kw):
        raise subprocess.TimeoutExpired(cmd="version", timeout=20.0)

    monkeypatch.setattr(update.subprocess, "run", _hang)
    assert "hung" in update._no_rev_reason(fake_bin, tmp_path)


def test_post_deploy_verify_no_rev_blames_dirty_not_install_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """A binary that reports no usable rev (dirty crates/ tree) must not be
    diagnosed as 'the rebuild did not land' - that sent three post-merge rituals
    hunting the install root while the deployed rev matched source exactly."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    fake_bin = tmp_path / "fake-fno-agents"
    fake_bin.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: "a" * 40)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)
    # cargo install succeeds; the probe then finds a dirty-tree build.
    monkeypatch.setattr(
        update.subprocess,
        "run",
        lambda cmd, **kw: types.SimpleNamespace(returncode=0, stdout='{"dirty": true}'),
    )
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: None)

    with pytest.raises(typer.Exit):
        update._refresh_rust_bins(source)
    err = capsys.readouterr().err
    assert "dirty crates/" in err
    assert "did not land" not in err


def test_sync_triad_copies_into_location_hosting_a_bin(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-FR: the whole triad is copied into a location already hosting one bin."""
    names = update._triad_names()
    cargo = tmp_path / "cargo" / "bin"
    cargo.mkdir(parents=True)
    for n in names:
        (cargo / n).write_text(f"new-{n}")
    dest = tmp_path / "local" / "bin"
    dest.mkdir(parents=True)
    (dest / names[0]).write_text("old-client")  # hosts one bin -> eligible
    monkeypatch.setattr(update, "_triad_install_dirs", lambda: [dest])

    update._sync_triad(cargo)
    for n in names:
        assert (dest / n).read_text() == f"new-{n}", n


def test_sync_triad_never_seeds_empty_location(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-EDGE: a location hosting none of the triad is never written to."""
    names = update._triad_names()
    cargo = tmp_path / "cargo" / "bin"
    cargo.mkdir(parents=True)
    for n in names:
        (cargo / n).write_text("x")
    empty = tmp_path / "empty"
    empty.mkdir()
    monkeypatch.setattr(update, "_triad_install_dirs", lambda: [empty])

    update._sync_triad(cargo)
    assert not any((empty / n).exists() for n in names)


def test_sync_triad_unwritable_location_halts_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """AC2-ERR: an unwritable live location halts update loud, naming the location."""
    names = update._triad_names()
    cargo = tmp_path / "cargo" / "bin"
    cargo.mkdir(parents=True)
    for n in names:
        (cargo / n).write_text("new")
    dest = tmp_path / "ro" / "bin"
    dest.mkdir(parents=True)
    (dest / names[0]).write_text("old")  # eligible, but content differs -> will copy
    monkeypatch.setattr(update, "_triad_install_dirs", lambda: [dest])

    def _boom(src, dst):
        raise OSError("read-only file system")

    monkeypatch.setattr(update.shutil, "copy2", _boom)
    with pytest.raises(typer.Exit):
        update._sync_triad(cargo)
    assert "triad sync FAILED" in capsys.readouterr().err


def test_sync_triad_cleans_temp_on_replace_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A copy2 that wrote the temp followed by a failing os.replace must not leave
    the .tmp orphaned in the destination dir (filesystem hygiene, matches the
    atomic-write cleanup in _write_rust_marker)."""
    names = update._triad_names()
    cargo = tmp_path / "cargo" / "bin"
    cargo.mkdir(parents=True)
    for n in names:
        (cargo / n).write_text("new")
    dest = tmp_path / "dest" / "bin"
    dest.mkdir(parents=True)
    (dest / names[0]).write_text("old")  # eligible; content differs -> will copy
    monkeypatch.setattr(update, "_triad_install_dirs", lambda: [dest])

    # copy2 runs for real (writes dest/.<name>.<pid>.tmp), then os.replace fails.
    def _replace_boom(src, dst):
        raise OSError("replace failed mid-swap")

    monkeypatch.setattr(update.os, "replace", _replace_boom)
    with pytest.raises(typer.Exit):
        update._sync_triad(cargo)
    leftover = [p.name for p in dest.iterdir() if p.name.endswith(".tmp")]
    assert leftover == [], f"temp file leaked: {leftover}"


def test_sync_triad_incomplete_cargo_root_is_noop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A cargo root missing one of the three bins propagates nothing (never a
    half-triad); no enumeration of install dirs occurs."""
    names = update._triad_names()
    cargo = tmp_path / "cargo" / "bin"
    cargo.mkdir(parents=True)
    (cargo / names[0]).write_text("only-client")  # daemon + worker absent
    called = {"enumerated": False}

    def _tripwire():
        called["enumerated"] = True
        return []

    monkeypatch.setattr(update, "_triad_install_dirs", _tripwire)
    update._sync_triad(cargo)
    assert called["enumerated"] is False


def test_sync_triad_skips_byte_identical_location(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fresh-path fast skip: a location already holding the identical triad is not
    rewritten (no copy2 call)."""
    names = update._triad_names()
    cargo = tmp_path / "cargo" / "bin"
    cargo.mkdir(parents=True)
    dest = tmp_path / "local" / "bin"
    dest.mkdir(parents=True)
    for n in names:
        (cargo / n).write_text("same")
        (dest / n).write_text("same")
    monkeypatch.setattr(update, "_triad_install_dirs", lambda: [dest])

    calls = []
    monkeypatch.setattr(update.shutil, "copy2", lambda s, d: calls.append((s, d)))
    update._sync_triad(cargo)
    assert calls == []


def test_fresh_path_missing_daemon_sibling_rebuilds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """P1: a fresh client whose same-dir daemon sibling is MISSING must NOT take
    the fresh fast path - that would skip the rebuild and leave the split (the
    exact DaemonBinMissing case this change repairs). It falls through to cargo,
    which reinstalls the full triad coherently."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    bindir = tmp_path / "cargo" / "bin"
    bindir.mkdir(parents=True)
    fake_bin = bindir / "fno-agents"
    fake_bin.write_text("x")
    (bindir / "fno-agents-worker").write_text("x")  # worker present, daemon ABSENT
    subtree = "a" * 40
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: fake_bin)
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree)
    monkeypatch.setattr(update.shutil, "which", lambda n: "/usr/bin/" + n)
    state = {"built": False}

    def _fake_run(cmd, **kw):
        if cmd and cmd[0] == "cargo":
            state["built"] = True
            for n in update._triad_names():
                (bindir / n).write_text("new")  # cargo writes the full triad
        return types.SimpleNamespace(returncode=0)

    monkeypatch.setattr(update.subprocess, "run", _fake_run)
    # Same-build semantics: a bin reports its rev only if present. The client +
    # worker report fresh; the vanished daemon returns None (real
    # _installed_bin_crates_rev on a missing binary), so _triad_same_build fails
    # and the gate rebuilds. After cargo writes the triad, verify sees fresh.
    monkeypatch.setattr(
        update, "_installed_bin_crates_rev",
        lambda b, **kw: subtree if Path(b).is_file() else None,
    )

    result = update._refresh_rust_bins(source)
    assert result == "refreshed", "a missing daemon must force a rebuild, not 'fresh'"
    assert state["built"] is True, "cargo must run to repair the split triad"


def test_fresh_path_complete_triad_short_circuits(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """P1 counterpart: a fresh client WITH both siblings present takes the fresh
    fast path (no cargo) - the presence check must not over-trigger a rebuild."""
    source = tmp_path / "cli"
    source.mkdir()
    (source.parent / "crates" / "fno-agents").mkdir(parents=True)
    monkeypatch.setattr(update, "_RUST_MARKER_FILE", tmp_path / "installed-rust-rev")
    bindir = tmp_path / "cargo" / "bin"
    bindir.mkdir(parents=True)
    subtree = "a" * 40
    for n in update._triad_names():
        (bindir / n).write_text("x")  # full triad present
    monkeypatch.setattr(update, "_cargo_installed_bin", lambda: bindir / "fno-agents")
    monkeypatch.setattr(update, "_rust_subtree_rev", lambda s: subtree)
    monkeypatch.setattr(update, "_installed_bin_crates_rev", lambda b, **kw: subtree)
    ran = []
    monkeypatch.setattr(
        update.subprocess, "run",
        lambda cmd, **kw: (ran.append(cmd), types.SimpleNamespace(returncode=0))[1],
    )
    result = update._refresh_rust_bins(source)
    assert result == "fresh"
    assert not [c for c in ran if c and c[0] == "cargo"], "complete fresh triad must skip cargo"


def test_install_then_mark_runs_every_refresh_even_when_one_fails(tmp_path: Path) -> None:
    """The refreshes are independent, so a wedged one must not skip the rest.

    They are `;`-separated rather than `&&`-chained for exactly this reason: a
    watcher that fails to refresh would otherwise leave every later agent
    pointing at the pre-update binary, which is the wedge the chain exists to
    repair.
    """
    import subprocess as _sp

    marker = tmp_path / "state" / "installed-rev"
    second = tmp_path / "second-refreshed"
    line = update._install_then_mark(
        ["true"], "feedface", marker=marker, pid=3,
        post_install=f"false; touch {second}",
    )
    result = _sp.run(["/bin/sh", "-c", line], check=False)

    assert second.exists(), "a failing refresh must not skip the ones after it"
    assert result.returncode == 0, "a refresh failure must not override a successful install"


def test_update_refreshes_the_groom_agent_too(monkeypatch) -> None:
    """Every launchd agent fno installs embeds an absolute binary path.

    Refreshing only pr-watch leaves the groom agent pointing at the pre-update
    entry point with no self-heal.
    """
    import inspect

    src = inspect.getsource(update.update_command)
    assert '"pr-watch", "refresh"' in src
    assert '"backlog", "groom", "--refresh-agent"' in src
