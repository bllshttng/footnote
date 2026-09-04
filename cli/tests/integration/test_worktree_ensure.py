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


def _bare_origin(main_repo: Path, tmp_path: Path) -> Path:
    """Bare origin with `main` pushed from main_repo; returns the origin path."""
    origin = tmp_path / "origin.git"
    subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(origin)], check=True)
    _git("remote", "add", "origin", str(origin), cwd=main_repo)
    _git("push", "-q", "origin", "main", cwd=main_repo)
    return origin


def test_ensure_continues_origin_feature_branch_ahead_of_main(
    main_repo: Path, tmp_path: Path
) -> None:
    """AC2-HP (x-28ff): origin/feature/<name> ahead of main -> the new worktree
    tracks that branch at its tip, and the receipt names it as continued."""
    origin = _bare_origin(main_repo, tmp_path)
    _git("checkout", "-qb", "feature/x-1234", cwd=main_repo)
    (main_repo / "a.txt").write_text("one\n")
    _git("add", "a.txt", cwd=main_repo)
    _git("commit", "-qm", "one", cwd=main_repo)
    (main_repo / "b.txt").write_text("two\n")
    _git("add", "b.txt", cwd=main_repo)
    _git("commit", "-qm", "two", cwd=main_repo)
    _git("push", "-q", "origin", "feature/x-1234", cwd=main_repo)
    tip = _git("rev-parse", "feature/x-1234", cwd=main_repo).stdout.strip()
    # Delete the LOCAL branch: the continued path fires when only origin has
    # the work (a fresh checkout, or a branch that never came back). A local
    # branch present in the canonical repo takes today's checkout path.
    _git("checkout", "-q", "main", cwd=main_repo)
    _git("branch", "-qD", "feature/x-1234", cwd=main_repo)

    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "x-1234"])
    assert res.exit_code == 0, res.stderr
    wt = Path(res.stdout.strip())
    assert _git("rev-parse", "HEAD", cwd=wt).stdout.strip() == tip
    upstream = _git("rev-parse", "--abbrev-ref", "feature/x-1234@{upstream}", cwd=wt).stdout.strip()
    assert upstream == "origin/feature/x-1234"
    assert "base=continued:origin/feature/x-1234:+2" in res.stderr


def test_ensure_salvages_remote_salvage_ref_when_no_branch(
    main_repo: Path, tmp_path: Path
) -> None:
    """AC3-HP (x-28ff): only refs/fno/salvage/<name> on origin ahead of main ->
    the new branch starts at that commit and the receipt names it as salvaged."""
    _bare_origin(main_repo, tmp_path)
    _git("checkout", "-qb", "spill", cwd=main_repo)
    (main_repo / "c.txt").write_text("salvage me\n")
    _git("add", "c.txt", cwd=main_repo)
    _git("commit", "-qm", "unpushed work", cwd=main_repo)
    spill_tip = _git("rev-parse", "HEAD", cwd=main_repo).stdout.strip()
    _git("push", "-q", "origin", f"{spill_tip}:refs/fno/salvage/x-5555", cwd=main_repo)
    _git("checkout", "-q", "main", cwd=main_repo)

    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "x-5555"])
    assert res.exit_code == 0, res.stderr
    wt = Path(res.stdout.strip())
    assert _git("rev-parse", "HEAD", cwd=wt).stdout.strip() == spill_tip
    assert "base=salvaged:refs/fno/salvage/x-5555:+1" in res.stderr


def test_ensure_merged_branch_cuts_fresh(main_repo: Path, tmp_path: Path) -> None:
    """AC4-EDGE (x-28ff): the branch exists on origin but carries zero commits
    ahead of main (it merged) -> fresh cut from origin/main, and the receipt
    says so. Name existence never reads as ahead."""
    origin = _bare_origin(main_repo, tmp_path)
    _git("checkout", "-qb", "feature/x-7777", cwd=main_repo)
    (main_repo / "d.txt").write_text("merged later\n")
    _git("add", "d.txt", cwd=main_repo)
    _git("commit", "-qm", "merged later", cwd=main_repo)
    _git("push", "-q", "origin", "feature/x-7777", cwd=main_repo)
    _git("checkout", "-q", "main", cwd=main_repo)
    _git("merge", "-q", "--ff-only", "feature/x-7777", cwd=main_repo)
    _git("push", "-q", "origin", "main", cwd=main_repo)
    # Only origin keeps the (now merged) branch, so ensure resolves the
    # remote refs instead of today's local-branch checkout path.
    _git("branch", "-qD", "feature/x-7777", cwd=main_repo)
    main_tip = _git("rev-parse", "origin/main", cwd=main_repo).stdout.strip()
    del origin

    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "x-7777"])
    assert res.exit_code == 0, res.stderr
    wt = Path(res.stdout.strip())
    assert _git("rev-parse", "HEAD", cwd=wt).stdout.strip() == main_tip
    assert "base=fresh:origin/main" in res.stderr
    assert "continued" not in res.stderr


