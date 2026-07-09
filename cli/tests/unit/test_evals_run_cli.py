"""`fno evals run` CLI exit-code contract (no spawn / no worktree needed)."""
from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from fno.evals.cli import evals_app

runner = CliRunner()


def _bank(tmp_path: Path, name: str, body: str) -> Path:
    d = tmp_path / "bank"
    d.mkdir(exist_ok=True)
    (d / name).write_text(body, encoding="utf-8")
    return d


def test_no_bank_exits_1(tmp_path: Path) -> None:
    res = runner.invoke(evals_app, ["run", "--bank", str(tmp_path / "nope")])
    assert res.exit_code == 1
    assert "no bank" in res.stdout.lower() or "no bank" in (res.stderr or "").lower()


def test_invalid_bank_task_exits_2(tmp_path: Path) -> None:
    d = _bank(tmp_path, "bad.yaml", "id: b\ntier: regression\ngrade: []\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d)])
    assert res.exit_code == 2


def test_empty_selection_exits_1(tmp_path: Path) -> None:
    d = _bank(tmp_path, "a.yaml",
              "id: a\ntier: regression\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d), "--task", "does-not-exist"])
    assert res.exit_code == 1


def test_bad_repeat_exits_1(tmp_path: Path) -> None:
    d = _bank(tmp_path, "a.yaml",
              "id: a\ntier: regression\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d), "--repeat", "0"])
    assert res.exit_code == 1


def test_prompt_task_defaults_provider(tmp_path: Path, monkeypatch) -> None:
    """codex P2: a prompt-bearing task with no --provider defaults to claude so
    the headless spawn is never provider-less. Assert run_task gets a provider."""
    import fno.evals.runner as runner_mod

    seen: dict = {}

    def fake_run_task(task, *, repeat, repo_root, worker_provider=None):
        seen["provider"] = worker_provider
        return [runner_mod.RunResult(task.id, task.tier, True, "", 0.0, 0)]

    # run_command does `from fno.evals.runner import run_task, sweep_orphans` at
    # call time, so patching the source module is what the import resolves.
    monkeypatch.setattr(runner_mod, "run_task", fake_run_task)
    monkeypatch.setattr(runner_mod, "sweep_orphans", lambda root: 0)

    d = _bank(tmp_path, "cap.yaml",
              "id: cap\ntier: capability\nprompt: do it\ngrade:\n  - {kind: exit, command: 'true'}\n")
    res = runner.invoke(evals_app, ["run", "--bank", str(d)])
    assert res.exit_code == 0
    assert seen.get("provider") == "claude"
    assert "defaulting" in res.stdout
