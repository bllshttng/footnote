"""Integration tests for `fno worktree ensure` (node x-73ca).

The verb is a mechanism-only primitive: given a repo MAIN checkout and a name,
it idempotently creates `<worktrees_base>/<repo>/<name>` (branched off
origin/main, not the dispatcher's local HEAD) and prints the path on stdout.
On any failure it exits non-zero and prints NOTHING on stdout, so a caller
doing `wt=$(fno worktree ensure ...)` falls back to its prior cwd and the
dispatch is never blocked.

HOME is pinned to a per-test temp dir so the default worktree base
(`~/.fno/worktrees`) lands in the sandbox. Real git repos are used because the
gitdir/common-dir distinction (main checkout vs linked worktree) is a
filesystem fact.
"""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

pytestmark = pytest.mark.skipif(shutil.which("git") is None, reason="git required")

runner = CliRunner()


def _git(*args: str, cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-c", "user.email=t@t", "-c", "user.name=t", *args],
        cwd=str(cwd), capture_output=True, text=True, check=True,
    )


@pytest.fixture
def main_repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A real git main checkout on `main` with one commit; HOME pinned to sandbox."""
    monkeypatch.setenv("HOME", str(tmp_path))
    main = tmp_path / "myrepo"
    main.mkdir()
    _git("init", "-q", "-b", "main", cwd=main)
    (main / "README.md").write_text("# repo\n")
    _git("add", "README.md", cwd=main)
    _git("commit", "-qm", "init", cwd=main)
    return main


def _default_wt(home: Path, repo: Path, name: str) -> Path:
    """Default worktree location: ~/.fno/worktrees/<repo>/<name> (no config)."""
    return home / ".fno" / "worktrees" / repo.name / name


def test_ensure_creates_worktree_and_prints_path(main_repo: Path, tmp_path: Path) -> None:
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "agent-a"])
    assert res.exit_code == 0, res.stderr
    wt = _default_wt(tmp_path, main_repo, "agent-a")
    assert res.stdout.strip() == str(wt)
    assert wt.is_dir()
    # It is a real, registered worktree of the repo.
    toplevel = _git("rev-parse", "--show-toplevel", cwd=wt).stdout.strip()
    assert Path(toplevel).resolve() == wt.resolve()


def test_ensure_branches_from_origin_main_not_local_head(
    main_repo: Path, tmp_path: Path
) -> None:
    """AC1-HP / Locked Decision 5: the new branch is based on origin/main, NOT
    the dispatcher's (possibly stale, ahead) local HEAD."""
    origin = tmp_path / "origin.git"
    subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(origin)], check=True)
    _git("remote", "add", "origin", str(origin), cwd=main_repo)
    _git("push", "-q", "origin", "main", cwd=main_repo)
    origin_main_sha = _git("rev-parse", "HEAD", cwd=main_repo).stdout.strip()
    # Advance local main ahead of origin/main.
    (main_repo / "extra.txt").write_text("ahead\n")
    _git("add", "extra.txt", cwd=main_repo)
    _git("commit", "-qm", "ahead of origin", cwd=main_repo)
    local_head = _git("rev-parse", "HEAD", cwd=main_repo).stdout.strip()
    assert local_head != origin_main_sha

    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "agent-b"])
    assert res.exit_code == 0, res.stderr
    wt = Path(res.stdout.strip())
    wt_head = _git("rev-parse", "HEAD", cwd=wt).stdout.strip()
    assert wt_head == origin_main_sha  # based on origin/main, not local HEAD


def test_ensure_idempotent_reuse(main_repo: Path, tmp_path: Path) -> None:
    """AC1-EDGE: a second ensure for the same name reuses the worktree."""
    first = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "dup"])
    assert first.exit_code == 0
    wt = first.stdout.strip()
    before = _git("worktree", "list", "--porcelain", cwd=main_repo).stdout

    second = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "dup"])
    assert second.exit_code == 0
    assert second.stdout.strip() == wt
    after = _git("worktree", "list", "--porcelain", cwd=main_repo).stdout
    # No second worktree created.
    assert before.count("worktree ") == after.count("worktree ")


def test_ensure_stray_dir_non_clobber(main_repo: Path, tmp_path: Path) -> None:
    """AC1-FR: a same-named NON-worktree dir is never clobbered; verb fails."""
    stray = _default_wt(tmp_path, main_repo, "stray")
    stray.mkdir(parents=True)
    sentinel = stray / "keep.txt"
    sentinel.write_text("do not delete\n")

    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "stray"])
    assert res.exit_code != 0
    assert res.stdout.strip() == ""  # nothing on stdout -> caller falls back
    assert sentinel.read_text() == "do not delete\n"  # untouched