def test_ensure_unreachable_origin_cuts_fresh_and_names_failure(
    main_repo: Path, tmp_path: Path
) -> None:
    """AC4-EDGE (x-28ff): the fetch cannot reach origin at all -> fresh cut,
    and the receipt's stderr names the fetch failure instead of silently
    blessing a possibly stale main."""
    origin = _bare_origin(main_repo, tmp_path)
    origin.rename(tmp_path / "origin-gone")  # unreachable: fetch must fail

    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "x-9999"])
    assert res.exit_code == 0, res.stderr
    wt = Path(res.stdout.strip())
    main_tip = _git("rev-parse", "origin/main", cwd=main_repo).stdout.strip()
    assert _git("rev-parse", "HEAD", cwd=wt).stdout.strip() == main_tip
    assert "base=fresh:origin/main" in res.stderr
    assert "base fetch failed" in res.stderr


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


def test_ensure_never_with_explicit_branch_refuses(main_repo: Path, tmp_path: Path) -> None:
    """A never project + an explicit --branch (batch lane) is a contradiction:
    an isolated branch cannot be created in place. Refuse (empty stdout) rather
    than report success on the canonical branch."""
    _write_config(
        main_repo / ".fno",
        f'[[work.workspaces.default.projects]]\npath = "{main_repo}"\nworktree = "never"\n',
    )
    res = runner.invoke(
        app,
        ["worktree", "ensure", "--repo", str(main_repo), "--name", "b", "--branch", "feature/batch-x"],
    )
    assert res.exit_code != 0
    assert res.stdout.strip() == ""


def test_ensure_honors_fno_config(
    main_repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """FNO_CONFIG, when set, is the sole config source: a `never` in it wins even
    though the repo-local .fno has no policy (fail-closed parity with the loader)."""
    cfg = tmp_path / "explicit.toml"
    cfg.write_text(
        f'[[work.workspaces.default.projects]]\npath = "{main_repo}"\nworktree = "never"\n'
    )
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "f"])
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(main_repo.resolve())


def test_ensure_malformed_workspaces_refuses(main_repo: Path, tmp_path: Path) -> None:
    """Fail-closed on gross map corruption: a `work.workspaces` present but the
    wrong type (a scalar, not a table) refuses rather than silently defaulting
    to auto-isolate a project whose `never` entry it can no longer read."""
    _write_config(main_repo / ".fno", '[work]\nworkspaces = "not-a-table"\n')
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "m"])
    assert res.exit_code != 0
    assert res.stdout.strip() == ""
    assert not _default_wt(tmp_path, main_repo, "m").exists()


def test_ensure_project_absent_falls_to_default(main_repo: Path, tmp_path: Path) -> None:
    """AC1-EDGE: a repo absent from the workspaces map falls to the default
    (harness-native under claude): a worktree IS created at the harness location."""
    _write_config(
        main_repo / ".fno",
        '[[work.workspaces.default.projects]]\npath = "/some/other/repo"\nworktree = "never"\n',
    )
    res = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "d", "--harness", "claude"]
    )
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(main_repo / ".claude" / "worktrees" / "d")


def test_ensure_external_worktrees_base_set_lands_under_base(
    main_repo: Path, tmp_path: Path
) -> None:
    """With `worktree.policy = external`, a configured paths.worktrees_base lands
    the worktree at <base>/<repo>/<name> (the base only governs the external mode;
    harness-native ignores it -- see the next test)."""
    base = tmp_path / "custom-bases"
    _write_config(
        main_repo / ".fno",
        f'[worktree]\npolicy = "external"\n[paths]\nworktrees_base = "{base}"\n',
    )
    res = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "e", "--harness", "claude"]
    )
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(base / main_repo.name / "e")


def test_ensure_harness_native_claude_lands_in_dot_claude(
    main_repo: Path, tmp_path: Path
) -> None:
    """AC2-HP: a claude code payload with no policy config lands harness-native at
    <repo>/.claude/worktrees/<name>, not under the ~/.fno base."""
    res = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "hn", "--harness", "claude"]
    )
    assert res.exit_code == 0, res.stderr
    wt = main_repo / ".claude" / "worktrees" / "hn"
    assert res.stdout.strip() == str(wt)
    assert wt.exists()
    assert not _default_wt(tmp_path, main_repo, "hn").exists()


def test_ensure_harness_native_ignores_worktrees_base(
    main_repo: Path, tmp_path: Path
) -> None:
    """The default flip is absolute: harness-native + claude lands in
    .claude/worktrees/ even when paths.worktrees_base is configured (the base is
    honored only under the `external` policy)."""
    base = tmp_path / "custom-bases"
    _write_config(main_repo / ".fno", f'[paths]\nworktrees_base = "{base}"\n')
    res = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "ib", "--harness", "claude"]
    )
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(main_repo / ".claude" / "worktrees" / "ib")
    assert not (base / main_repo.name / "ib").exists()


