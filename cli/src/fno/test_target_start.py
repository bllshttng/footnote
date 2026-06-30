"""Tests for `fno target start` - the one-verb cold-start (x-d91b).

Covers the pure name sanitizer plus the four command branches with the
subprocess + setup-hook stubbed so no real worktree/state is created:
  * already-isolated -> no-op, nothing spawned (Boundary).
  * happy path -> ensure + setup-hook + init, receipt `node=claimed`.
  * existing manifest -> idempotent skip, init NOT re-run (Invariant).
  * ensure failure -> loud non-zero, init never reached (Errors).
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from typer.testing import CliRunner

from fno import target_cli
from fno.target_cli import _wt_name, target_app

runner = CliRunner()


# ----------------------------- pure sanitizer ----------------------------- #
def test_wt_name_node_id_roundtrips():
    assert _wt_name("x-d91b") == "x-d91b"


def test_wt_name_slugifies_free_text():
    assert _wt_name("Fix the Login Bug!") == "fix-the-login-bug"


def test_wt_name_never_empty():
    assert _wt_name("///") == "target"


def test_wt_name_bounded():
    assert len(_wt_name("a" * 200)) == 60


# ------------------------------- no-op branch ----------------------------- #
def test_already_isolated_is_noop(monkeypatch):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: True)
    spawned = []
    monkeypatch.setattr(
        target_cli.subprocess, "run", lambda *a, **k: spawned.append(a) or None
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "already isolated" in result.stdout
    assert spawned == []  # nothing created


# --------------------------- happy path + idempotency --------------------- #
def _wire_happy(monkeypatch, wt_path: Path, *, manifest_exists: bool):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(
        target_cli, "_git_out", lambda cwd, *a: "/canonical/repo"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda r, w: (0, "")
    )
    if manifest_exists:
        (wt_path / ".fno").mkdir(parents=True, exist_ok=True)
        (wt_path / ".fno" / "target-state.md").write_text("session_id: x\n")

    init_calls = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt_path), stderr="")
        if "init" in args:
            init_calls.append(kwargs.get("cwd"))
            return subprocess.CompletedProcess(args, 0)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    return init_calls


def test_happy_path_claims_and_prints_receipt(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=False)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert f"worktree={wt}" in result.stdout
    assert "base=origin/main" in result.stdout
    assert "node=claimed" in result.stdout
    # init ran exactly once, from inside the worktree (binds owner_cwd).
    assert init_calls == [str(wt)]


def test_existing_manifest_is_idempotent(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=True)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "node=already-claimed" in result.stdout
    assert init_calls == []  # invariant: never double-claim


def test_ensure_failure_is_loud_and_skips_init(monkeypatch, tmp_path):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    init_calls = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 1, stdout="", stderr="boom")
        if "init" in args:
            init_calls.append(True)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert init_calls == []  # never proceed past a failed ensure
