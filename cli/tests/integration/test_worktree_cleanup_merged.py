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
from typer.testing import CliRunner

REPO_ROOT = Path(__file__).resolve().parents[3]
LIFECYCLE_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-lifecycle.sh"
LIFECYCLE_COMPAT_SRC = REPO_ROOT / "scripts" / "worktree-lifecycle.sh"
UNPUSHED_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-unpushed.sh"
ARCHIVE_SRC = REPO_ROOT / "scripts" / "setup" / "archive-worktree.sh"
TARGET_GUARD_SRC = REPO_ROOT / "scripts" / "lib" / "target-guard.sh"
REMOVAL_EVENT_SRC = REPO_ROOT / "scripts" / "lib" / "worktree-removal-event.sh"
SETUP_SRC = REPO_ROOT / "scripts" / "setup" / "setup-worktree.sh"
runner = CliRunner()


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
    shutil.copy2(TARGET_GUARD_SRC, canon / "scripts" / "lib" / "target-guard.sh")
    shutil.copy2(REMOVAL_EVENT_SRC, canon / "scripts" / "lib" / "worktree-removal-event.sh")
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


def _cargo_sweep(canon: Path, *flags: str, env_extra: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    script = canon / "scripts" / "lib" / "worktree-lifecycle.sh"
    env = os.environ.copy()
    if env_extra:
        env.update(env_extra)
    return subprocess.run(
        ["bash", str(script), "cleanup", "--cargo-targets", *flags],
        cwd=str(canon), capture_output=True, text=True, env=env,
    )


def _add_target(canon: Path, name: str, size: int, *, old: bool = False) -> Path:
    wt = canon / name
    _git(canon, "worktree", "add", str(wt), "-b", f"feature/{name}", "main")
    target = wt / "crates" / "fixture" / "target"
    target.mkdir(parents=True)
    (target / "artifact.bin").write_bytes(b"x" * size)
    if old:
        old_ts = 1_600_000_000
        os.utime(target / "artifact.bin", (old_ts, old_ts))
        os.utime(target, (old_ts, old_ts))
    return target


def test_cargo_target_dry_run_reports_projection_without_mutation(repo: Path):
    old = _add_target(repo, "cargo-old", 2 * 1024 * 1024, old=True)
    young = _add_target(repo, "cargo-young", 2 * 1024 * 1024)

    r = _cargo_sweep(repo, "--cap-bytes", str(3 * 1024 * 1024), "--target-max-age", "7d")

    assert r.returncode == 0, r.stderr
    assert "mode=dry-run" in r.stdout
    assert "before_bytes=" in r.stdout
    assert "after_bytes=" in r.stdout
    assert "projected_after_bytes=" in r.stdout
    assert "cap_bytes=3145728" in r.stdout
    assert "free_bytes=" in r.stdout
    assert "effective_cap_bytes=" in r.stdout
    assert "reason=age" in r.stdout
    assert old.exists() and young.exists(), "dry-run must not remove either target"


def test_cargo_target_apply_reaps_age_then_cap_and_is_idempotent(repo: Path):
    old = _add_target(repo, "cargo-old", 2 * 1024 * 1024, old=True)
    young = _add_target(repo, "cargo-young", 2 * 1024 * 1024)

    first = _cargo_sweep(
        repo,
        "--cap-bytes", str(3 * 1024 * 1024),
        "--target-max-age", "7d",
        "--apply",
    )
    second = _cargo_sweep(
        repo,
        "--cap-bytes", str(3 * 1024 * 1024),
        "--target-max-age", "7d",
        "--apply",
    )

    assert first.returncode == 0, first.stderr
    assert "mode=apply" in first.stdout
    assert "reason=age" in first.stdout
    assert "after_bytes=" in first.stdout
    assert "cap_bytes=3145728" in first.stdout
    assert not old.exists(), "over-age target must be reaped even when one deletion satisfies cap"
    assert young.exists(), "young target should remain once the cap is satisfied"
    assert second.returncode == 0, second.stderr
    assert "reaped=0" in second.stdout
    assert "reclaimed_bytes=0" in second.stdout


def test_cargo_target_explicit_dry_run_wins_over_apply(repo: Path):
    target = _add_target(repo, "cargo-dry-run", 2 * 1024 * 1024, old=True)

    r = _cargo_sweep(
        repo,
        "--cap-bytes", "1",
        "--target-max-age", "0d",
        "--apply",
        "--dry-run",
    )

    assert r.returncode == 0, r.stderr
    assert "mode=dry-run" in r.stdout
    assert "would-reap" in r.stdout
    assert target.exists(), "explicit dry-run must override apply"


def test_cargo_target_apply_refuses_to_delete_rooted_builder(repo: Path):
    target = _add_target(repo, "cargo-live", 2 * 1024 * 1024, old=True)
    holder = subprocess.Popen(["sleep", "10"], cwd=target.parent.parent.parent)
    try:
        r = _cargo_sweep(
            repo,
            "--cap-bytes", "1",
            "--target-max-age", "0d",
            "--apply",
        )
    finally:
        holder.terminate()
        holder.wait(timeout=5)

    assert r.returncode != 0
    assert "protected" in r.stdout
    assert "over-cap-protected" in r.stdout
    assert target.exists(), "a rooted process must protect its worktree target"


def test_cargo_target_low_free_space_tightens_ceiling_high_free_leaves_alone(repo: Path):
    # x-ea02: the same allocation must be left alone when free space is ample
    # and reaped when the disk is nearly full - the ceiling is a function of
    # free space, not just the absolute cap.
    target = _add_target(repo, "cargo-lowfree", 4 * 1024 * 1024)

    high = _cargo_sweep(
        repo, "--cap-bytes", str(64 * 1024 * 1024), "--apply",
        env_extra={"FNO_CARGO_FREE_BYTES": "100000000000000"},
    )
    assert high.returncode == 0, high.stderr
    assert "status=ok" in high.stdout
    assert "reaped=0" in high.stdout
    assert "effective_cap_bytes=67108864" in high.stdout
    assert target.exists(), "ample free space must leave the same allocation alone"

    low = _cargo_sweep(
        repo, "--cap-bytes", str(64 * 1024 * 1024), "--apply",
        env_extra={"FNO_CARGO_FREE_BYTES": "6000000"},
    )
    assert low.returncode == 0, low.stderr
    assert "reason=cap" in low.stdout
    assert "free_bytes=6000000" in low.stdout
    assert "effective_cap_bytes=3000000" in low.stdout
    assert not target.exists(), "a nearly full disk must tighten the ceiling and reap"


def test_cargo_target_unreadable_free_space_falls_back_to_absolute_cap(repo: Path):
    target = _add_target(repo, "cargo-badfree", 2 * 1024 * 1024)

    r = _cargo_sweep(
        repo, "--cap-bytes", str(64 * 1024 * 1024), "--apply",
        env_extra={"FNO_CARGO_FREE_BYTES": "garbage"},
    )

    assert r.returncode == 0, r.stderr
    assert "free_bytes=unknown" in r.stdout
    assert "effective_cap_bytes=67108864" in r.stdout
    assert target.exists(), "an unreadable free-space read must not widen the ceiling"


def test_cargo_target_over_free_ceiling_with_everything_protected_names_free(repo: Path):
    target = _add_target(repo, "cargo-protfree", 2 * 1024 * 1024, old=True)
    holder = subprocess.Popen(["sleep", "10"], cwd=target.parent.parent.parent)
    try:
        r = _cargo_sweep(
            repo, "--cap-bytes", "1", "--target-max-age", "0d", "--apply",
            env_extra={"FNO_CARGO_FREE_BYTES": "2"},
        )
    finally:
        holder.terminate()
        holder.wait(timeout=5)

    assert r.returncode != 0
    assert "over-cap-protected" in r.stdout
    assert "free_bytes=2" in r.stdout
    assert "effective_cap_bytes=1" in r.stdout
    assert target.exists(), "protection holds even under a tightened ceiling"


def test_setup_worktree_runs_the_same_cargo_target_apply_path():
    text = SETUP_SRC.read_text()
    # Canonical spelling leads; the retired root one stays for one release as
    # the deploy-window fallback (repo script newer than the installed fno).
    assert "fno agents workspace worktree cleanup --cargo-targets --apply" in text
    assert "fno workspace worktree cleanup --cargo-targets --apply" in text
    assert "cargo target cleanup failed" in text


def test_cargo_target_cli_forwards_explicit_bounds(monkeypatch: pytest.MonkeyPatch):
    from fno.worktree_cli import cli as worktree_cli

    seen: list[str] = []

    def fake_run(*args: str) -> int:
        seen.extend(args)
        return 0

    monkeypatch.setattr(worktree_cli, "_run_lifecycle", fake_run)
    result = runner.invoke(
        worktree_cli.app,
        [
            "cleanup",
            "--cargo-targets",
            "--cap-bytes",
            "8388608",
            "--target-max-age",
            "3d",
            "--apply",
        ],
    )

    assert result.exit_code == 0, result.output
    assert seen == [
        "cleanup",
        "--apply",
        "--cargo-targets",
        "--cap-bytes",
        "8388608",
        "--free-share-pct",
        "50",
        "--target-max-age",
        "3d",
    ]


def test_archive_cli_forwards_explicit_guard_flags(monkeypatch: pytest.MonkeyPatch):
    from fno.worktree_cli import cli as worktree_cli

    seen: list[str] = []

    def fake_run(*args: str) -> int:
        seen.extend(args)
        return 0

    monkeypatch.setattr(worktree_cli, "_run_lifecycle", fake_run)
    result = runner.invoke(
        worktree_cli.app,
        ["archive", "--force", "--yes", "--delete-branch", "fixture"],
    )

    assert result.exit_code == 0, result.output
    assert seen == ["archive", "--force", "--yes", "--delete-branch", "fixture"]


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


def test_merge_triggered_archive_refuses_pushed_but_unmerged_head(repo: Path):
    wt = repo / "wt-unmerged-archive"
    _git(repo, "worktree", "add", str(wt), "-b", "feature/unmerged-archive", "main")
    _commit(wt, "unmerged.txt")
    _git(wt, "push", "-u", "origin", "feature/unmerged-archive")

    result = subprocess.run(
        ["bash", str(ARCHIVE_SRC), str(wt), "--merge-triggered"],
        cwd=str(repo), capture_output=True, text=True,
    )

    assert result.returncode == 2, result.stderr
    assert "origin/main" in result.stderr
    assert wt.exists(), "merge-triggered cleanup must keep an unmerged worktree"


# ── x-60de: every removal emits one attributable event row ─────────────────
def test_apply_removal_emits_event_row(repo: Path, tmp_path: Path, monkeypatch):
    """A removal without a row is unattributable: live trees lost on
    2026-08-25 and 2026-08-29 left no evidence because no removal path
    emitted anything. The row must name the path, the caller, the claim
    state read at decision time, and the reason - and reach the
    machine-global journal, the instrument that walked 2239 rows and found
    zero removals."""
    import json as _json

    venv_bin = REPO_ROOT / "cli" / ".venv" / "bin"
    if not (venv_bin / "fno-py").exists():
        pytest.skip("cli venv absent (run fno doctor test once); emit falls back to deployed fno")
    monkeypatch.setenv("PATH", f"{venv_bin}{os.pathsep}{os.environ['PATH']}")
    # Sandbox the machine-global mirror through a point-config: the global
    # journal follows config state_dir, and FNO_CONFIG redirects config
    # loading for the emit subprocess.
    sandbox_state = tmp_path / "fno-state"
    sandbox_state.mkdir()
    sandbox_cfg = tmp_path / "fno-config.toml"
    sandbox_cfg.write_text(f'state_dir = "{sandbox_state}"\n')
    monkeypatch.setenv("FNO_CONFIG", str(sandbox_cfg))

    wt = _add_merged(repo, "reapme")

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert r.returncode == 0, diag
    assert not wt.exists(), diag

    def _rows(path: Path) -> list[dict]:
        assert path.exists(), f"missing journal: {path}"
        return [
            _json.loads(line)
            for line in path.read_text().splitlines()
            if line.strip()
        ]

    project_rows = _rows(repo / ".fno" / "events.jsonl")
    hits = [row for row in project_rows if row.get("type") == "worktree_removed"]
    assert hits, f"removal emitted no row; project log types: {[r.get('type') for r in project_rows]}"
    data = hits[-1]["data"]
    assert data["path"] == str(wt), diag
    assert data["caller"] == "cleanup --merged", diag
    assert data["claim"], "the row must carry the claim state read at decision time"
    assert data["reason"], "the row must carry why the tree was judged safe"
    assert data["branch"] == "feature/reapme", diag
    assert data["reclaimed_bytes"] > 0, diag

    global_rows = _rows(sandbox_state / "events.jsonl")
    assert any(row.get("type") == "worktree_removed" for row in global_rows), \
        "worktree_removed is a GLOBAL_MIRROR_TYPES row: it must reach the machine-global journal"


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


# ── AC2-EDGE: local-only .fno do state survives the reap ───────────────────────
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


def test_archive_force_discloses_exact_dirty_and_unpushed_state(repo: Path):
    wt = repo / "wt-force-disclosure"
    _git(repo, "worktree", "add", str(wt), "-b", "feature/force-disclosure", "main")
    unique_subject = "add force disclosure specimen"
    _commit(wt, "committed.txt")
    _git(wt, "commit", "--amend", "-m", unique_subject)
    short_sha = _git(wt, "rev-parse", "--short", "HEAD").stdout.strip()
    (wt / "dirty.txt").write_text("not committed\n")

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), "--force", "--yes", str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )
    diag = f"stdout={r.stdout}\nstderr={r.stderr}"

    assert r.returncode == 0, diag
    assert "FORCE" in diag
    assert "dirty.txt" in diag
    assert short_sha in diag
    assert unique_subject in diag
    assert "discarded" in diag
    assert not wt.exists(), diag


