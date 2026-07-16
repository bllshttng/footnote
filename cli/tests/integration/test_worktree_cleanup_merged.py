"""Integration tests for `worktree cleanup --merged` (node x-2380).

Drives the real `scripts/lib/worktree-lifecycle.sh` + `scripts/setup/archive-worktree.sh`
against a throwaway git repo with a bare `origin`, so no real worktree is touched.
The two scripts are copied into the fixture so the sweep's hardcoded
`$MAIN_DIR/scripts/setup/archive-worktree.sh` path resolves to the real code.
"""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
LIFECYCLE_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-lifecycle.sh"
ARCHIVE_SRC = REPO_ROOT / "scripts" / "setup" / "archive-worktree.sh"


def _git(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(cwd), *args], check=check, capture_output=True, text=True)


def _commit(wt: Path, name: str, body: str = "x") -> None:
    (wt / name).write_text(body)
    _git(wt, "add", name)
    _git(wt, "commit", "-m", f"add {name}")


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """A canonical checkout with an `origin` bare remote and a `main` branch.

    `.fno/` is gitignored (as in the real repo) so the symlink family never
    counts as a dirty tree.
    """
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
    # Vendor the scripts under test into the fixture so ARCHIVE path resolves.
    (canon / "scripts" / "lib").mkdir(parents=True)
    (canon / "scripts" / "setup").mkdir(parents=True)
    shutil.copy2(LIFECYCLE_SRC, canon / "scripts" / "lib" / "worktree-lifecycle.sh")
    shutil.copy2(ARCHIVE_SRC, canon / "scripts" / "setup" / "archive-worktree.sh")
    return canon


def _sweep(canon: Path, *flags: str) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    return subprocess.run(
        ["bash", str(script), "cleanup", "--merged", *flags],
        cwd=str(canon), capture_output=True, text=True,
    )


def _add_merged(canon: Path, name: str) -> Path:
    """Worktree whose branch is merged into origin/main (a reap candidate)."""
    wt = canon / name
    _git(canon, "worktree", "add", str(wt), "-b", f"feature/{name}", "main")
    _commit(wt, f"{name}.txt")
    _git(canon, "merge", "--no-ff", f"feature/{name}", "-m", f"merge {name}")
    _git(canon, "push", "origin", "main")
    return wt


# ── AC1-UI: dry-run is the default and mutates nothing ──────────────────────
def test_dry_run_default_mutates_nothing(repo: Path):
    wt = _add_merged(repo, "reapme")
    before = _git(repo, "worktree", "list").stdout

    r = _sweep(repo)  # no --apply

    assert r.returncode == 0, r.stderr
    assert "would-archive" in r.stdout
    assert "dry-run" in r.stdout
    assert wt.exists(), "dry-run must not remove the worktree"
    assert _git(repo, "worktree", "list").stdout == before


# ── AC1-HP + branch preservation: --apply reaps, branch survives ────────────
def test_apply_reaps_merged_and_preserves_branch(repo: Path):
    wt = _add_merged(repo, "reapme")

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert r.returncode == 0, diag
    assert "1 archived" in r.stdout, diag
    assert not wt.exists(), "worktree dir should be gone" + diag
    branches = _git(repo, "branch", "--list", "feature/reapme").stdout
    assert "feature/reapme" in branches, "branch must be preserved"


# ── AC2-HP: the four keep-reasons hold, none removed ────────────────────────
def test_keep_reasons(repo: Path):
    # dirty: merged tip but an untracked file
    dirty = repo / "wt-dirty"
    _git(repo, "worktree", "add", str(dirty), "-b", "feature/dirty", "main")
    (dirty / "scratch.txt").write_text("uncommitted")

    # unmerged: pushed to its own remote branch, not in main
    unmerged = repo / "wt-unmerged"
    _git(repo, "worktree", "add", str(unmerged), "-b", "feature/unmerged", "main")
    _commit(unmerged, "u.txt")
    _git(unmerged, "push", "-u", "origin", "feature/unmerged")

    # unpushed: local commit, no upstream
    unpushed = repo / "wt-unpushed"
    _git(repo, "worktree", "add", str(unpushed), "-b", "feature/unpushed", "main")
    _commit(unpushed, "p.txt")

    # live: merged tip but a live owner_pid in the manifest
    live = _add_merged(repo, "wt-live")
    (live / ".fno").mkdir()
    (live / ".fno" / "target-state.md").write_text(
        f"owner_pid: {os.getpid()}\ngraph_node_id: x-live\n"
    )

    r = _sweep(repo, "--apply")

    assert r.returncode == 0, r.stderr
    assert "kept (dirty)" in r.stdout
    assert "kept (unmerged)" in r.stdout
    assert "kept (unpushed)" in r.stdout
    assert "kept (live-session)" in r.stdout
    assert "0 archived" in r.stdout  # nothing reaped
    for wt in (dirty, unmerged, unpushed, live):
        assert wt.exists(), f"{wt} must not be removed"