def test_ensure_refuses_linked_worktree(main_repo: Path, tmp_path: Path) -> None:
    """Boundary: a --repo that is itself a linked worktree must not nest."""
    linked = tmp_path / "linked"
    _git("worktree", "add", str(linked), "-b", "side", cwd=main_repo)
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(linked), "--name", "nested"])
    assert res.exit_code != 0
    assert res.stdout.strip() == ""


def test_ensure_non_git_repo_falls_back(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR shape: a non-git --repo exits non-zero with empty stdout."""
    monkeypatch.setenv("HOME", str(tmp_path))
    plain = tmp_path / "notgit"
    plain.mkdir()
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(plain), "--name", "x"])
    assert res.exit_code != 0
    assert res.stdout.strip() == ""


# --- policy gate (x-168b) ---------------------------------------------------


def _write_config(fno_dir: Path, body: str) -> None:
    fno_dir.mkdir(parents=True, exist_ok=True)
    (fno_dir / "config.toml").write_text(body)


def test_ensure_policy_never_returns_repo_root(main_repo: Path, tmp_path: Path) -> None:
    """AC1-HP: a per-project `never` policy launches in place: repo root on
    stdout, exit 0, and NO worktree created anywhere."""
    _write_config(
        main_repo / ".fno",
        f'[[work.workspaces.default.projects]]\npath = "{main_repo}"\nworktree = "never"\n',
    )
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "n"])
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(main_repo.resolve())
    assert not _default_wt(tmp_path, main_repo, "n").exists()


def test_ensure_policy_broken_config_refuses(main_repo: Path, tmp_path: Path) -> None:
    """AC1-ERR: a config.toml that exists but fails to parse refuses creation
    (empty stdout, non-zero) -- fail closed, never auto-isolate on a misconfig."""
    (main_repo / ".fno").mkdir(parents=True, exist_ok=True)
    (main_repo / ".fno" / "config.toml").write_text("this = = broken toml\n")
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "b"])
    assert res.exit_code != 0
    assert res.stdout.strip() == ""
    assert not _default_wt(tmp_path, main_repo, "b").exists()


def test_ensure_policy_out_of_enum_refuses_naming_valid(main_repo: Path, tmp_path: Path) -> None:
    """AC2-ERR: an out-of-enum value (`conductor` is a base, not a mode) refuses
    and names the valid values on stderr."""
    _write_config(
        main_repo / ".fno",
        f'[[work.workspaces.default.projects]]\npath = "{main_repo}"\nworktree = "conductor"\n',
    )
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "c"])
    assert res.exit_code != 0
    assert res.stdout.strip() == ""
    assert "never" in res.stderr and "harness-native" in res.stderr and "external" in res.stderr


def test_ensure_project_absent_falls_to_default(main_repo: Path, tmp_path: Path) -> None:
    """AC1-EDGE: a repo absent from the workspaces map falls to the default
    (harness-native under claude): a worktree IS created."""
    _write_config(
        main_repo / ".fno",
        '[[work.workspaces.default.projects]]\npath = "/some/other/repo"\nworktree = "never"\n',
    )
    res = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "d", "--harness", "claude"]
    )
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(_default_wt(tmp_path, main_repo, "d"))


def test_ensure_worktrees_base_set_lands_under_base(
    main_repo: Path, tmp_path: Path
) -> None:
    """A configured paths.worktrees_base lands the worktree at <base>/<repo>/<name>."""
    base = tmp_path / "custom-bases"
    _write_config(main_repo / ".fno", f'[paths]\nworktrees_base = "{base}"\n')
    res = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "e", "--harness", "claude"]
    )
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(base / main_repo.name / "e")


def test_policy_verb_reports_never_and_default(main_repo: Path, tmp_path: Path) -> None:
    """The read-only `policy` verb shares the resolver: `never` prints bare
    `never`; the default prints `harness-native` + a base line."""
    _write_config(
        main_repo / ".fno",
        f'[[work.workspaces.default.projects]]\npath = "{main_repo}"\nworktree = "never"\n',
    )
    res = runner.invoke(app, ["worktree", "policy", "--repo", str(main_repo)])
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == "never"

    # A fresh repo with no config -> default harness-native under claude.
    other = tmp_path / "other"
    other.mkdir()
    _git("init", "-q", "-b", "main", cwd=other)
    (other / "r").write_text("x")
    _git("add", "r", cwd=other)
    _git("commit", "-qm", "i", cwd=other)
    res2 = runner.invoke(
        app, ["worktree", "policy", "--repo", str(other), "--harness", "claude"]
    )
    assert res2.exit_code == 0, res2.stderr
    lines = res2.stdout.strip().splitlines()
    assert lines[0] == "harness-native"
    assert lines[1].startswith("base=")
