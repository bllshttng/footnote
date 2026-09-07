"""Integration tests for `worktree cargo-offload` (node x-f96e, task 1.3).

Drives the real scripts/lib/worktree-lifecycle.sh against a throwaway git
repo with a bare origin, same fixture family as test_worktree_cleanup_merged.
FNO_CARGO_TARGETS_BASE points both the offload and the sweep at a sandbox so
no real cache is ever moved.
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


def _git(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(cwd), *args], check=check, capture_output=True, text=True)


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """A canonical checkout with an origin remote; scripts vendored in."""
    origin = tmp_path / "origin.git"
    canon = tmp_path / "canon"
    subprocess.run(["git", "init", "--bare", "-b", "main", str(origin)], check=True, capture_output=True)
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
    b = tmp_path / "cargo-offload-base"
    b.mkdir()
    return b


def _offload(canon: Path, base: Path, *flags: str) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    env = os.environ.copy()
    env["FNO_CARGO_TARGETS_BASE"] = str(base)
    return subprocess.run(
        ["bash", str(script), "cargo-offload", *flags],
        cwd=str(canon), capture_output=True, text=True, env=env,
    )


def _sweep(canon: Path, base: Path, *flags: str, env_extra: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    env = os.environ.copy()
    env["FNO_CARGO_TARGETS_BASE"] = str(base)
    if env_extra:
        env.update(env_extra)
    return subprocess.run(
        ["bash", str(script), "cleanup", "--cargo-targets", *flags],
        cwd=str(canon), capture_output=True, text=True, env=env,
    )


def _add_target(canon: Path, name: str, size: int, *, old: bool = False) -> Path:
    """A realistic cargo target dir: CACHEDIR.TAG present, as cargo writes it."""
    wt = canon / name
    _git(canon, "worktree", "add", str(wt), "-b", f"feature/{name}", "main")
    target = wt / "crates" / "fixture" / "target"
    target.mkdir(parents=True)
    (target / "artifact.bin").write_bytes(b"x" * size)
    (target / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f068868ba2775\n")
    if old:
        old_ts = 1_600_000_000
        os.utime(target / "artifact.bin", (old_ts, old_ts))
        os.utime(target, (old_ts, old_ts))
    return target


def _add_canonical_target(canon: Path, size: int) -> Path:
    target = canon / "crates" / "fixture" / "target"
    target.mkdir(parents=True)
    (target / "artifact.bin").write_bytes(b"x" * size)
    (target / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f068868ba2775\n")
    return target


# -- AC7-HP: dry run reports and moves nothing --------------------------------


def test_dry_run_prints_bytes_and_dest_for_every_target(repo: Path, base: Path):
    target = _add_target(repo, "off-dry", 2 * 1024 * 1024)
    canon_target = _add_canonical_target(repo, 1024 * 1024)

    r = _offload(repo, base)

    assert r.returncode == 0, r.stderr
    assert "mode=dry-run" in r.stdout
    for path in (target, canon_target):
        line = next(l for l in r.stdout.splitlines() if f"path={path}" in l)
        assert line.startswith("cargo-offload would-move bytes="), line
        assert str(base) in line
    assert target.is_dir() and not target.is_symlink(), "dry run must move nothing"
    assert canon_target.is_dir() and not canon_target.is_symlink()


# -- AC8-HP: apply relocates with a symlink, one dest per tree ----------------


def test_apply_moves_canonical_and_worktree_caches_to_per_tree_dests(repo: Path, base: Path):
    target = _add_target(repo, "off-live-tree", 2 * 1024 * 1024)
    canon_target = _add_canonical_target(repo, 1024 * 1024)

    r = _offload(repo, base, "--apply")

    assert r.returncode == 0, r.stderr
    assert "mode=apply" in r.stdout
    assert "moved=2" in r.stdout
    wt_dest = (base / repo.name / "off-live-tree" / "fixture").resolve()
    canon_dest = (base / repo.name / "canonical" / "fixture").resolve()
    assert wt_dest != canon_dest, "no two trees may share a destination"
    for old_path, dest in ((target, wt_dest), (canon_target, canon_dest)):
        assert old_path.is_symlink(), f"{old_path} must be a symlink after offload"
        assert (dest / "artifact.bin").exists(), "the bytes must be at the destination"
        # A write through the link lands in the destination: built-binary
        # paths under the old location keep working.
        (old_path / "through-link.bin").write_text("ok\n")
        assert (dest / "through-link.bin").exists()


def test_apply_is_idempotent(repo: Path, base: Path):
    target = _add_target(repo, "off-idem", 1024 * 1024)

    first = _offload(repo, base, "--apply")
    second = _offload(repo, base, "--apply")

    assert first.returncode == 0, first.stderr
    assert "moved=1" in first.stdout
    assert second.returncode == 0, second.stderr
    assert "moved=0" in second.stdout
    assert "already-offloaded=1" in second.stdout
    assert target.is_symlink()


# -- AC9-EDGE: a live process holds its tree ----------------------------------


def test_apply_skips_tree_with_a_live_process(repo: Path, base: Path):
    target = _add_target(repo, "off-proc", 2 * 1024 * 1024)
    wt = target.parents[2]
    holder = subprocess.Popen(["sleep", "10"], cwd=str(wt))
    try:
        r = _offload(repo, base, "--apply")
    finally:
        holder.terminate()
        holder.wait(timeout=5)

    assert r.returncode == 0, r.stderr
    line = next(l for l in r.stdout.splitlines() if f"path={target}" in l)
    assert line.startswith("cargo-offload protected bytes=") and "reason=processes:" in line
    assert target.is_dir() and not target.is_symlink(), "a live tree keeps its cache in place"


# -- AC10-EDGE: source dirs named target are never touched ---------------------


def test_source_dirs_named_target_are_untouched_and_unnamed(repo: Path, base: Path):
    target = _add_target(repo, "off-src", 1024 * 1024)
    wt = target.parents[2]
    source_dirs = []
    for rel in ("skills/target", "tests/target", "cli/src/fno/target"):
        d = wt / rel
        d.mkdir(parents=True)
        (d / "real_source.py").write_text("x = 1\n")
        source_dirs.append(d)

    r = _offload(repo, base, "--apply")

    assert r.returncode == 0, r.stderr
    for d in source_dirs:
        assert d.is_dir() and not d.is_symlink(), f"{d} is source, never cargo output"
        assert (d / "real_source.py").exists()
        assert str(d) not in r.stdout, "a source dir must not appear in offload output"


# -- AC11-HP: the sweep follows the relocated symlink --------------------------


def test_sweep_counts_and_reaps_relocated_caches(repo: Path, base: Path):
    target = _add_target(repo, "off-swept", 2 * 1024 * 1024)
    old_ts = 1_600_000_000
    off = _offload(repo, base, "--apply")
    assert off.returncode == 0, off.stderr
    dest = (base / repo.name / "off-swept" / "fixture").resolve()
    os.utime(dest / "artifact.bin", (old_ts, old_ts))
    os.utime(dest, (old_ts, old_ts))

    dry = _sweep(repo, base, "--cap-bytes", str(64 * 1024 * 1024), "--target-max-age", "0d")
    assert dry.returncode == 0, dry.stderr
    assert "would-reap" in dry.stdout, "a process-free relocated cache must be a reap candidate"
    assert f"path={target}" in dry.stdout

    applied = _sweep(
        repo, base, "--cap-bytes", str(64 * 1024 * 1024), "--target-max-age", "0d", "--apply"
    )
    assert applied.returncode == 0, applied.stderr
    assert "reason=age" in applied.stdout
    assert not dest.exists(), "the sweep deletes the RESOLVED directory"
    assert not target.exists(), "and then the link itself"


# -- AC12-EDGE: a link outside the base or without the tag is refused ----------


def test_sweep_refuses_links_outside_base_or_without_cachedir_tag(repo: Path, base: Path, tmp_path: Path):
    target = _add_target(repo, "off-guard", 1024, old=True)
    wt = target.parents[2]

    # Two hostile links in the crates/*/target shape: one resolving to a
    # TAGGED dir OUTSIDE the base, one to an UNTAGGED dir inside it.
    tagged_outside = tmp_path / "tagged-outside"
    tagged_outside.mkdir()
    (tagged_outside / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f068868ba2775\n")
    (tagged_outside / "artifact.bin").write_bytes(b"x" * 1024)
    untagged_inside = base / "untagged-inside"
    untagged_inside.mkdir()
    (untagged_inside / "artifact.bin").write_bytes(b"x" * 1024)
    shutil.rmtree(target)
    target.symlink_to(tagged_outside, target_is_directory=True)
    second = wt / "crates" / "other" / "target"
    second.parent.mkdir(parents=True)
    second.symlink_to(untagged_inside, target_is_directory=True)

    r = _sweep(repo, base, "--cap-bytes", "1", "--target-max-age", "0d", "--apply")

    assert tagged_outside.exists(), "a link outside the base must not be followed with rm"
    assert (tagged_outside / "artifact.bin").exists()
    assert untagged_inside.exists(), "a dest without CACHEDIR.TAG must not be deleted"
    assert target.is_symlink() and second.is_symlink(), "the links stay"
    assert r.stdout.count("reason=link-not-owned") == 2, "both refusals are reported"
