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
LIFECYCLE_COMPAT_SRC = REPO_ROOT / "scripts" / "worktree-lifecycle.sh"
UNPUSHED_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-unpushed.sh"
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
    shutil.copy2(LIFECYCLE_COMPAT_SRC, canon / "scripts" / "worktree-lifecycle.sh")
    shutil.copy2(UNPUSHED_SRC, canon / "scripts" / "lib" / "worktree-unpushed.sh")
    shutil.copy2(ARCHIVE_SRC, canon / "scripts" / "setup" / "archive-worktree.sh")
    return canon


def _sweep(canon: Path, *flags: str) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    return subprocess.run(
        ["bash", str(script), "cleanup", "--merged", *flags],
        cwd=str(canon), capture_output=True, text=True,
    )


def _age_sweep(canon: Path, *flags: str) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    return subprocess.run(
        ["bash", str(script), "cleanup", "--older-than", "0d", *flags],
        cwd=str(canon), capture_output=True, text=True,
    )


def _compat_age_sweep(canon: Path, *flags: str) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "worktree-lifecycle.sh"
    return subprocess.run(
        ["bash", str(script), "cleanup", "--older-than", "0d", *flags],
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


def _add_merged_at(canon: Path, wt: Path, name: str) -> Path:
    wt.parent.mkdir(parents=True, exist_ok=True)
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


# ── archive-worktree.sh must not false-match its own process tree ───────────
# Regression for the CI-only failure: pgrep -f matched the script's own forks
# (TARGET is argv[1], so the command-substitution subshells carry it), and the
# headless /dev/tty prompt then declined with exit 3. Invoke the script
# directly (no sweep) with a detached stdin to reproduce the exact path.
def test_archive_script_excludes_own_process_tree(repo: Path):
    wt = _add_merged(repo, "reapme")
    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )
    assert r.returncode == 0, f"stdout={r.stdout}\nstderr={r.stderr}"
    assert not wt.exists(), f"stderr={r.stderr}"