def test_ensure_harness_native_reuse(main_repo: Path, tmp_path: Path) -> None:
    """AC2-FR: a re-dispatch reuses the existing harness-native worktree (same
    path, exit 0), never clobbering or duplicating it."""
    args = ["worktree", "ensure", "--repo", str(main_repo), "--name", "ru", "--harness", "claude"]
    first = runner.invoke(app, args)
    assert first.exit_code == 0, first.stderr
    wt = main_repo / ".claude" / "worktrees" / "ru"
    assert first.stdout.strip() == str(wt)
    second = runner.invoke(app, args)
    assert second.exit_code == 0, second.stderr
    assert second.stdout.strip() == str(wt)


def test_ensure_emits_stderr_receipt_naming_mode_and_path(
    main_repo: Path, tmp_path: Path
) -> None:
    """AC: every spawn emits exactly one stderr receipt naming the resolved mode
    and the worktree path - harness-native, external, and the reuse path alike
    (the path itself stays the SOLE stdout line so callers still read it clean)."""
    hn = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "r1", "--harness", "claude"]
    )
    assert hn.exit_code == 0, hn.stderr
    assert "policy=harness-native" in hn.stderr
    assert str(main_repo / ".claude" / "worktrees" / "r1") in hn.stderr
    assert hn.stdout.strip() == str(main_repo / ".claude" / "worktrees" / "r1")

    ext = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "r2"])
    assert ext.exit_code == 0, ext.stderr
    assert "policy=external" in ext.stderr
    assert str(_default_wt(tmp_path, main_repo, "r2")) in ext.stderr

    reuse = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "r1", "--harness", "claude"]
    )
    assert reuse.exit_code == 0, reuse.stderr
    assert "reusing worktree at" in reuse.stderr
    assert reuse.stdout.strip() == str(main_repo / ".claude" / "worktrees" / "r1")


def test_ensure_no_harness_degrades_to_external(main_repo: Path, tmp_path: Path) -> None:
    """AC5-FR: an omitted --harness never guesses a harness location -- the default
    harness-native degrades to external and lands under the ~/.fno base, NOT in
    .claude/worktrees/."""
    res = runner.invoke(app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "nh"])
    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(_default_wt(tmp_path, main_repo, "nh"))
    assert not (main_repo / ".claude" / "worktrees" / "nh").exists()


def test_codex_native_degradation_uses_fno_fallback_not_configured_allocator(
    main_repo: Path, tmp_path: Path
) -> None:
    """Unsupported Codex callers never counterfeit app ownership or inherit an
    external allocator configured for explicit external worktrees."""
    configured = tmp_path / "conductor" / "workspaces"
    _write_config(
        main_repo / ".fno",
        f'[paths]\nworktrees_base = "{configured}"\n',
    )

    res = runner.invoke(
        app,
        ["worktree", "ensure", "--repo", str(main_repo), "--name", "cx", "--harness", "codex"],
    )

    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(_default_wt(tmp_path, main_repo, "cx"))
    assert "requested=harness-native" in res.stderr
    assert "degraded=true" in res.stderr
    assert not (configured / main_repo.name / "cx").exists()


def test_codex_explicit_external_policy_honors_configured_allocator(
    main_repo: Path, tmp_path: Path
) -> None:
    configured = tmp_path / "conductor" / "workspaces"
    _write_config(
        main_repo / ".fno",
        f'[worktree]\npolicy = "external"\n[paths]\nworktrees_base = "{configured}"\n',
    )

    res = runner.invoke(
        app,
        ["worktree", "ensure", "--repo", str(main_repo), "--name", "cx-ext", "--harness", "codex"],
    )

    assert res.exit_code == 0, res.stderr
    assert res.stdout.strip() == str(configured / main_repo.name / "cx-ext")
    assert "degraded=true" not in res.stderr


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


def test_ensure_reuses_the_branch_checkout_when_the_policy_path_moves(
    main_repo: Path, tmp_path: Path
) -> None:
    """A relocated policy path must reuse the branch's existing checkout.

    A branch has exactly one checkout. When the resolved path moves under an
    existing lane (a node whose harness stopped resolving to claude, or a
    `worktrees_base` edit), `git worktree add` refuses with "already used by
    worktree at ..." and the lane wedges on every tick, because its tree is
    unmerged and nothing reaps it.
    """
    first = runner.invoke(
        app, ["worktree", "ensure", "--repo", str(main_repo), "--name", "lane-a"]
    )
    assert first.exit_code == 0, first.stderr
    original = Path(first.stdout.strip())
    assert original == _default_wt(tmp_path, main_repo, "lane-a")

    # Same node, a harness whose policy resolves somewhere else.
    moved = runner.invoke(
        app,
        ["worktree", "ensure", "--repo", str(main_repo), "--name", "lane-a",
         "--harness", "claude"],
    )
    assert moved.exit_code == 0, moved.stderr
    assert Path(moved.stdout.strip()) == original
    assert "a branch has one checkout" in moved.stderr
