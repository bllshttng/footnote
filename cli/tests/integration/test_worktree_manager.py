"""Integration tests for scripts/lib/worktree-manager.sh.

Cover the AC4-EDGE seeds called out in the worktree-management-overhaul plan:

- create honors worktree_base from settings.yaml
- create is idempotent on re-call (returns existing worktree, status=ok existing=true)
- setup skips install when lockfile hash matches
- setup re-runs install when lockfile hash changes
- setup copies env files declared in config.worktree.env_files
- cleanup --mode=stale --dry-run is non-destructive
- cleanup refuses to remove a worktree with status: IN_PROGRESS
- Path resolution falls back to .claude/worktrees/ when worktree_base is unset
- Tilde-prefixed worktree_base values are expanded to $HOME

Tests use a temp git repo + temp $HOME so we never touch real worktrees.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import textwrap
from pathlib import Path

import pytest


def _repo_root() -> Path:
    """Resolve the repo root via git, not via parents[N] - the latter
    silently breaks if the test file moves up or down a directory.
    """
    out = subprocess.check_output(
        ["git", "rev-parse", "--show-toplevel"],
        cwd=str(Path(__file__).parent),
        text=True,
    ).strip()
    return Path(out)


REPO_ROOT = _repo_root()
SCRIPT = REPO_ROOT / "scripts" / "lib" / "worktree-manager.sh"


# ---------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------


@pytest.fixture
def tmp_repo(tmp_path: Path) -> Path:
    """Initialize a minimal git repo with one commit on main."""
    subprocess.run(["git", "init", "-b", "main", str(tmp_path)],
                   check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "test@test.com"],
                   cwd=tmp_path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "T"],
                   cwd=tmp_path, check=True, capture_output=True)
    (tmp_path / "README.md").write_text("# test\n")
    subprocess.run(["git", "add", "."], cwd=tmp_path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "init"],
                   cwd=tmp_path, check=True, capture_output=True)
    return tmp_path


@pytest.fixture
def isolated_env(tmp_path: Path, monkeypatch) -> dict[str, str]:
    """Point HOME at a clean temp dir so global ~/.fno/settings.yaml
    can be controlled per-test without touching the user's real settings.
    """
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setenv("HOME", str(fake_home))
    return {"HOME": str(fake_home)}


def run_wtm(*args: str, cwd: Path | None = None, env: dict[str, str] | None = None,
            check: bool = False) -> subprocess.CompletedProcess:
    """Invoke worktree-manager.sh with the given arguments."""
    full_env = os.environ.copy()
    if env:
        full_env.update(env)
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        cwd=str(cwd) if cwd else None,
        env=full_env,
        capture_output=True,
        text=True,
        check=check,
    )


def parse_json(stdout: str) -> dict:
    # Take the LAST line that parses as JSON. The script may emit log lines
    # to stderr, but stdout should be the JSON payload alone.
    lines = [ln for ln in stdout.strip().splitlines() if ln.strip()]
    assert lines, f"no stdout from worktree-manager (stdout was empty)"
    last = lines[-1]
    try:
        return json.loads(last)
    except json.JSONDecodeError as e:  # pragma: no cover - debug aid
        raise AssertionError(f"bad JSON: {last!r}\nfull stdout: {stdout!r}") from e


def write_settings(home: Path, settings_yaml: str) -> Path:
    """Write a global settings.yaml under $HOME/.fno/."""
    fno = home / ".fno"
    fno.mkdir(parents=True, exist_ok=True)
    path = fno / "settings.yaml"
    path.write_text(settings_yaml)
    return path


# ---------------------------------------------------------------
# resolve verb
# ---------------------------------------------------------------


def test_resolve_falls_back_to_claude_worktrees(tmp_repo, isolated_env):
    """No settings.yaml => fall back to <repo>/.claude/worktrees."""
    result = run_wtm("resolve", "anything", cwd=tmp_repo, env=isolated_env)
    assert result.returncode == 0, result.stderr
    expected = str(tmp_repo / ".claude" / "worktrees")
    assert result.stdout.strip() == expected


def test_resolve_honors_worktree_base_from_global_settings(tmp_repo, isolated_env, tmp_path):
    """Project listed in global settings.yaml gets its worktree_base honored."""
    write_settings(Path(isolated_env["HOME"]), textwrap.dedent("""
        work:
          workspaces:
            ws1:
              projects:
                - name: foo
                  path: /tmp/foo
                  worktree_base: ~/conductor/workspaces/foo
    """))
    result = run_wtm("resolve", "foo", cwd=tmp_repo, env=isolated_env)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == f"{isolated_env['HOME']}/conductor/workspaces/foo"


def test_resolve_legacy_flat_workspace_shape(tmp_repo, isolated_env):
    """work.projects[] (legacy flat shape) is still honored."""
    write_settings(Path(isolated_env["HOME"]), textwrap.dedent("""
        work:
          projects:
            - name: legacy
              path: /tmp/legacy
              worktree_base: /custom/path/legacy
    """))
    result = run_wtm("resolve", "legacy", cwd=tmp_repo, env=isolated_env)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "/custom/path/legacy"


def test_resolve_unknown_project_falls_back(tmp_repo, isolated_env):
    """A project not in settings.yaml falls back to .claude/worktrees."""
    write_settings(Path(isolated_env["HOME"]), textwrap.dedent("""
        work:
          workspaces:
            ws1:
              projects:
                - name: foo
                  worktree_base: /custom/foo
    """))
    result = run_wtm("resolve", "bar", cwd=tmp_repo, env=isolated_env)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == str(tmp_repo / ".claude" / "worktrees")


# ---------------------------------------------------------------
# create verb
# ---------------------------------------------------------------


def test_create_uses_worktree_base_from_settings(tmp_repo, isolated_env):
    """create against a project with custom worktree_base produces path under it."""
    base = Path(isolated_env["HOME"]) / "conductor" / "workspaces" / "myproj"
    write_settings(Path(isolated_env["HOME"]), textwrap.dedent(f"""
        work:
          workspaces:
            ws1:
              projects:
                - name: myproj
                  worktree_base: {base}
    """))
    result = run_wtm("create", "myproj", "feature-x", cwd=tmp_repo, env=isolated_env)
    assert result.returncode == 0, f"stderr: {result.stderr}\nstdout: {result.stdout}"
    payload = parse_json(result.stdout)
    assert payload["status"] == "ok"
    assert payload["existing"] is False
    assert payload["path"] == f"{base}/feature-x"
    assert payload["branch"] == "feature/feature-x"
    assert Path(payload["path"]).is_dir()


def test_create_is_idempotent(tmp_repo, isolated_env):
    """Re-calling create with the same slug returns existing=true, no error."""
    result1 = run_wtm("create", ".", "idem-test", cwd=tmp_repo, env=isolated_env)
    assert result1.returncode == 0, result1.stderr
    payload1 = parse_json(result1.stdout)
    assert payload1["existing"] is False

    result2 = run_wtm("create", ".", "idem-test", cwd=tmp_repo, env=isolated_env)
    assert result2.returncode == 0, result2.stderr
    payload2 = parse_json(result2.stdout)
    assert payload2["existing"] is True
    assert payload2["path"] == payload1["path"]


def test_create_ephemeral_branch_naming(tmp_repo, isolated_env):
    """ephemeral mode does not prepend feature/ to the branch name."""
    result = run_wtm(
        "create", ".", "ephem-test",
        "--mode=ephemeral", "--branch=discover/abc123",
        cwd=tmp_repo, env=isolated_env,
    )
    assert result.returncode == 0, result.stderr
    payload = parse_json(result.stdout)
    assert payload["branch"] == "discover/abc123"
    assert payload["mode"] == "ephemeral"


# ---------------------------------------------------------------
# setup verb
# ---------------------------------------------------------------


def test_setup_caches_lockfile_hash_and_skips_on_match(tmp_repo, isolated_env):
    """Two setup runs back-to-back: first runs install, second hits cache."""
    # Add a fake lockfile so _wtm_lockfile_hash has something to hash, and a
    # no-op setup_command so we don't actually install anything.
    write_settings(Path(isolated_env["HOME"]), textwrap.dedent("""
        config:
          worktree:
            setup_command: "echo setup-ran"
    """))
    (tmp_repo / "package-lock.json").write_text('{"name": "test"}')

    create = run_wtm("create", ".", "cache-test", cwd=tmp_repo, env=isolated_env)
    assert create.returncode == 0, create.stderr
    wt_path = parse_json(create.stdout)["path"]
    # The created worktree shares the lockfile via git checkout
    (Path(wt_path) / "package-lock.json").write_text('{"name": "test"}')

    first = run_wtm("setup", wt_path, cwd=tmp_repo, env=isolated_env)
    assert first.returncode == 0, first.stderr
    p1 = parse_json(first.stdout)
    assert p1["cached"] is False
    assert p1["install"] == "ok"

    second = run_wtm("setup", wt_path, cwd=tmp_repo, env=isolated_env)
    assert second.returncode == 0, second.stderr
    p2 = parse_json(second.stdout)
    assert p2["cached"] is True, f"expected cache hit, got: {p2}"


def test_setup_reinstalls_when_lockfile_changes(tmp_repo, isolated_env):
    """Changing the lockfile hash forces a re-install."""
    write_settings(Path(isolated_env["HOME"]), textwrap.dedent("""
        config:
          worktree:
            setup_command: "echo setup-ran"
    """))
    (tmp_repo / "package-lock.json").write_text('{"v": 1}')

    create = run_wtm("create", ".", "rehash-test", cwd=tmp_repo, env=isolated_env)
    wt_path = parse_json(create.stdout)["path"]
    (Path(wt_path) / "package-lock.json").write_text('{"v": 1}')

    first = run_wtm("setup", wt_path, cwd=tmp_repo, env=isolated_env)
    assert parse_json(first.stdout)["cached"] is False

    # Mutate lockfile -> hash changes -> install must re-run
    (Path(wt_path) / "package-lock.json").write_text('{"v": 2}')
    second = run_wtm("setup", wt_path, cwd=tmp_repo, env=isolated_env)
    p2 = parse_json(second.stdout)
    assert p2["cached"] is False
    assert p2["install"] == "ok"


def test_setup_copies_declared_env_files(tmp_repo, isolated_env):
    """env files listed in config.worktree.env_files are copied to the worktree."""
    write_settings(Path(isolated_env["HOME"]), textwrap.dedent("""
        config:
          worktree:
            env_files:
              - .env.test
            setup_command: "true"
    """))
    (tmp_repo / ".env.test").write_text("FOO=bar\n")

    create = run_wtm("create", ".", "envcopy-test", cwd=tmp_repo, env=isolated_env)
    wt_path = Path(parse_json(create.stdout)["path"])

    setup = run_wtm("setup", str(wt_path), cwd=tmp_repo, env=isolated_env)
    assert setup.returncode == 0, setup.stderr
    p = parse_json(setup.stdout)
    assert int(p["env_files_copied"]) >= 1
    assert (wt_path / ".env.test").exists()
    assert (wt_path / ".env.test").read_text() == "FOO=bar\n"


# ---------------------------------------------------------------
# cleanup verb (lighter coverage - delegates to existing lifecycle script)
# ---------------------------------------------------------------


def test_cleanup_dry_run_is_non_destructive(tmp_repo, isolated_env):
    """cleanup --dry-run never actually removes worktrees."""
    create = run_wtm("create", ".", "dryrun-test", cwd=tmp_repo, env=isolated_env)
    wt_path = parse_json(create.stdout)["path"]

    # Use --older-than 0 so the dry-run would otherwise hit our worktree.
    result = run_wtm(
        "cleanup", "--mode=all", "--older-than=0d", "--dry-run",
        cwd=tmp_repo, env=isolated_env,
    )
    assert result.returncode == 0, result.stderr
    assert Path(wt_path).is_dir(), "dry-run must NOT remove the worktree"


def test_cleanup_skips_in_progress_target_session(tmp_repo, isolated_env):
    """A worktree with status: IN_PROGRESS is never removed."""
    create = run_wtm("create", ".", "active-test", cwd=tmp_repo, env=isolated_env)
    wt_path = Path(parse_json(create.stdout)["path"])

    abil = wt_path / ".fno"
    abil.mkdir(parents=True, exist_ok=True)
    (abil / "target-state.md").write_text(textwrap.dedent("""
        ---
        status: IN_PROGRESS
        ---
    """))

    # Force-old by lying about commit time would require git plumbing; instead
    # we run cleanup with --older-than=0 so the worktree is "old enough" to
    # qualify, and verify the IN_PROGRESS sentinel keeps it safe.
    result = run_wtm(
        "cleanup", "--mode=all", "--older-than=0d",
        cwd=tmp_repo, env=isolated_env,
    )
    assert result.returncode == 0, result.stderr
    assert wt_path.is_dir(), "active target session worktree must be preserved"


# ---------------------------------------------------------------
# migrate verb
# ---------------------------------------------------------------


def test_migrate_dry_run_classifies_without_removing(tmp_repo, isolated_env):
    """migrate --dry-run lists candidates and removes nothing."""
    create = run_wtm("create", ".", "migrate-test", cwd=tmp_repo, env=isolated_env)
    wt_path = Path(parse_json(create.stdout)["path"])

    result = run_wtm("migrate", "--dry-run", cwd=tmp_repo, env=isolated_env)
    assert result.returncode == 0, result.stderr
    payload = parse_json(result.stdout)
    assert payload["status"] == "ok"
    assert int(payload["removed"]) == 0
    assert wt_path.is_dir()


def test_migrate_auto_removes_stale_but_preserves_live(tmp_repo, isolated_env):
    """migrate --auto removes stale worktrees and preserves IN_PROGRESS ones."""
    # Create two worktrees: one stale, one with IN_PROGRESS target state.
    stale = run_wtm("create", ".", "stale-wt", cwd=tmp_repo, env=isolated_env)
    stale_path = Path(parse_json(stale.stdout)["path"])

    live = run_wtm("create", ".", "live-wt", cwd=tmp_repo, env=isolated_env)
    live_path = Path(parse_json(live.stdout)["path"])
    abil = live_path / ".fno"
    abil.mkdir(parents=True, exist_ok=True)
    (abil / "target-state.md").write_text("---\nstatus: IN_PROGRESS\n---\n")

    result = run_wtm("migrate", "--auto", cwd=tmp_repo, env=isolated_env)
    assert result.returncode == 0, result.stderr
    payload = parse_json(result.stdout)
    assert payload["status"] == "ok"
    # The stale dir should be gone; the live one preserved.
    assert not stale_path.is_dir(), "stale worktree should be removed by --auto"
    assert live_path.is_dir(), "IN_PROGRESS worktree must NEVER be removed"
    assert int(payload["live"]) >= 1


def test_migrate_treats_unreadable_state_as_live(tmp_repo, isolated_env):
    """A worktree whose target-state.md is unreadable (chmod 000) is treated as
    LIVE, not stale - this is the fail-safe for the silent-failure-hunter
    finding that an unreadable state file would otherwise let migrate --auto
    delete an active session.
    """
    create = run_wtm("create", ".", "unreadable-test", cwd=tmp_repo, env=isolated_env)
    wt_path = Path(parse_json(create.stdout)["path"])
    abil = wt_path / ".fno"
    abil.mkdir(parents=True, exist_ok=True)
    state = abil / "target-state.md"
    state.write_text("---\nstatus: IN_PROGRESS\n---\n")
    try:
        state.chmod(0o000)  # unreadable
        result = run_wtm("migrate", "--auto", cwd=tmp_repo, env=isolated_env)
        assert result.returncode == 0, result.stderr
        assert wt_path.is_dir(), "unreadable state must be treated as LIVE"
    finally:
        state.chmod(0o644)  # restore so pytest can clean up


def test_cleanup_mode_stale_excludes_ephemeral(tmp_repo, isolated_env):
    """cleanup --mode=stale runs against feature/* branches but skips
    discover/ and speculate/ prefixes. This protects the API contract
    promise that stale and ephemeral are complementary modes.
    """
    # Create three worktrees: one feature/, one discover/, one speculate/
    feat = run_wtm("create", ".", "feat-stale", cwd=tmp_repo, env=isolated_env)
    feat_path = parse_json(feat.stdout)["path"]
    disc = run_wtm("create", ".", "disc-skip",
                   "--mode=ephemeral", "--branch=discover/x",
                   cwd=tmp_repo, env=isolated_env)
    disc_path = parse_json(disc.stdout)["path"]
    spec = run_wtm("create", ".", "spec-skip",
                   "--mode=ephemeral", "--branch=speculate/y",
                   cwd=tmp_repo, env=isolated_env)
    spec_path = parse_json(spec.stdout)["path"]

    # Use --dry-run so we don't delete anything; we just verify the
    # exit code is 0 and the script ran. The lifecycle script's stderr
    # tells us which prefixes were dispatched.
    result = run_wtm(
        "cleanup", "--mode=stale", "--older-than=0d", "--dry-run",
        cwd=tmp_repo, env=isolated_env,
    )
    assert result.returncode == 0, result.stderr
    # All three worktrees should still exist (dry-run).
    assert Path(feat_path).is_dir()
    assert Path(disc_path).is_dir()
    assert Path(spec_path).is_dir()
    # feature/feat-stale should appear in the dry-run dispatch; the
    # ephemeral prefixes should NOT.
    stderr = result.stderr
    assert "feature/" in stderr, f"stale mode should dispatch feature/* prefix; stderr={stderr}"
    assert "discover/" not in stderr, "stale mode must skip discover/ prefix"
    assert "speculate/" not in stderr, "stale mode must skip speculate/ prefix"