def test_archive_force_refreshes_upstream_before_disclosure(repo: Path):
    wt = repo / "wt-force-stale-upstream"
    branch = "feature/force-stale-upstream"
    _git(repo, "worktree", "add", str(wt), "-b", branch, "main")
    _commit(wt, "unique.txt")
    unique_subject = "add unique.txt"
    short_sha = _git(wt, "rev-parse", "--short", "HEAD").stdout.strip()
    _git(wt, "push", "-u", "origin", branch)
    _git(_origin_bare(repo), "branch", "-f", branch, "main")

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), "--force", "--yes", "--delete-branch", str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )
    diag = f"stdout={r.stdout}\nstderr={r.stderr}"

    assert r.returncode == 0, diag
    assert short_sha in diag
    assert unique_subject in diag
    assert "branch feature/force-stale-upstream deleted" in r.stderr
    assert not wt.exists(), diag
    assert not _git(repo, "branch", "--list", branch).stdout.strip()


def test_archive_delete_branch_refuses_unverifiable_remote_state(repo: Path):
    wt = repo / "wt-delete-unverifiable"
    branch = "feature/delete-unverifiable"
    _git(repo, "worktree", "add", str(wt), "-b", branch, "main")
    _commit(wt, "unique.txt")
    unique_sha = _git(wt, "rev-parse", "--short", "HEAD").stdout.strip()
    _git(repo, "remote", "remove", "origin")

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), "--yes", "--delete-branch", str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )
    diag = f"stdout={r.stdout}\nstderr={r.stderr}"

    assert r.returncode == 2, diag
    assert "not verifiable" in r.stderr, diag
    assert wt.exists(), diag
    assert _git(repo, "rev-parse", "--short", branch).stdout.strip() == unique_sha


