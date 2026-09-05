"""The `fno do target start` init-failure receipt (orphan-worktree naming).

A cold start that creates a worktree and then fails init used to leave the
tree on disk holding an init-time manifest and no claim - indistinguishable
from a live session until someone read the lockfile. The receipt names the
tree, the exit code, and the reclaim verb; a reused tree gets no creation
claim. Lives under cli/tests (not beside the verb's own tests in src) purely
for the file-budget allowance; the stubs mirror test_target_start.py's
ordinary-path set."""

from __future__ import annotations

import subprocess
from pathlib import Path

from typer.testing import CliRunner

from fno import target_cli
from fno.target_cli import target_app

runner = CliRunner()


def _ordinary_start_stubs(monkeypatch, canonical, wt):
    monkeypatch.chdir(canonical)
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n, entries=None: n)

    def _git_out_stub(cwd, *args):
        if args == ("rev-parse", "--show-toplevel"):
            return str(canonical)
        if args == ("rev-parse", "--verify", "origin/main^{commit}"):
            return "base-sha-0001"
        return None

    monkeypatch.setattr(target_cli, "_git_out", _git_out_stub)
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, path: (0, "")
    )


def _worktree_fixture(tmp_path: Path):
    canonical = tmp_path / "footnote"
    wt = tmp_path / ".claude" / "worktrees" / "x-0b3f"
    canonical.mkdir()
    wt.mkdir(parents=True)
    return canonical, wt


def test_start_init_failure_names_the_orphan_worktree_and_reclaim_verb(
    monkeypatch, tmp_path
):
    """A tree THIS invocation created, whose init then refused: stderr carries
    one receipt line naming the worktree path, the init exit code, and the
    reclaim verb. The tree is not deleted - a later reader must be able to
    tell an abandoned run from a live one without archaeology."""
    canonical, wt = _worktree_fixture(tmp_path)
    _ordinary_start_stubs(monkeypatch, canonical, wt)

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(
                args, 0, stdout=str(wt), stderr="worktree ensure: worktree at x"
            )
        if "init" in args:
            return subprocess.CompletedProcess(args, 3, stdout="", stderr="")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 3
    assert f"worktree at {wt} is created but unclaimed" in result.output
    assert (
        f"reclaim with: fno agents workspace worktree archive {wt}" in result.output
    )


def test_start_init_failure_on_a_reused_tree_makes_no_creation_claim(
    monkeypatch, tmp_path
):
    """A tree that existed before the invocation (ensure said reusing) gets no
    created-here receipt: this run did not create it, and characterizing it
    as ours would misdirect the cleanup."""
    canonical, wt = _worktree_fixture(tmp_path)
    _ordinary_start_stubs(monkeypatch, canonical, wt)

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(
                args,
                0,
                stdout=str(wt),
                stderr=f"worktree ensure: reusing worktree at {wt}",
            )
        if "init" in args:
            return subprocess.CompletedProcess(args, 3, stdout="", stderr="")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 3
    assert "created but unclaimed" not in result.output
    assert "worktree archive" not in result.output
    assert "predates this run" in result.output


def test_start_reads_the_explicit_created_token_not_the_wording(
    monkeypatch, tmp_path
):
    """The token decides, not the receipt's prose: created=false with no
    'reusing' anywhere still reads as pre-existing, and created=true with
    'reusing' still reads as created. The substring fallback only fires for
    an installed ensure that predates the token."""
    canonical, wt = _worktree_fixture(tmp_path)
    _ordinary_start_stubs(monkeypatch, canonical, wt)

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(
                args,
                0,
                stdout=str(wt),
                stderr=f"worktree ensure: worktree at {wt} created=false",
            )
        if "init" in args:
            return subprocess.CompletedProcess(args, 3, stdout="", stderr="")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 3
    assert "created but unclaimed" not in result.output
    assert "predates this run" in result.output