# ── AC1-ERR: fetch failure aborts loudly, nothing removed ───────────────────
def test_fetch_failure_aborts(repo: Path):
    wt = _add_merged(repo, "reapme")
    _git(repo, "remote", "set-url", "origin", str(repo / "does-not-exist.git"))

    r = _sweep(repo, "--apply")

    assert r.returncode != 0
    assert "aborting" in r.stderr
    assert wt.exists(), "no worktree may be removed after a fetch abort"


# ── AC2-EDGE: local-only .fno state survives the reap ───────────────────────
def test_salvage_preserves_local_state(repo: Path):
    (repo / ".fno").mkdir()
    (repo / ".fno" / "config.toml").write_text("canonical\n")

    wt = _add_merged(repo, "reapme")
    fno = wt / ".fno"
    fno.mkdir()
    (fno / "target-state.md").write_text("graph_node_id: x-salv\nowner_pid: 999999\n")
    (fno / "events.jsonl").write_text("EVT\n")
    (fno / "scratchpad").mkdir()
    (fno / "scratchpad" / "note.md").write_text("SCRATCH\n")
    # a symlink into canonical — must NOT be salvaged
    (fno / "config.toml").symlink_to(repo / ".fno" / "config.toml")

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"
    assert r.returncode == 0, diag
    assert "1 archived" in r.stdout, diag
    assert not wt.exists(), diag

    canon_fno = repo / ".fno"
    # loose file -> salvage/<date>-<node>/events.jsonl
    evt = list(canon_fno.glob("salvage/*-x-salv/events.jsonl"))
    assert evt and evt[0].read_text() == "EVT\n", "events.jsonl not salvaged"
    # directory -> scratchpad/<date>-<node>/note.md
    note = list(canon_fno.glob("scratchpad/*-x-salv/note.md"))
    assert note and note[0].read_text() == "SCRATCH\n", "scratchpad/ not salvaged"
    # the symlinked config.toml must not have been copied into a date-node dir
    assert not list(canon_fno.glob("config.toml/*")), "symlink was wrongly salvaged"


# ── AC2-FR: salvage failure blocks removal ──────────────────────────────────
def test_salvage_failure_keeps_worktree(repo: Path):
    canon_fno = repo / ".fno"
    canon_fno.mkdir()
    wt = _add_merged(repo, "reapme")
    fno = wt / ".fno"
    fno.mkdir()
    (fno / "events.jsonl").write_text("EVT\n")
    os.chmod(canon_fno, 0o500)  # unwritable canonical .fno
    try:
        r = _sweep(repo, "--apply")
        assert r.returncode == 0, r.stderr
        assert "salvage-failed" in r.stdout
        assert wt.exists(), "worktree must be kept when salvage fails"
    finally:
        os.chmod(canon_fno, 0o700)


# ── --prefix scopes the merged sweep (never touches out-of-prefix branches) ─
def test_prefix_scopes_merged(repo: Path):
    keep = _add_merged(repo, "keepme")   # feature/keepme
    drop = _add_merged(repo, "dropme")   # feature/dropme

    r = _sweep(repo, "--apply", "--prefix", "feature/keep")

    assert r.returncode == 0, r.stderr
    assert not keep.exists(), "in-prefix merged worktree should be archived"
    assert drop.exists(), "out-of-prefix worktree must be untouched"
    assert "1 archived" in r.stdout  # only the prefixed one counted


# ── an explicit --dry-run wins over --apply (safety wrappers) ───────────────
def test_dry_run_overrides_apply(repo: Path):
    wt = _add_merged(repo, "reapme")

    r = _sweep(repo, "--apply", "--dry-run")

    assert r.returncode == 0, r.stderr
    assert "would-archive" in r.stdout
    assert "dry-run" in r.stdout
    assert wt.exists(), "--dry-run must veto --apply"


# ── silent-failure guard: empty-state line is explicit, not silence ─────────
def test_empty_state_is_explicit(repo: Path):
    r = _sweep(repo)
    assert r.returncode == 0, r.stderr
    assert "No non-canonical worktrees found." in r.stdout