def test_archive_refuses_unreadable_live_claim(
    repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    wt = _add_merged(repo, "unreadable-claim")
    fno_bin = tmp_path / "bin"
    fno_bin.mkdir()
    fake_fno = fno_bin / "fno"
    fake_fno.write_text("#!/bin/sh\nexit 1\n")
    fake_fno.chmod(0o755)
    monkeypatch.setenv("PATH", f"{fno_bin}:{os.environ['PATH']}")
    state = wt / ".fno"
    state.mkdir()
    (state / "target-state.md").write_text(
        "input: x-unreadable\ntarget_claim_key: node:x-unreadable\nowner_pid: 999999\n"
    )

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), "--force", "--yes", str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )

    assert r.returncode == 2
    assert "live target claim not verifiable" in r.stderr
    assert wt.exists()


def test_archive_force_rechecks_state_after_process_cleanup(repo: Path):
    wt = _add_merged(repo, "force-recheck")
    late_path = wt / "late.txt"
    holder_env = os.environ.copy()
    holder_env["LATE_PATH"] = str(late_path)
    holder = subprocess.Popen(
        [
            "bash",
            "-c",
            f"cd '{wt}'; trap 'printf late > \"$LATE_PATH\"; exit 0' TERM; while :; do sleep 1; done",
        ],
        cwd=repo,
        env=holder_env,
        start_new_session=True,
    )
    try:
        script = repo / "scripts" / "setup" / "archive-worktree.sh"
        r = subprocess.run(
            ["bash", str(script), "--force", "--yes", str(wt)],
            cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
        )
    finally:
        if holder.poll() is None:
            holder.terminate()
        holder.wait(timeout=5)

    diag = f"stdout={r.stdout}\nstderr={r.stderr}"
    assert r.returncode == 2, diag
    assert "forced state changed after disclosure" in r.stderr, diag
    assert (wt / "late.txt").exists(), diag
    assert wt.exists(), diag


