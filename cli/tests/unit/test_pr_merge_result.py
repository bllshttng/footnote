"""The merge-result probe against a real tmp repo carrying the 1654 specimen.

The specimen shape: commit A defines ``CONST`` at the top of ``mod.py`` and an
unrelated function at the bottom. ``main`` adds a use of ``CONST`` between
them; ``pr`` deletes the definition. Each branch is green alone, the merge
tree is red with F821, and no git operation ever reports a conflict.
"""
import subprocess
from pathlib import Path

from fno.pr import _merge_result

SCRIPT = Path(__file__).parents[3] / "scripts" / "ci" / "check-python-static.sh"


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
    scripts = repo / "scripts" / "ci"
    scripts.mkdir(parents=True)
    (scripts / "check-python-static.sh").write_text(SCRIPT.read_text())
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


def test_both_parents_green_and_merge_red(tmp_path: Path) -> None:
    repo = _specimen_repo(tmp_path)
    for branch in ("main", "pr"):
        tree = _git(repo, "rev-parse", f"{branch}^{{tree}}")
        verdict, reason = _merge_result.static_verdict_for_tree(tree, str(repo))
        assert verdict == "ok", f"{branch} must be green alone: {reason}"
    tree, reason = _merge_result.merge_tree("main", "pr", str(repo))
    assert tree, reason
    verdict, red = _merge_result.static_verdict_for_tree(tree, str(repo))
    assert verdict == "red"
    assert "F821" in red
    assert "CONST" in red
    assert "cli/src/pkg/mod.py" in red


def test_head_already_contains_base_skips_static(tmp_path: Path, monkeypatch) -> None:
    repo = _specimen_repo(tmp_path)
    _git(repo, "checkout", "-q", "pr")
    _git(repo, "merge", "-q", "--no-edit", "main")
    _bare_origin(repo, tmp_path)
    _git(repo, "push", "-q", "origin", "main")
    head = _git(repo, "rev-parse", "pr")
    monkeypatch.setattr(_merge_result, "_gh_pr_refs", lambda pr, cwd: ("main", head))
    monkeypatch.setattr(_merge_result, "_fetch_pull_head", lambda pr, cwd: head)
    calls: list[str] = []

    def _spy(tree: str, cwd: str) -> tuple[str, str]:
        calls.append(tree)
        return ("ok", "static step should never run")

    monkeypatch.setattr(_merge_result, "static_verdict_for_tree", _spy)
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
    tree, reason = _merge_result.merge_tree("origin/main", head, str(repo))
    assert not tree
    assert "cli/src/pkg/mod.py" in reason
    rc = _merge_result.run_merge_result_check(7, str(repo))
    assert rc == _merge_result.REFUSED_RED
    assert "mod.py" in capsys.readouterr().err


def test_dead_probes_answer_unknown_exit_4(tmp_path: Path, monkeypatch, capsys) -> None:
    monkeypatch.setattr(_merge_result, "_probe", lambda args, cwd: None)
    rc = _merge_result.run_merge_result_check(1, str(tmp_path))
    assert rc == _merge_result.UNKNOWN
    assert "merge-result: unknown" in capsys.readouterr().err
