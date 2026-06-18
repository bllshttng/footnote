"""Characterization tests for the _rebase.py port (ab-d4c98550, US3/AC5).

Exercises the exit-code protocol (0 clean / 1 failed|refused / 2 dirty /
3 protected / 42 needs_resolver) against REAL temp git repos, plus the
guardrail unit table and the phase-B --continue resume cycle. The exit codes
are the load-bearing contract a caller branches on, so they are pinned here.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.pr import _rebase


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=cwd, text=True, capture_output=True, check=True
    ).stdout


def _init_repo_with_origin(tmp_path: Path) -> Path:
    """A work repo whose ``origin`` is a local bare remote with a main branch."""
    bare = tmp_path / "origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True)
    work = tmp_path / "work"
    subprocess.run(["git", "init", "-b", "main", str(work)], check=True)
    _git(work, "config", "user.email", "t@t.t")
    _git(work, "config", "user.name", "t")
    (work / "base.txt").write_text("base\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "init")
    _git(work, "remote", "add", "origin", str(bare))
    _git(work, "push", "-u", "origin", "main")
    return work


def _emitted_json(capsys) -> dict:
    out = capsys.readouterr().out.strip().splitlines()
    return json.loads(out[-1])


def test_clean_rebase_exits_0(tmp_path, capsys):
    work = _init_repo_with_origin(tmp_path)
    # Advance origin/main with a non-conflicting file.
    _git(work, "checkout", "-b", "tmp")
    (work / "added-on-main.txt").write_text("x\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "main advances")
    _git(work, "push", "origin", "tmp:main")
    # Feature branch off the old main, touching a different file.
    _git(work, "checkout", "main")
    _git(work, "checkout", "-b", "feature/x")
    (work / "feature.txt").write_text("f\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "feature work")

    rc = _rebase.run_rebase(["--base=origin/main"], cwd=str(work))
    assert rc == 0
    obj = _emitted_json(capsys)
    assert obj["status"] == "clean"
    assert obj["base"] == "origin/main"


def test_protected_branch_exits_3(tmp_path, capsys):
    work = _init_repo_with_origin(tmp_path)  # on main
    rc = _rebase.run_rebase(["--base=origin/main"], cwd=str(work))
    assert rc == 3
    assert _emitted_json(capsys)["status"] == "refused"


def test_dirty_tree_exits_2(tmp_path, capsys):
    work = _init_repo_with_origin(tmp_path)
    _git(work, "checkout", "-b", "feature/dirty")
    (work / "base.txt").write_text("uncommitted change\n")
    rc = _rebase.run_rebase(["--base=origin/main"], cwd=str(work))
    assert rc == 2
    assert _emitted_json(capsys)["status"] == "dirty"


def _make_conflict(tmp_path: Path, conflict_file: str = "base.txt") -> Path:
    """Build a feature branch that conflicts with an advanced origin/main."""
    work = _init_repo_with_origin(tmp_path)
    # origin/main edits the file.
    _git(work, "checkout", "-b", "tmp")
    (work / conflict_file).write_text("main side\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "main edit")
    _git(work, "push", "origin", "tmp:main")
    # feature edits the same file differently.
    _git(work, "checkout", "main")
    _git(work, "checkout", "-b", "feature/conflict")
    (work / conflict_file).write_text("feature side\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "feature edit")
    return work


def test_conflict_needs_resolver_exits_42(tmp_path, capsys, monkeypatch):
    monkeypatch.setattr(_rebase, "_conflict_resolution", lambda: "opus")
    work = _make_conflict(tmp_path)
    rc = _rebase.run_rebase(["--base=origin/main"], cwd=str(work))
    assert rc == 42
    obj = _emitted_json(capsys)
    assert obj["status"] == "needs_resolver"
    assert "base.txt" in obj["files"]
    # The rebase is left in-progress for the agent (not aborted).
    assert (Path(work) / ".git" / "rebase-merge").exists() or (
        Path(work) / ".git" / "rebase-apply"
    ).exists()


def test_conflict_resolution_fail_exits_1(tmp_path, capsys, monkeypatch):
    monkeypatch.setattr(_rebase, "_conflict_resolution", lambda: "fail")
    work = _make_conflict(tmp_path)
    rc = _rebase.run_rebase(["--base=origin/main"], cwd=str(work))
    assert rc == 1
    obj = _emitted_json(capsys)
    assert obj["status"] == "failed"
    assert "conflict_resolution=fail" in obj["reason"]


def test_guardrail_lockfile_refuses_exit_1(tmp_path, capsys, monkeypatch):
    monkeypatch.setattr(_rebase, "_conflict_resolution", lambda: "opus")
    work = _make_conflict(tmp_path, conflict_file="uv.lock")
    rc = _rebase.run_rebase(["--base=origin/main"], cwd=str(work))
    assert rc == 1
    obj = _emitted_json(capsys)
    assert obj["status"] == "refused"
    assert "lock file" in obj["reason"]


def test_phase_b_continue_resolves_exit_0(tmp_path, capsys, monkeypatch):
    monkeypatch.setattr(_rebase, "_conflict_resolution", lambda: "opus")
    work = _make_conflict(tmp_path)
    # Phase A leaves an in-progress rebase at exit 42.
    assert _rebase.run_rebase(["--base=origin/main"], cwd=str(work)) == 42
    capsys.readouterr()  # drain
    # The "agent" resolves + stages the conflict (stage-only: --continue must
    # finalise the commit non-interactively, the headless-editor regression).
    (Path(work) / "base.txt").write_text("resolved\n")
    _git(Path(work), "add", "base.txt")
    rc = _rebase.run_rebase(["--base=origin/main", "--continue"], cwd=str(work))
    assert rc == 0
    assert _emitted_json(capsys)["status"] == "resolved"


def test_phase_b_continue_more_conflicts_exits_42(tmp_path, capsys, monkeypatch):
    """AC3-EDGE / gemini HIGH on #524: in a multi-commit rebase, --continue
    exits non-zero when it pauses on a NEW conflict in a later commit; the
    port must report needs_resolver (42), not failed (1)."""
    monkeypatch.setattr(_rebase, "_conflict_resolution", lambda: "opus")
    bare = tmp_path / "origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True)
    work = tmp_path / "work"
    subprocess.run(["git", "init", "-b", "main", str(work)], check=True)
    _git(work, "config", "user.email", "t@t.t")
    _git(work, "config", "user.name", "t")
    (work / "f1.txt").write_text("base1\n")
    (work / "f2.txt").write_text("base2\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "init")
    _git(work, "remote", "add", "origin", str(bare))
    _git(work, "push", "-u", "origin", "main")
    # origin/main edits BOTH files.
    _git(work, "checkout", "-b", "tmp")
    (work / "f1.txt").write_text("main1\n")
    (work / "f2.txt").write_text("main2\n")
    _git(work, "add", "-A")
    _git(work, "commit", "-m", "main edits both")
    _git(work, "push", "origin", "tmp:main")
    # feature: commit A edits f1, commit B edits f2 (each conflicts).
    _git(work, "checkout", "main")
    _git(work, "checkout", "-b", "feature/multi")
    (work / "f1.txt").write_text("feat1\n")
    _git(work, "add", "f1.txt")
    _git(work, "commit", "-m", "feature A: f1")
    (work / "f2.txt").write_text("feat2\n")
    _git(work, "add", "f2.txt")
    _git(work, "commit", "-m", "feature B: f2")
    # Phase A: commit A conflicts -> 42.
    assert _rebase.run_rebase(["--base=origin/main"], cwd=str(work)) == 42
    capsys.readouterr()
    # Resolve f1, stage, --continue -> applies B -> f2 conflict -> 42 (not 1).
    (work / "f1.txt").write_text("resolved1\n")
    _git(work, "add", "f1.txt")
    rc = _rebase.run_rebase(["--base=origin/main", "--continue"], cwd=str(work))
    assert rc == 42
    obj = _emitted_json(capsys)
    assert obj["status"] == "needs_resolver"
    assert "f2.txt" in obj["files"]


# ---- guardrail unit table ----


@pytest.mark.parametrize(
    "path,expect_refused",
    [
        ("supabase/migrations/001_x.sql", True),
        ("app/migrations/0001.py", True),
        ("schema.prisma", True),
        (".env", True),
        (".env.local", True),
        ("config/secrets/key.pem", True),
        ("package-lock.json", True),
        ("Cargo.lock", True),
        (".gitignore", True),
        (".gitattributes", True),
        ("src/app/main.py", False),
        ("README.md", False),
    ],
)
def test_guardrail_classification(tmp_path, path, expect_refused):
    refused = _rebase.check_guardrails([path], cwd=str(tmp_path))
    assert (refused is not None) == expect_refused


def test_guardrail_mass_conflict_refused(tmp_path):
    f = tmp_path / "big.py"
    f.write_text("\n".join(["<<<<<<< HEAD"] * 4))
    refused = _rebase.check_guardrails(["big.py"], cwd=str(tmp_path))
    assert refused is not None
    assert "mass conflicts" in refused["reason"]


def test_guardrail_three_markers_allowed(tmp_path):
    f = tmp_path / "small.py"
    f.write_text("\n".join(["<<<<<<< HEAD"] * 3))
    assert _rebase.check_guardrails(["small.py"], cwd=str(tmp_path)) is None