def test_archive_reports_failed_branch_deletion(repo: Path):
    wt = repo / "wt-delete-fail"
    holder = repo / "wt-delete-holder"
    branch = "feature/delete-fail"
    _git(repo, "worktree", "add", str(wt), "-b", branch, "main")
    _commit(wt, "delete-fail.txt")
    _git(repo, "worktree", "add", "-f", str(holder), branch)

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), "--force", "--yes", "--delete-branch", str(wt)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )
    diag = f"stdout={r.stdout}\nstderr={r.stderr}"

    assert r.returncode == 1, diag
    assert not wt.exists(), diag
    assert holder.exists(), diag
    assert "branch delete failed" in r.stderr.lower()
    assert "branch feature/delete-fail preserved" in r.stderr
    assert _git(repo, "branch", "--list", branch).stdout.strip()


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

    r = _age_sweep(repo, "--apply")

    assert r.returncode == 0, r.stderr
    assert not wt.exists(), r.stdout
    assert "REMOVED" in r.stdout


def test_age_sweep_is_dry_run_by_default(repo: Path):
    """Both removal modes share one default: a bare sweep reports, --apply
    executes. The age mode used to remove on a bare call - a king ran it
    expecting a preview and 13 trees went."""
    wt = _add_detached(repo, repo / "wt-age-default")

    r = _age_sweep(repo)

    assert r.returncode == 0, r.stderr
    assert "WOULD REMOVE" in r.stdout
    assert "dry-run" in r.stdout
    assert wt.exists(), "a bare age sweep must not remove anything"


