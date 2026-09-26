"""The merge-result probe against a real tmp repo carrying the 1654 specimen.

The specimen shape: commit A defines ``CONST`` at the top of ``mod.py`` and an
unrelated function at the bottom. ``main`` adds a use of ``CONST`` between
them; ``pr`` deletes the definition. Each branch is green alone, the merge
tree is red with F821, and no git operation ever reports a conflict.
"""
import os
import re
import subprocess
from pathlib import Path

from fno.pr import _merge_result

SCRIPT = Path(__file__).parents[3] / "scripts" / "ci" / "check-merge-result.sh"


def _git(repo: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


def _commit(repo: Path, message: str) -> None:
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", message)


def _run_script(repo: Path, base: str, head: str) -> subprocess.CompletedProcess:
    # The extracted tree has no venv, so the bare venv tools stand in for the
    # caller's uv-run forms.
    return subprocess.run(
        ["bash", str(SCRIPT), str(repo), base, head],
        capture_output=True,
        text=True,
        env={**os.environ, "RUFF": "ruff", "MYPY": "mypy"},
        check=False,
    )


MOD_USES_CONST = "CONST = 1\n\n\ndef use():\n    return CONST\n\n\ndef unrelated():\n    pass\n"
MOD_DROPS_CONST = "\n\ndef unrelated():\n    pass\n"


def _specimen_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    repo.mkdir()
    _git(repo, "init", "-q", "-b", "main")
    _git(repo, "config", "user.email", "test@example.com")
    _git(repo, "config", "user.name", "test")
    cli = repo / "cli"
    (cli / "src" / "pkg").mkdir(parents=True)
    (cli / "pyproject.toml").write_text("[project]\nname = 'pkg'\nversion = '0'\n\n[tool.ruff]\n\n[tool.mypy]\n")
    (cli / "src" / "pkg" / "__init__.py").write_text("")
    mod = cli / "src" / "pkg" / "mod.py"
    mod.write_text("CONST = 1\n\n\ndef unrelated():\n    pass\n")
    root = Path(__file__).parents[3]
    scripts = repo / "scripts" / "ci"
    scripts.mkdir(parents=True)
    for name in ("check-merge-result.sh", "check-python-static.sh"):
        (scripts / name).write_text((root / "scripts" / "ci" / name).read_text())
    _commit(repo, "A: constant defined, used by nobody")
    mod.write_text(MOD_USES_CONST)
    _commit(repo, "main uses CONST")
    _git(repo, "checkout", "-q", "-b", "pr", "main~1")
    mod.write_text(MOD_DROPS_CONST)
    _commit(repo, "pr drops CONST")
    return repo


def _bare_origin(repo: Path, tmp_path: Path) -> None:
    bare = tmp_path / "origin.git"
    _git(tmp_path, "init", "-q", "--bare", bare.name)
    _git(repo, "remote", "add", "origin", str(bare))


def _preamble_repo(tmp_path: Path) -> Path:
    """Two branches off one base, each appending 20 bytes to a preamble file.

    The ceiling sits 30 bytes over the base total, so either branch alone is
    under it and main plus both is over: the collision the merge-time step
    exists to refuse. No cli/ in the fixture, so the step ordering before the
    no-cli early exit is proven too.
    """
    repo = tmp_path / "preamble"
    repo.mkdir()
    _git(repo, "init", "-q", "-b", "main")
    _git(repo, "config", "user.email", "test@example.com")
    _git(repo, "config", "user.name", "test")
    (repo / "AGENTS.md").write_text("agents preamble\n")
    (repo / "CLAUDE.md").write_text("claude preamble\n")
    skill = repo / "skills" / "using-fno"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("body, no frontmatter\n")
    root = Path(__file__).parents[3]
    scripts = repo / "scripts" / "ci"
    scripts.mkdir(parents=True)
    for name in ("check-merge-result.sh", "check-python-static.sh", "check-preamble-budget.sh"):
        (scripts / name).write_text((root / "scripts" / "ci" / name).read_text())
    total = sum(len(p.read_bytes()) for p in (repo / "AGENTS.md", repo / "CLAUDE.md", skill / "SKILL.md"))
    gate = scripts / "check-preamble-budget.sh"
    gate.write_text(
        re.sub(r"(?m)^DESCRIPTIONS_CEILING_BYTES=\d+$", "DESCRIPTIONS_CEILING_BYTES=0",
               re.sub(r"(?m)^CEILING_BYTES=\d+$", f"CEILING_BYTES={total + 30}", gate.read_text()))
    )
    _commit(repo, "base: preamble corpus and gate scripts")
    base = _git(repo, "rev-parse", "HEAD")
    _git(repo, "checkout", "-q", "-b", "a")
    (repo / "AGENTS.md").write_text("agents preamble\n" + "a" * 20 + "\n")
    _commit(repo, "a: 20 bytes on AGENTS.md")
    _git(repo, "checkout", "-q", "main")
    _git(repo, "merge", "-q", "--no-edit", "a")
    _git(repo, "checkout", "-q", "-b", "b", base)
    (repo / "CLAUDE.md").write_text("claude preamble\n" + "b" * 20 + "\n")
    _commit(repo, "b: 20 bytes on CLAUDE.md")
    return repo


def test_both_parents_green_and_merge_red(tmp_path: Path) -> None:
    repo = _specimen_repo(tmp_path)
    for branch in ("main", "pr"):
        proc = _run_script(repo, "main~1", branch)
        assert proc.returncode == 0, f"{branch} must be green alone: {proc.stdout}{proc.stderr}"
    proc = _run_script(repo, "main", "pr")
    assert proc.returncode == _merge_result.REFUSED_RED
    assert "F821" in proc.stdout
    assert "CONST" in proc.stdout
    assert "cli/src/pkg/mod.py" in proc.stdout


def test_preamble_budget_runs_on_main_plus_the_pr(tmp_path: Path) -> None:
    repo = _preamble_repo(tmp_path)
    for branch in ("a", "b"):
        proc = _run_script(repo, "main~1", branch)
        assert proc.returncode == 0, f"{branch} alone is under the ceiling: {proc.stdout}{proc.stderr}"
        assert "preamble:" in proc.stdout
    proc = _run_script(repo, "main", "b")
    assert proc.returncode == _merge_result.REFUSED_RED
    assert "preamble" in proc.stdout
    assert "-byte ceiling" in proc.stdout


def test_merge_tree_without_the_preamble_script_skips_the_step(tmp_path: Path) -> None:
    repo = _specimen_repo(tmp_path)
    proc = _run_script(repo, "main", "pr")
    assert proc.returncode == _merge_result.REFUSED_RED
    assert "F821" in proc.stdout
    assert "preamble" not in proc.stdout


def test_head_already_contains_base_skips_the_script(tmp_path: Path, monkeypatch) -> None:
    repo = _specimen_repo(tmp_path)
    _git(repo, "checkout", "-q", "pr")
    _git(repo, "merge", "-q", "--no-edit", "main")
    _bare_origin(repo, tmp_path)
    _git(repo, "push", "-q", "origin", "main")
    head = _git(repo, "rev-parse", "pr")
    monkeypatch.setattr(_merge_result, "_gh_pr_refs", lambda pr, cwd: ("main", head))
    calls: list[tuple] = []

    def _spy(top, base_rev, head_oid, cwd):
        calls.append((top, base_rev, head_oid))
        return ("ok", "the script should never run")

    monkeypatch.setattr(_merge_result, "_run_script", _spy)
    verdict, reason = _merge_result.merge_result_verdict(1, str(repo))
    assert verdict == "ok"
    assert "already contains" in reason
    assert calls == []
    assert _merge_result.run_merge_result_check(1, str(repo)) == _merge_result.OK


def test_textual_conflict_refuses_with_exit_3(tmp_path: Path, monkeypatch, capsys) -> None:
    repo = _specimen_repo(tmp_path)
    _git(repo, "checkout", "-q", "main")
    mod = repo / "cli" / "src" / "pkg" / "mod.py"
    mod.write_text("CONST = 2\n\n\ndef use():\n    return CONST\n\n\ndef unrelated():\n    pass\n")
    _commit(repo, "main rewrites the first line")
    _git(repo, "checkout", "-q", "pr")
    mod.write_text("GONE = 1\n\n\ndef unrelated():\n    pass\n")
    _commit(repo, "pr rewrites the first line")
    _bare_origin(repo, tmp_path)
    _git(repo, "push", "-q", "origin", "main")
    _git(repo, "push", "-q", "origin", "pr")
    head = _git(repo, "rev-parse", "pr")
    monkeypatch.setattr(_merge_result, "_gh_pr_refs", lambda pr, cwd: ("main", head))
    monkeypatch.setattr(_merge_result, "_fetch_pull_head", lambda pr, cwd: head)
    rc = _merge_result.run_merge_result_check(7, str(repo))
    assert rc == _merge_result.REFUSED_RED
    err = capsys.readouterr().err
    assert "REFUSED" in err
    assert "cli/src/pkg/mod.py" in err


def test_dead_probes_answer_unknown_exit_4(tmp_path: Path, monkeypatch, capsys) -> None:
    monkeypatch.setattr(_merge_result, "_probe", lambda args, cwd: None)
    rc = _merge_result.run_merge_result_check(1, str(tmp_path))
    assert rc == _merge_result.UNKNOWN
    assert "merge-result: unknown" in capsys.readouterr().err
