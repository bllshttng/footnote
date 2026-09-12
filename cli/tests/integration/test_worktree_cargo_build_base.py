"""Integration tests for the build-base half of `cleanup --cargo-targets`.

test_worktree_cleanup_merged.py covers the sweep's worktree half. These cover
the lanes this repo added with build.build-dir: the sharded hash dirs under
the build base (reap + protect), the unverifiable-metadata guard, and the
legacy offload-symlink refusal the retired verb's suite used to own.

Drives the real scripts/lib/worktree-lifecycle.sh against a throwaway git
repo with a bare origin, same fixture family as the merged-cleanup tests.
FNO_CARGO_TARGETS_BASE points the sweep at a sandbox so no real build dir is
ever touched.
"""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
LIFECYCLE_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-lifecycle.sh"
LIFECYCLE_COMPAT_SRC = REPO_ROOT / "scripts" / "worktree-lifecycle.sh"
UNPUSHED_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-unpushed.sh"
TARGET_GUARD_SRC = REPO_ROOT / "scripts" / "lib" / "target-guard.sh"
REMOVAL_EVENT_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-removal-event.sh"

CACHEDIR_TAG = "Signature: 8a477f597d28d172789f068868ba2775\n"
OLD_TS = 1_600_000_000


def _git(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", "-C", str(cwd), *args], check=check, capture_output=True, text=True
    )


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """A canonical checkout with an origin remote; scripts vendored in."""
    origin = tmp_path / "origin.git"
    canon = tmp_path / "canon"
    subprocess.run(
        ["git", "init", "--bare", "-b", "main", str(origin)], check=True, capture_output=True
    )
    subprocess.run(["git", "init", "-b", "main", str(canon)], check=True, capture_output=True)
    _git(canon, "config", "user.email", "t@t.com")
    _git(canon, "config", "user.name", "T")
    (canon / ".gitignore").write_text(".fno/\n")
    (canon / "README.md").write_text("# repo\n")
    _git(canon, "add", ".gitignore", "README.md")
    _git(canon, "commit", "-m", "init")
    _git(canon, "remote", "add", "origin", str(origin))
    _git(canon, "push", "-u", "origin", "main")
    _git(canon, "remote", "set-head", "origin", "main")
    (canon / "scripts" / "lib").mkdir(parents=True)
    shutil.copy2(LIFECYCLE_SRC, canon / "scripts" / "lib" / "worktree-lifecycle.sh")
    shutil.copy2(LIFECYCLE_COMPAT_SRC, canon / "scripts" / "worktree-lifecycle.sh")
    shutil.copy2(UNPUSHED_SRC, canon / "scripts" / "lib" / "worktree-unpushed.sh")
    shutil.copy2(TARGET_GUARD_SRC, canon / "scripts" / "lib" / "target-guard.sh")
    shutil.copy2(REMOVAL_EVENT_SRC, canon / "scripts" / "lib" / "worktree-removal-event.sh")
    return canon


@pytest.fixture
def base(tmp_path: Path) -> Path:
    b = tmp_path / "cargo-build-base"
    b.mkdir()
    return b


def _sweep(
    canon: Path, base: Path, *flags: str, env_extra: dict[str, str] | None = None
) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    env = os.environ.copy()
    env["FNO_CARGO_TARGETS_BASE"] = str(base)
    if env_extra:
        env.update(env_extra)
    return subprocess.run(
        ["bash", str(script), "cleanup", "--cargo-targets", *flags],
        cwd=str(canon),
        capture_output=True,
        text=True,
        env=env,
    )


def _plant_hash(base: Path, tag: str, size: int = 4096) -> Path:
    """A realistic build-dir hash dir: sharded two deep, CACHEDIR.TAG on top."""
    hash_dir = base / "bd" / "ef" / f"bac4721f2d16ec{tag}"
    (hash_dir / "debug" / "deps").mkdir(parents=True)
    (hash_dir / "CACHEDIR.TAG").write_text(CACHEDIR_TAG)
    payload = hash_dir / "debug" / "deps" / "probe-0123456789abcdef"
    payload.write_bytes(b"x" * size)
    os.utime(hash_dir, (OLD_TS, OLD_TS))
    return hash_dir


