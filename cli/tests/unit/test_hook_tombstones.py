"""hook-tombstones gate: a referenced hook script cannot vanish stubless."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
import typer
import typer.main
from click.testing import CliRunner

from fno import paths
from fno.hook_config import referenced_scripts, stubless_deletions
from fno.lint_cli import lint


def _live_lint_command():
    # Same shape the live CLI resolves (see test_lint_cli.py): lint is a
    # plain-function registry entry wrapped in a one-command Typer.
    sub = typer.Typer(add_completion=False)
    sub.command(name="lint")(lint)
    return typer.main.get_command(sub)


app = _live_lint_command()
runner = CliRunner()

_CONFIG_ENTRY_WITH_GATE = (
    '{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": ['
    '{"type": "command", "command": '
    '"python3 ${CLAUDE_PLUGIN_ROOT}/hooks/demo-gate.sh"}'
    "]}]}}"
)

_CONFIG_ENTRY_WITHOUT_GATE = (
    '{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": ['
    '{"type": "command", "command": '
    '"bash ${PLUGIN_ROOT}/hooks/other-gate.sh"}'
    "]}]}}"
)


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


def _commit_all(repo: Path, message: str) -> None:
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", message)


def _repo_with_hooks(tmp_path: Path) -> tuple[Path, str]:
    repo = tmp_path / "repo"
    repo.mkdir()
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    (repo / "hooks").mkdir()
    (repo / "hooks" / "hooks.json").write_text(_CONFIG_ENTRY_WITH_GATE)
    (repo / "hooks" / "demo-gate.sh").write_text("#!/usr/bin/env bash\nexit 0\n")
    _commit_all(repo, "base: config references demo-gate.sh")
    base = _git(repo, "rev-parse", "HEAD").strip()
    return repo, base


@pytest.fixture(autouse=True)
def _fresh_repo_root_cache():
    paths.resolve_repo_root.cache_clear()
    yield
    paths.resolve_repo_root.cache_clear()


def _invoke(repo: Path, *args: str):
    import os

    old = os.environ.get("FNO_REPO_ROOT")
    os.environ["FNO_REPO_ROOT"] = str(repo)
    try:
        return runner.invoke(app, ["hook-tombstones", *args])
    finally:
        if old is None:
            os.environ.pop("FNO_REPO_ROOT", None)
        else:
            os.environ["FNO_REPO_ROOT"] = old


# --- the reader ---


def test_referenced_scripts_extracts_every_shape() -> None:
    text = json.dumps(
        {
            "a": "uv run --project ${CLAUDE_PLUGIN_ROOT}/cli/pyproject.toml "
            "python3 ${CLAUDE_PLUGIN_ROOT}/hooks/law-gate.py",
            "b": "bash ${PLUGIN_ROOT}/hooks/wrap.sh -- "
            "${PLUGIN_ROOT}/hooks/target.sh",
            "c": "${CODEX_PLUGIN_ROOT}/scripts/save-session.py",
            "d": "plain command with no root variable",
        }
    )
    assert referenced_scripts(text) == {
        "cli/pyproject.toml",
        "hooks/law-gate.py",
        "hooks/wrap.sh",
        "hooks/target.sh",
        "scripts/save-session.py",
    }


def test_stubless_deletions_returns_none_when_git_fails(tmp_path: Path) -> None:
    # Not a git repo: the probe never ran, which is not a zero.
    assert stubless_deletions(tmp_path, "HEAD~1", "HEAD") is None


# --- the gate ---


def test_stubless_deletion_of_referenced_script_fails(tmp_path: Path) -> None:
    repo, base = _repo_with_hooks(tmp_path)
    (repo / "hooks" / "demo-gate.sh").unlink()
    (repo / "hooks" / "hooks.json").write_text(_CONFIG_ENTRY_WITHOUT_GATE)
    _commit_all(repo, "delete script and entry together")

    result = _invoke(repo, "--base", base)

    assert result.exit_code == 1
    assert "hooks/demo-gate.sh" in result.stderr
    assert "stub" in result.stderr


def test_retirement_with_stub_passes(tmp_path: Path) -> None:
    repo, base = _repo_with_hooks(tmp_path)
    (repo / "hooks" / "hooks.json").write_text(_CONFIG_ENTRY_WITHOUT_GATE)
    (repo / "hooks" / "demo-gate.sh").write_text(
        "#!/usr/bin/env bash\n# retired: no-op stub, delete next release\nexit 0\n"
    )
    _commit_all(repo, "remove entry, keep stub")

    result = _invoke(repo, "--base", base)

    assert result.exit_code == 0
    assert "0 deleted" in result.stdout


def test_unreferenced_deletion_passes(tmp_path: Path) -> None:
    repo, base = _repo_with_hooks(tmp_path)
    (repo / "hooks" / "unused-helper.txt").write_text("x")
    _commit_all(repo, "add helper")
    (repo / "hooks" / "unused-helper.txt").unlink()
    _commit_all(repo, "delete unreferenced helper")

    result = _invoke(repo, "--base", base)

    assert result.exit_code == 0


def test_rename_of_referenced_script_fails(tmp_path: Path) -> None:
    # The new path is wired and the old file is gone, but sessions started
    # before this change cached the OLD registration.
    repo, base = _repo_with_hooks(tmp_path)
    (repo / "hooks" / "demo-gate.sh").unlink()
    (repo / "hooks" / "hooks.json").write_text(_CONFIG_ENTRY_WITHOUT_GATE)
    (repo / "hooks" / "other-gate.sh").write_text("#!/usr/bin/env bash\nexit 0\n")
    _commit_all(repo, "move gate to other-gate.sh")

    result = _invoke(repo, "--base", base)

    assert result.exit_code == 1
    assert "hooks/demo-gate.sh" in result.stderr


def test_base_auto_resolves_to_head_parent_locally(tmp_path: Path) -> None:
    # No origin/* and no event sha: the local fallback is HEAD~1, which is
    # the base commit here, so the deletion still gates without --base.
    repo, _base = _repo_with_hooks(tmp_path)
    (repo / "hooks" / "demo-gate.sh").unlink()
    (repo / "hooks" / "hooks.json").write_text(_CONFIG_ENTRY_WITHOUT_GATE)
    _commit_all(repo, "delete script and entry together")

    result = _invoke(repo)

    assert result.exit_code == 1


def test_no_repo_degrades_to_refusal(tmp_path: Path) -> None:
    empty = tmp_path / "not-a-repo"
    empty.mkdir()

    result = _invoke(empty)

    assert result.exit_code == 2
