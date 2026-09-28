"""Integration tests for the build-base delegation of `cleanup --cargo-targets`.

The build-base half of the sweep is the Rust lane now
(`fno-agents reclaim cargo-build-dirs`, crates/fno-agents/src/cargo_build_dirs.rs);
its classification cases live in that module's Rust tests. These cover the
bash side of the contract: the delegate prints the binary's lines and skips
without error when no binary resolves, and the in-checkout lanes keep their
legacy offload-symlink refusal (the retired verb's suite used to own it).

Drives the real scripts/lib/worktree-lifecycle.sh against a throwaway git
repo with a bare origin, same fixture family as the merged-cleanup tests.
FNO_CARGO_TARGETS_BASE points the sweep at a sandbox so no real build dir is
ever touched; FNO_AGENTS_BIN pins the delegate so no ambient binary answers.
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


STUB_SUMMARY = (
    "cargo-build-dirs mode=dry-run bases=1 rows=1 trees_resolved=1 orphans=0 "
    "reaped=0 reclaimed_bytes=0 after_bytes=0 effective_cap_bytes=0 orphan_lane=on shards_removed=0"
)


def _plant_stub(tmp_path: Path, extra: str = "") -> Path:
    """A stub fno-agents whose cargo-build-dirs answers deterministically."""
    stub = tmp_path / "bin" / "fno-agents"
    stub.parent.mkdir(exist_ok=True)
    stub.write_text(f"#!/bin/sh\nprintf '%s\\n' '{STUB_SUMMARY}'\n{extra}")
    stub.chmod(stub.stat().st_mode | 0o111)
    return stub


def _sweep(
    canon: Path, base: Path, *flags: str, env_extra: dict[str, str] | None = None
) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    env = os.environ.copy()
    env["FNO_CARGO_TARGETS_BASE"] = str(base)
    # Pin the delegate unless a test overrides it: no ambient fno-agents may
    # answer from a developer's PATH.
    env.setdefault("FNO_AGENTS_BIN", "/nonexistent/fno-agents")
    if env_extra:
        env.update(env_extra)
    return subprocess.run(
        ["bash", str(script), "cleanup", "--cargo-targets", *flags],
        cwd=str(canon),
        capture_output=True,
        text=True,
        env=env,
    )


# -- the build-base rows ride the Rust lane ------------------------------------


def test_delegate_prints_the_binary_s_cargo_build_dirs_lines(repo: Path, base: Path, tmp_path: Path):
    """AC9: the sweep prints a stub binary's cargo-build-dirs line, and the
    --apply flag reaches it. No bash code walks the build base anymore."""
    stub = _plant_stub(
        tmp_path,
        'case "$*" in *--apply*) printf \'STUB-SAW-APPLY\\n\' ;; esac\n',
    )

    dry = _sweep(repo, base, env_extra={"FNO_AGENTS_BIN": str(stub)})
    assert dry.returncode == 0, dry.stderr
    assert STUB_SUMMARY in dry.stdout, dry.stdout
    assert "STUB-SAW-APPLY" not in dry.stdout

    applied = _sweep(repo, base, "--apply", env_extra={"FNO_AGENTS_BIN": str(stub)})
    assert applied.returncode == 0, applied.stderr
    assert "STUB-SAW-APPLY" in applied.stdout, applied.stdout


def test_missing_binary_skips_the_build_base_without_error(repo: Path, base: Path):
    """A partial deploy (no fno-agents resolvable) skips the lane and still
    exits 0: the in-checkout half is untouched. The system PATH is dropped so
    a developer's ambient fno-agents cannot answer either."""
    applied = _sweep(
        repo,
        base,
        "--apply",
        env_extra={
            "FNO_AGENTS_BIN": "/nonexistent/fno-agents",
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        },
    )
    assert applied.returncode == 0, applied.stderr
    assert "cargo-target build-base skipped reason=fno-agents-missing" in applied.stdout


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