def test_age_sweep_keeps_detached_tree_with_uncommitted_work(repo: Path):
    """Same loss class as unpushed commits, one step earlier: a detached tree
    has no branch holding uncommitted content either, and the sweep removes
    with --force under --apply. The in-flight eval tree rides this guard via
    its untracked marker file (cli/src/fno/evals/runner.py)."""
    wt = _add_detached(repo, repo / "wt-age-dirty")
    (wt / "scratch.txt").write_text("not on any ref\n")

    r = _age_sweep(repo, "--apply")

    assert r.returncode == 0, r.stderr
    assert f"SKIP: {wt} (holds uncommitted work: " in r.stdout
    assert wt.exists(), "age sweep must not force-remove uncommitted work on a detached HEAD"


def test_age_sweep_keeps_branched_tree_with_uncommitted_work(repo: Path):
    """The uncommitted-work guard used to fire only on detached HEADs; a
    branched tree with a dirty file hit the same --force remove and lost it -
    the branch never recorded it. DIRTY is never touched by any automatic
    path, branched or detached."""
    wt = repo / "wt-age-branched-dirty"
    _git(repo, "worktree", "add", str(wt), "-b", "feature/wt-age-branched-dirty", "main")
    _commit(wt, "branched.txt")
    (wt / "dirty.txt").write_text("uncommitted\n")

    r = _age_sweep(repo, "--apply")

    assert r.returncode == 0, r.stderr
    assert f"SKIP: {wt} (holds uncommitted work: " in r.stdout
    assert wt.exists(), "age sweep must not force-remove uncommitted work on a branched tree"
    assert (wt / "dirty.txt").exists()


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


def test_non_origin_single_remote_detached_still_archives(repo: Path):
    """The refresh verifies whichever remotes exist. A repo whose only remote
    is named anything but origin must still archive a detached head that the
    remote carries; hardcoding origin made every such repo read as
    permanently unverifiable."""
    _git(repo, "remote", "rename", "origin", "upstream")
    det = _add_detached(repo, repo / "wt-scratch-upstream")

    script = repo / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), str(det)],
        cwd=str(repo), capture_output=True, text=True, stdin=subprocess.DEVNULL,
    )

    assert r.returncode == 0, f"stdout={r.stdout}\nstderr={r.stderr}"
    assert not det.exists(), f"stderr={r.stderr}"