def test_archive_refuses_codex_app_owned_worktree_even_with_force(
    repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    codex_home = tmp_path / ".codex"
    wt = codex_home / "worktrees" / "thread-a" / repo.name
    wt.parent.mkdir(parents=True)
    _git(repo, "worktree", "add", "--detach", str(wt), "main")
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    script = repo / "scripts" / "setup" / "archive-worktree.sh"

    r = subprocess.run(
        ["bash", str(script), "--force", str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )

    assert r.returncode == 6
    assert "app-owned Codex worktree" in r.stderr
    assert "archive its associated chat" in r.stderr
    assert wt.exists()


def test_merged_sweep_keeps_codex_app_owned_worktree(
    repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    codex_home = tmp_path / ".codex"
    wt = _add_merged_at(
        repo, codex_home / "worktrees" / "thread-b" / repo.name, "native"
    )
    monkeypatch.setenv("CODEX_HOME", str(codex_home))

    r = _sweep(repo, "--apply")
    diag = f"stdout={r.stdout}\nstderr={r.stderr}"

    assert r.returncode == 0, diag
    assert "kept (app-owned)" in r.stdout, diag
    assert "1 app-owned" in r.stdout, diag
    assert wt.exists()


def test_age_sweep_keeps_codex_app_owned_worktree(
    repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    codex_home = tmp_path / ".codex"
    wt = codex_home / "worktrees" / "thread-c" / repo.name
    wt.parent.mkdir(parents=True)
    _git(repo, "worktree", "add", "--detach", str(wt), "main")
    monkeypatch.setenv("CODEX_HOME", str(codex_home))

    r = _age_sweep(repo)

    assert r.returncode == 0, r.stderr
    assert f"SKIP: {wt} (app-owned Codex worktree)" in r.stdout
    assert wt.exists()


def test_age_sweep_keeps_detached_tree_with_unpushed_commits(repo: Path):
    """The age sweep removes by default and with --force, and a detached tree
    has no preserved branch to fall back on: force-removing it destroys any
    commit no remote carries. The same wt_unpushed_count guard the merged
    sweep uses must hold on this path too."""
    wt = _add_detached(repo, repo / "wt-age-unpushed")
    _commit(wt, "age.txt")

    r = _age_sweep(repo)

    assert r.returncode == 0, r.stderr
    assert f"SKIP: {wt} (detached HEAD holds unpushed commits)" in r.stdout
    assert wt.exists(), "age sweep must not force-remove unique commits on a detached HEAD"


def test_age_sweep_removes_old_clean_detached_tree(repo: Path):
    wt = _add_detached(repo, repo / "wt-age-clean")

    r = _age_sweep(repo)

    assert r.returncode == 0, r.stderr
    assert not wt.exists(), r.stdout
    assert "REMOVED" in r.stdout


def test_age_sweep_keeps_detached_tree_with_uncommitted_work(repo: Path):
    """Same loss class as unpushed commits, one step earlier: a detached tree
    has no branch holding uncommitted content either, and the sweep removes by
    default with --force. The in-flight eval tree rides this guard via its
    untracked marker file (cli/src/fno/evals/runner.py)."""
    wt = _add_detached(repo, repo / "wt-age-dirty")
    (wt / "scratch.txt").write_text("not on any ref\n")

    r = _age_sweep(repo)

    assert r.returncode == 0, r.stderr
    assert f"SKIP: {wt} (detached HEAD holds uncommitted work" in r.stdout
    assert wt.exists(), "age sweep must not force-remove uncommitted work on a detached HEAD"


def test_compat_age_sweep_delegates_app_owned_guard(
    repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    codex_home = tmp_path / ".codex"
    wt = codex_home / "worktrees" / "thread-d" / repo.name
    wt.parent.mkdir(parents=True)
    _git(repo, "worktree", "add", "--detach", str(wt), "main")
    monkeypatch.setenv("CODEX_HOME", str(codex_home))

    r = _compat_age_sweep(repo)

    assert r.returncode == 0, r.stderr
    assert f"SKIP: {wt} (app-owned Codex worktree)" in r.stdout
    assert wt.exists()


# ── detached scratch trees: classified by content, not branch name ──────────
def _add_detached(repo: Path, wt: Path, at: str = "main") -> Path:
    wt.parent.mkdir(parents=True, exist_ok=True)
    _git(repo, "worktree", "add", "--detach", str(wt), at)
    return wt


def test_detached_at_remote_tip_is_reap_candidate(repo: Path):
    """A scratch tree is detached BY CONSTRUCTION; a head that some remote
    already carries is recreatable, so the sweep must offer it as a candidate
    instead of keeping it on the branch-name proxy."""
    wt = _add_detached(repo, repo / "wt-scratch-clean")

    r = _sweep(repo)

    assert r.returncode == 0, r.stderr
    assert any(
        "would-archive" in line and str(wt) in line for line in r.stdout.splitlines()
    ), r.stdout
    assert wt.exists(), "dry-run must not remove the worktree"


def test_apply_reaps_detached_scratch_tree(repo: Path):
    wt = _add_detached(repo, repo / "wt-scratch-reapme")

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert r.returncode == 0, diag
    assert "1 archived" in r.stdout, diag
    assert not wt.exists(), "clean detached tree with nothing unpushed should be archived" + diag


def test_detached_with_local_commits_kept(repo: Path):
    wt = _add_detached(repo, repo / "wt-scratch-unpushed")
    _commit(wt, "d.txt")

    r = _sweep(repo, "--apply")

    assert r.returncode == 0, r.stderr
    assert "kept (unpushed)" in r.stdout
    assert wt.exists(), "local-only commits at a detached HEAD must never be removed"


def test_preflight_tree_is_permanent(repo: Path):
    """scripts/ci/preflight.sh pins a scratch worktree named `preflight` and
    hard-resets it per run; sweeping it would churn its warm caches."""
    wt = _add_detached(repo, repo / ".claude" / "worktrees" / "preflight")

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert r.returncode == 0, diag
    assert "kept (permanent)" in r.stdout, diag
    assert "1 permanent" in r.stdout, diag
    assert wt.exists(), diag


def test_archive_detached_pushed_to_nondefault_remote_branch(repo: Path):
    """The removal-time gate must agree with the sweep. A detached head that
    lives on a pushed non-default branch holds nothing unique, yet the
    default-ref comparison used to refuse it (exit 2), so a sweep that
    classified the tree as reapable could never actually remove it."""
    src = repo / "wt-pushed-src"
    _git(repo, "worktree", "add", str(src), "-b", "feature/pushed", "main")
    _commit(src, "s.txt")
    _git(src, "push", "origin", "feature/pushed")
    sha = _git(src, "rev-parse", "HEAD").stdout.strip()
    _git(repo, "worktree", "remove", str(src))
    det = _add_detached(repo, repo / "wt-scratch-pushed", at=sha)

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), str(det)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )

    assert r.returncode == 0, f"stdout={r.stdout}\nstderr={r.stderr}"
    assert not det.exists(), f"stderr={r.stderr}"


def test_archive_detached_local_only_refused(repo: Path):
    det = _add_detached(repo, repo / "wt-scratch-local")
    _commit(det, "l.txt")

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), str(det)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )

    assert r.returncode == 2
    assert "detached HEAD not on any remote" in r.stderr
    assert det.exists()


# ── stale tracking refs: the branch is gone on the server, the ref lives on ─
def _origin_bare(repo: Path) -> Path:
    return Path(_git(repo, "remote", "get-url", "origin").stdout.strip())


def _detached_at_deleted_remote_branch(repo: Path, name: str) -> Path:
    """A tree detached at a commit whose remote branch is GONE on the server:
    the local refs/remotes entry still points at the commit, so only a pruned
    refresh sees that no remote carries it anymore."""
    src = repo / f"{name}-src"
    _git(repo, "worktree", "add", str(src), "-b", f"feature/{name}", "main")
    _commit(src, f"{name}.txt")
    _git(src, "push", "origin", f"feature/{name}")
    sha = _git(src, "rev-parse", "HEAD").stdout.strip()
    _git(repo, "worktree", "remove", str(src))
    _git(_origin_bare(repo), "branch", "-D", f"feature/{name}")
    return _add_detached(repo, repo / f"wt-scratch-{name}", at=sha)


def test_detached_kept_when_remote_branch_was_deleted(repo: Path):
    """Judged against the stale tracking ref the count reads 0 and the sweep
    destroys the only copy of the commit. The pruned refresh must flip the
    verdict to kept."""
    wt = _detached_at_deleted_remote_branch(repo, "gone")

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert r.returncode == 0, diag
    assert "kept (unpushed)" in r.stdout, diag
    assert wt.exists(), "a commit no remote carries anymore must never be reaped" + diag


def test_archive_detached_kept_when_remote_branch_was_deleted(repo: Path):
    """The removal-time gate must agree with the sweep on the stale-ref case;
    otherwise a tree the sweep keeps can still be archived by hand against
    the stale ref."""
    wt = _detached_at_deleted_remote_branch(repo, "gone2")
    script = repo / "scripts" / "setup" / "archive-worktree.sh"

    r = subprocess.run(
        ["bash", str(script), str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )

    assert r.returncode == 2, f"stdout={r.stdout}\nstderr={r.stderr}"
    assert wt.exists(), f"stderr={r.stderr}"


def test_archive_detached_refused_when_refs_unverifiable(repo: Path):
    """No network, no verdict. A detached head whose commits the (now
    unreachable) remote does carry must still be kept when the refresh
    cannot verify that, not archived against refs that may be stale."""
    wt = _add_detached(repo, repo / "wt-scratch-unverifiable")
    _git(repo, "remote", "set-url", "origin", str(repo / "unreachable.git"))
    script = repo / "scripts" / "setup" / "archive-worktree.sh"

    r = subprocess.run(
        ["bash", str(script), str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )

    assert r.returncode == 2, f"stdout={r.stdout}\nstderr={r.stderr}"
    assert "not verifiable" in r.stderr, f"stderr={r.stderr}"
    assert wt.exists(), f"stderr={r.stderr}"


def test_dead_secondary_remote_degrades_instead_of_aborting(repo: Path):
    """fetch --all fails if ANY remote is dead, and one stale account remote
    must not brick the reclaim verb. Origin stays the merged-check baseline
    (branched judging continues); the detached tree is kept because its refs
    cannot be verified against the dead remote - with a receipt that says so,
    not a phantom unpushed count."""
    merged = _add_merged(repo, "reapme")
    det = _add_detached(repo, repo / "wt-scratch-fork")
    _git(repo, "remote", "add", "fork", str(repo / "dead-fork.git"))

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert r.returncode == 0, diag
    assert "1 archived" in r.stdout, diag
    assert not merged.exists(), diag
    assert "remote refs unverifiable" in r.stdout, diag
    assert det.exists(), "refs unverifiable against a dead remote must keep detached trees" + diag


# ── silent-failure guard: empty-state line is explicit, not silence ─────────
def test_empty_state_is_explicit(repo: Path):
    r = _sweep(repo)
    assert r.returncode == 0, r.stderr
    assert "No non-canonical worktrees found." in r.stdout