def _make_live(tree: Path) -> None:
    """A target-state.md whose owner_pid is this test: _wt_live reads it live."""
    fno = tree / ".fno"
    fno.mkdir(exist_ok=True)
    (fno / "target-state.md").write_text(f"owner_pid: {os.getpid()}\n")


# -- the build-base lane reaps an unowned tagged hash dir ----------------------


def test_build_base_hash_dir_dry_runs_then_reaps(repo: Path, base: Path):
    hash_dir = _plant_hash(base, "one")

    dry = _sweep(repo, base, "--cap-bytes", str(64 * 1024 * 1024), "--target-max-age", "0d")
    assert dry.returncode == 0, dry.stderr
    assert "would-reap" in dry.stdout, "an unowned tagged hash dir is a reap candidate"
    assert f"path={hash_dir}" in dry.stdout
    assert hash_dir.exists(), "dry run must delete nothing"

    applied = _sweep(
        repo, base, "--cap-bytes", str(64 * 1024 * 1024), "--target-max-age", "0d", "--apply"
    )
    assert applied.returncode == 0, applied.stderr
    assert "reason=age" in applied.stdout
    assert not hash_dir.exists(), "the sweep deletes the resolved hash dir"


# -- an unreadable cargo metadata protects EVERY build-base dir ----------------


def test_unverifiable_metadata_protects_every_build_base_dir(repo: Path, base: Path):
    hash_dir = _plant_hash(base, "guard")
    _make_live(repo)
    broken = repo / "crates" / "broken"
    broken.mkdir(parents=True)
    (broken / "Cargo.toml").write_text("not [valid toml")

    applied = _sweep(
        repo, base, "--cap-bytes", str(64 * 1024 * 1024), "--target-max-age", "0d", "--apply"
    )

    assert applied.returncode == 0, applied.stderr
    assert "reason=build-dir-unverifiable" in applied.stdout, applied.stdout
    assert hash_dir.exists(), "a blind sweep is the one mistake this lane cannot undo"


# -- a live workspace's resolved build_directory is protected ------------------


def test_live_workspace_build_dir_is_protected(repo: Path, base: Path, tmp_path: Path):
    hash_dir = _plant_hash(base, "live")
    _make_live(repo)
    crates = repo / "crates" / "fake"
    crates.mkdir(parents=True)
    (crates / "Cargo.toml").write_text("[package]\nname = 'fake'\nversion = '0.1.0'\n")
    # A cargo stub whose metadata answers the planted hash dir, so the sweep's
    # live-workspace resolution is deterministic without cargo 1.91 semantics.
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    cargo = fake_bin / "cargo"
    cargo.write_text(f"#!/bin/sh\nprintf '{{\"build_directory\":\"{hash_dir}\"}}\\n'\n")
    cargo.chmod(cargo.stat().st_mode | 0o111)

    applied = _sweep(
        repo,
        base,
        "--cap-bytes",
        str(64 * 1024 * 1024),
        "--target-max-age",
        "0d",
        "--apply",
        env_extra={"PATH": f"{fake_bin}:{os.environ['PATH']}"},
    )

    assert applied.returncode == 0, applied.stderr
    assert "reason=live-workspace-build-dir" in applied.stdout, applied.stdout
    assert hash_dir.exists(), "a live workspace keeps its build dir"


# -- a legacy offload link outside both bases is never followed with rm --------


def test_link_outside_both_bases_is_never_deleted(repo: Path, base: Path, tmp_path: Path):
    outside = tmp_path / "outside-tagged"
    outside.mkdir()
    (outside / "CACHEDIR.TAG").write_text(CACHEDIR_TAG)
    (outside / "artifact.bin").write_bytes(b"x" * 1024)
    wt = repo / "wt-link"
    _git(repo, "worktree", "add", str(wt), "-b", "feature/wt-link", "main")
    link = wt / "crates" / "fixture" / "target"
    link.parent.mkdir(parents=True)
    link.symlink_to(outside, target_is_directory=True)

    applied = _sweep(
        repo, base, "--cap-bytes", str(64 * 1024 * 1024), "--target-max-age", "0d", "--apply"
    )

    assert applied.returncode == 0, applied.stderr
    assert "reason=link-not-owned" in applied.stdout, applied.stdout
    assert outside.exists(), "a tagged dir outside both bases must not be deleted"
    assert (outside / "artifact.bin").exists()
    assert link.is_symlink(), "the link itself stays"