# ── silent-failure guard: empty-state line is explicit, not silence ─────────
def test_empty_state_is_explicit(repo: Path):
    r = _sweep(repo)
    assert r.returncode == 0, r.stderr
    assert "No non-canonical worktrees found." in r.stdout


# ── the deleted-upstream case: the shape EVERY merged PR leaves behind ──────
def _add_merged_with_deleted_upstream(canon: Path, name: str) -> Path:
    """The real post-merge shape: the branch tracked a remote branch, the merge
    landed in main, and the remote branch was deleted (GitHub does this on
    merge), so `fetch --prune` drops the ref while the branch config keeps
    naming it."""
    wt = canon / name
    _git(canon, "worktree", "add", str(wt), "-b", f"feature/{name}", "main")
    _commit(wt, f"{name}.txt")
    _git(wt, "push", "-u", "origin", f"feature/{name}")
    _git(canon, "merge", "--no-ff", f"feature/{name}", "-m", f"merge {name}")
    _git(canon, "push", "origin", "main")
    _git(canon, "push", "origin", "--delete", f"feature/{name}")
    _git(canon, "fetch", "--prune", "origin")
    return wt


def test_deleted_upstream_ref_still_archives(repo: Path):
    """A pruned upstream is not an unreadable one.

    `rev-parse --abbrev-ref --symbolic-full-name @{u}` prints the literal
    `@{u}` on stdout when the ref is gone, so `|| true` never clears it and the
    comparison against that string errors. The caller read the error as
    "unpushed state not verifiable" and refused. Measured on the real repo:
    122 of 132 merged candidates refused this way, every one of them already
    contained in origin/main.
    """
    wt = _add_merged_with_deleted_upstream(repo, "pruned")
    assert _git(wt, "rev-parse", "--verify", "--quiet", "@{u}", check=False).returncode != 0

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert r.returncode == 0, diag
    assert "1 archived" in r.stdout, diag
    assert "not verifiable" not in r.stderr, diag
    assert not wt.exists(), "worktree dir should be gone" + diag


def test_deleted_upstream_with_unpushed_work_is_still_refused(repo: Path):
    """Clearing the pruned NAME must not clear the strictness it carried.

    A branch that tracked a remote is held to a refusal, not a warning, when
    its state cannot be verified. The commit below is in neither the deleted
    remote branch nor main, so removal would destroy it.
    """
    wt = _add_merged_with_deleted_upstream(repo, "pruned2")
    _commit(wt, "after-merge.txt")

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert "1 archived" not in r.stdout, diag
    assert wt.exists(), "a worktree carrying unmerged work must survive" + diag


def test_deleted_upstream_archives_without_origin_head(repo: Path):
    """`refs/remotes/origin/HEAD` is optional, and a cleared upstream leaves the
    default-ref lookup with no baseline. The branch still tracked a remote, so
    the missing baseline is a refusal, and a fully merged worktree survives.

    The deletion has to outlive the script's own fetch. Deleting the local ref
    alone does not: git restores it on the next fetch, so an earlier version of
    this test asserted the absence, watched the fetch undo it, and passed while
    measuring nothing. Pointing the REMOTE's HEAD at a ref that does not exist
    is what makes the absence survive.
    """
    wt = _add_merged_with_deleted_upstream(repo, "nohead")
    origin_git = repo.parent / "origin.git"
    subprocess.run(
        ["git", "-C", str(origin_git), "symbolic-ref", "HEAD", "refs/heads/gone"],
        check=True, capture_output=True,
    )
    _git(repo, "remote", "set-head", "origin", "--delete")
    _git(repo, "fetch", "origin")
    assert (
        _git(repo, "rev-parse", "--verify", "--quiet", "origin/HEAD", check=False).returncode
        != 0
    ), "the absence must survive a fetch, or this test measures nothing"

    r = _sweep(repo, "--apply")
    diag = f"\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}"

    assert "1 archived" in r.stdout, diag
    assert "no upstream and no resolvable remote HEAD" not in r.stderr, diag
    assert not wt.exists(), "worktree dir should be gone" + diag
