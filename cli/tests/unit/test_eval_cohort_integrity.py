"""Cohort integrity: declared splits validated natively before any worker
call; tuning exports are train-only; held-out reporting is aggregate-only
(x-ecda AC2-HP, AC2-EDGE)."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional

import pytest
from typer.testing import CliRunner

import fno.evals.bank as bank_mod
from fno.evals.bank import (
    COHORTS_FILENAME,
    CohortDecl,
    CohortError,
    bank_unchanged_since,
    load_cohorts,
)
from fno.evals.cli import evals_app
from fno.rust_binary import find_dev_binary

runner = CliRunner()

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


def _task_yaml(tid: str) -> str:
    return f"id: {tid}\ntier: regression\ngrade:\n  - {{kind: exit, command: 'true'}}\n"


def _repo_with_bank(tmp_path: Path, tasks: list[str]) -> tuple[Path, Path, str]:
    """A real git repo with evals/bank/<id>.yaml committed.

    Returns (repo_root, bank_dir, bank_current_rev)."""
    root = tmp_path / "repo"
    root.mkdir()
    def g(*args: str) -> None:
        subprocess.run(["git", *args], cwd=str(root), check=True, capture_output=True)
    g("init", "-q")
    g("config", "user.email", "t@t.t")
    g("config", "user.name", "t")
    bank = root / "evals" / "bank"
    bank.mkdir(parents=True)
    for tid in tasks:
        (bank / f"{tid}.yaml").write_text(_task_yaml(tid), encoding="utf-8")
    g("add", "-A")
    g("commit", "-qm", "bank seed")
    rev = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=str(root), capture_output=True, text=True, check=True
    ).stdout.strip()
    return root, bank, rev


def _write_decl(bank: Path, *, train: list, validation: list, qualification: list,
                bank_rev: str = "HEAD") -> Path:
    path = bank / COHORTS_FILENAME
    lines = [f"bank_rev: {bank_rev}"]
    for role, ids in (("train", train), ("validation", validation),
                      ("qualification", qualification)):
        lines.append(f"{role}: [{', '.join(ids)}]")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def _history_append(path: Path, row: dict) -> None:
    from fno.evals import history as _history

    _history.append_row(path, row)


def _grader_row(task_id: str, *, passed: bool, bank_rev: Optional[str] = None) -> dict:
    """A modern attempt row: structured obs evidence of a valid grade."""
    row = {
        "task_id": task_id,
        "tier": "regression",
        "pass": passed,
        "obs": {"fixture_prepared": True, "worker_required": False,
                "grader_ran": True, "grader_passed": passed},
    }
    if bank_rev is not None:
        row["bank_rev"] = bank_rev
    return row


# --------------------------------------------------------------------------- #
# load_cohorts
# --------------------------------------------------------------------------- #

def test_load_cohorts_missing_file_returns_none(tmp_path: Path) -> None:
    assert load_cohorts(tmp_path / "bank") is None


def test_load_cohorts_valid_declaration_loads(tmp_path: Path) -> None:
    bank = tmp_path / "bank"
    bank.mkdir()
    _write_decl(bank, train=["t1", "t2"], validation=["v1"], qualification=["q1"],
                bank_rev="abc123")
    decl = load_cohorts(bank)
    assert decl is not None
    assert decl.bank_rev == "abc123"
    assert decl.train == ["t1", "t2"]
    assert decl.validation == ["v1"]
    assert decl.qualification == ["q1"]


def test_load_cohorts_malformed_yaml_raises(tmp_path: Path) -> None:
    bank = tmp_path / "bank"
    bank.mkdir()
    (bank / COHORTS_FILENAME).write_text("bank_rev: [unclosed\n", encoding="utf-8")
    with pytest.raises(CohortError):
        load_cohorts(bank)


def test_load_cohorts_bad_shape_raises(tmp_path: Path) -> None:
    bank = tmp_path / "bank"
    bank.mkdir()
    (bank / COHORTS_FILENAME).write_text("train: [t1]\n", encoding="utf-8")
    with pytest.raises(CohortError):
        load_cohorts(bank)
    (bank / COHORTS_FILENAME).write_text(
        "bank_rev: abc\ntrain: not-a-list\n", encoding="utf-8")
    with pytest.raises(CohortError):
        load_cohorts(bank)


# --------------------------------------------------------------------------- #
# bank_unchanged_since (the pinned-revision check)
# --------------------------------------------------------------------------- #

def test_bank_unchanged_true_when_no_bank_change_since_pin(tmp_path: Path) -> None:
    root, _bank, rev = _repo_with_bank(tmp_path, ["t1"])
    decl = CohortDecl(bank_rev=rev, train=["t1"])
    assert bank_unchanged_since(decl, root) is True


def test_bank_unchanged_false_after_bank_change(tmp_path: Path) -> None:
    root, bank, rev = _repo_with_bank(tmp_path, ["t1"])
    (bank / "t2.yaml").write_text(_task_yaml("t2"), encoding="utf-8")
    def g(*a: str) -> None:
        subprocess.run(["git", *a], cwd=str(root), check=True, capture_output=True)
    g("add", "-A")
    g("commit", "-qm", "grow the bank")
    decl = CohortDecl(bank_rev=rev, train=["t1"])
    assert bank_unchanged_since(decl, root) is False


def test_bank_unchanged_false_when_pin_unreadable(tmp_path: Path) -> None:
    root = tmp_path / "repo"
    root.mkdir()
    def g(*a: str) -> None:
        subprocess.run(["git", *a], cwd=str(root), check=True, capture_output=True)
    g("init", "-q")
    decl = CohortDecl(bank_rev="no-such-rev-000000")
    assert bank_unchanged_since(decl, root) is False


# --------------------------------------------------------------------------- #
# native door (integration through the compiled binary, skipped without it)
# --------------------------------------------------------------------------- #

@requires_rust
def test_native_door_validates_and_resolves_roles(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("FNO_AGENTS_BIN", str(find_dev_binary()))
    decl = CohortDecl(bank_rev="abc", train=["t1", "t2"], validation=["v1"],
                      qualification=["q1"])
    verdict = bank_mod.cohorts_verdict(decl, known_ids=["t1", "v1", "q1"],
                                       task_ids=["t1", "q1", "zz"])
    assert verdict["ok"] is False
    assert any("t2" in e for e in verdict["errors"])
    verdict2 = bank_mod.cohorts_verdict(
        CohortDecl(bank_rev="abc", train=["t1"], validation=["v1"], qualification=["q1"]),
        known_ids=["t1", "v1", "q1"], task_ids=["t1", "q1", "zz"])
    assert verdict2["ok"] is True
    assert verdict2["roles"] == {"t1": "train", "q1": "qualification", "zz": None}


@requires_rust
def test_native_door_fail_closed_when_binary_absent(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("FNO_AGENTS_BIN", "/nonexistent/fno-agents-probe")
    monkeypatch.setattr(bank_mod, "_door_binary", lambda: None)
    decl = CohortDecl(bank_rev="abc", train=["t1"], validation=[], qualification=[])
    verdict = bank_mod.cohorts_verdict(decl)
    assert verdict["ok"] is False
    assert "unreachable" in verdict["errors"][0]


# --------------------------------------------------------------------------- #
# run gating: validation fires BEFORE any worker call
# --------------------------------------------------------------------------- #

def _stub_run_pipeline(monkeypatch, calls: dict) -> None:
    """Stub the spawn-side of `evals run`: sweep_orphans + run_task."""
    import fno.evals.runner as runner_mod

    def fake_run_task(task, *, repeat, repo_root, **kw):
        calls["run"] = calls.get("run", 0) + 1
        from fno.evals.runner import RunResult

        return [RunResult(task.id, task.tier, True, "", 0.0, 0)]

    monkeypatch.setattr(runner_mod, "run_task", fake_run_task)
    monkeypatch.setattr(runner_mod, "sweep_orphans", lambda root: 0)


def _patch_repo_root(monkeypatch, root: Path) -> None:
    """Point resolve_canonical_repo_root at the tmp repo (the commands import
    it from fno.paths at call time)."""
    import fno.paths

    monkeypatch.setattr(fno.paths, "resolve_canonical_repo_root", lambda: root)


def _stub_verdict(monkeypatch, *, ok: bool, errors: Optional[list[str]] = None,
                  roles: Optional[dict] = None) -> None:
    """Canned native door verdict for the run/export/qualify gates."""
    monkeypatch.setattr(
        bank_mod, "cohorts_verdict",
        lambda decl, **kw: {"ok": ok, "errors": errors or [], "roles": roles or {}},
    )


def test_run_refuses_overlap_before_any_spawn(tmp_path, monkeypatch) -> None:
    calls: dict = {}
    _stub_run_pipeline(monkeypatch, calls)
    root, bank, rev = _repo_with_bank(tmp_path, ["t1", "t2"])
    _write_decl(bank, train=["t1"], validation=["t1"], qualification=[])
    _patch_repo_root(monkeypatch, root)
    _stub_verdict(monkeypatch, ok=False,
                  errors=["task id 't1' is in both 'train' and 'validation'"])
    res = runner.invoke(evals_app, ["run", "--bank", str(bank), "--yes"])
    assert res.exit_code == 2
    assert "cohort split refused" in res.stdout + (res.stderr or "")
    assert calls.get("run", 0) == 0  # no worker call


def test_run_refuses_unknown_declared_id(tmp_path, monkeypatch) -> None:
    calls: dict = {}
    _stub_run_pipeline(monkeypatch, calls)
    root, bank, _rev = _repo_with_bank(tmp_path, ["t1"])
    _write_decl(bank, train=["t1", "ghost"], validation=[], qualification=[])
    _patch_repo_root(monkeypatch, root)
    _stub_verdict(monkeypatch, ok=False, errors=["unknown task id 'ghost' in 'train'"])
    res = runner.invoke(evals_app, ["run", "--bank", str(bank), "--yes"])
    assert res.exit_code == 2
    assert calls.get("run", 0) == 0


def test_run_refuses_stale_bank_pin(tmp_path, monkeypatch) -> None:
    calls: dict = {}
    _stub_run_pipeline(monkeypatch, calls)
    root, bank, rev = _repo_with_bank(tmp_path, ["t1"])
    (bank / "t2.yaml").write_text(_task_yaml("t2"), encoding="utf-8")
    def g(*a: str) -> None:
        subprocess.run(["git", *a], cwd=str(root), check=True, capture_output=True)
    g("add", "-A")
    g("commit", "-qm", "grow the bank")
    _write_decl(bank, train=["t1"], validation=[], qualification=[],
                bank_rev=rev)  # pinned BEFORE the growth commit
    _patch_repo_root(monkeypatch, root)
    _stub_verdict(monkeypatch, ok=True)
    res = runner.invoke(evals_app, ["run", "--bank", str(bank), "--yes"])
    assert res.exit_code == 2
    assert "redeclare" in res.stdout + (res.stderr or "")
    assert calls.get("run", 0) == 0


def test_run_with_valid_split_proceeds_and_announces(tmp_path, monkeypatch) -> None:
    calls: dict = {}
    _stub_run_pipeline(monkeypatch, calls)
    root, bank, rev = _repo_with_bank(tmp_path, ["t1", "t2"])
    _write_decl(bank, train=["t1"], validation=["t2"], qualification=[])
    _patch_repo_root(monkeypatch, root)
    _stub_verdict(monkeypatch, ok=True)
    res = runner.invoke(evals_app, ["run", "--bank", str(bank), "--yes"])
    assert res.exit_code == 0
    assert "validated cohort split" in res.stdout
    assert calls.get("run", 0) == 2  # both tasks ran


# --------------------------------------------------------------------------- #
# tuning export: train-only, refused without a current declared split
# --------------------------------------------------------------------------- #

@requires_rust
def test_export_is_train_only(tmp_path, monkeypatch) -> None:
    root, bank, rev = _repo_with_bank(tmp_path, ["t1", "t2", "q1"])
    _write_decl(bank, train=["t1", "t2"], validation=[], qualification=["q1"],
                bank_rev=rev)
    _patch_repo_root(monkeypatch, root)
    hp = tmp_path / "h.jsonl"
    for tid in ("t1", "q1", "t2"):
        _history_append(hp, _grader_row(tid, passed=True, bank_rev=rev))
    out = tmp_path / "tuning.jsonl"
    res = runner.invoke(evals_app, ["export", "--bank", str(bank), "--history", str(hp),
                                    "--out", str(out)])
    assert res.exit_code == 0, res.output
    lines = [json.loads(x) for x in out.read_text(encoding="utf-8").splitlines()]
    assert sorted(r["task_id"] for r in lines) == ["t1", "t2"]  # q1 never leaves
    assert all(r["role"] == "train" for r in lines)


def test_export_requires_a_declared_split(tmp_path, monkeypatch) -> None:
    root, bank, _rev = _repo_with_bank(tmp_path, ["t1"])
    _patch_repo_root(monkeypatch, root)
    out = tmp_path / "tuning.jsonl"
    res = runner.invoke(evals_app, ["export", "--bank", str(bank),
                                    "--out", str(out)])
    assert res.exit_code == 1
    assert "no declared cohort split" in res.stdout + (res.stderr or "")
    assert not out.exists()


@requires_rust
def test_export_refuses_stale_bank_pin(tmp_path, monkeypatch) -> None:
    root, bank, rev = _repo_with_bank(tmp_path, ["t1"])
    (bank / "t2.yaml").write_text(_task_yaml("t2"), encoding="utf-8")
    def g(*a: str) -> None:
        subprocess.run(["git", *a], cwd=str(root), check=True, capture_output=True)
    g("add", "-A")
    g("commit", "-qm", "grow the bank")
    _write_decl(bank, train=["t1"], validation=[], qualification=[], bank_rev=rev)
    _patch_repo_root(monkeypatch, root)
    out = tmp_path / "tuning.jsonl"
    res = runner.invoke(evals_app, ["export", "--bank", str(bank),
                                    "--out", str(out)])
    assert res.exit_code == 2
    assert "redeclare" in res.stdout + (res.stderr or "")
    assert not out.exists()


# --------------------------------------------------------------------------- #
# qualify: the allowed aggregate projection
# --------------------------------------------------------------------------- #

def test_qualify_reports_unqualified_without_declaration(tmp_path, monkeypatch) -> None:
    root, bank, _rev = _repo_with_bank(tmp_path, ["q1"])
    _patch_repo_root(monkeypatch, root)
    res = runner.invoke(evals_app, ["qualify", "--bank", str(bank)])
    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    assert payload["qualified"] is False
    assert "no declared cohort split" in payload["reason"]


def test_qualify_reports_unqualified_with_empty_qualification(tmp_path, monkeypatch) -> None:
    root, bank, rev = _repo_with_bank(tmp_path, ["t1"])
    _write_decl(bank, train=["t1"], validation=[], qualification=[], bank_rev=rev)
    _patch_repo_root(monkeypatch, root)
    res = runner.invoke(evals_app, ["qualify", "--bank", str(bank)])
    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    assert payload == {"qualified": False, "reason": "no declared qualification cohort"}


def test_qualify_reports_unqualified_when_door_refuses(tmp_path, monkeypatch) -> None:
    root, bank, rev = _repo_with_bank(tmp_path, ["q1"])
    _write_decl(bank, train=[], validation=[], qualification=["q1"], bank_rev=rev)
    _patch_repo_root(monkeypatch, root)
    _stub_verdict(monkeypatch, ok=False, errors=["unknown task id 'ghost' in 'qualification'"])
    res = runner.invoke(evals_app, ["qualify", "--bank", str(bank)])
    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    assert payload["qualified"] is False
    assert "ghost" in payload["reason"]


def _fake_batch_binary(tmp_path: Path, verdicts: list[dict]) -> str:
    """A stub binary that answers any evals-attempt --rows call with canned verdicts."""
    import shlex

    script = tmp_path / "fake-agents.sh"
    script.write_text(
        "#!/bin/sh\nprintf '%s' " + shlex.quote(json.dumps(verdicts)) + "\n",
        encoding="utf-8",
    )
    script.chmod(0o755)
    return str(script)


def test_qualify_aggregates_only_valid_grades(tmp_path, monkeypatch) -> None:
    root, bank, rev = _repo_with_bank(tmp_path, ["q1"])
    _write_decl(bank, train=[], validation=[], qualification=["q1"], bank_rev=rev)
    _patch_repo_root(monkeypatch, root)
    _stub_verdict(monkeypatch, ok=True, roles={"q1": "qualification"})
    hp = tmp_path / "h.jsonl"
    _history_append(hp, _grader_row("q1", passed=True, bank_rev=rev))
    _history_append(hp, {"task_id": "q1", "pass": False,
                         "obs": {"fixture_prepared": False}})
    _history_append(hp, {"task_id": "q1", "pass": False})  # legacy: no obs
    _history_append(hp, _grader_row("q1", passed=True, bank_rev="different-rev"))
    fake = _fake_batch_binary(tmp_path, [
        {"line": 1, "status": "graded", "graded": True, "retryable": False, "rev_match": True},
        {"line": 2, "status": "infrastructure", "graded": None, "retryable": True, "rev_match": True},
        {"line": 3, "status": "legacy", "graded": None, "retryable": False, "rev_match": True},
        {"line": 4, "status": "graded", "graded": True, "retryable": False, "rev_match": False},
    ])
    monkeypatch.setattr("fno.rust_binary.find_dev_binary", lambda: fake)
    res = runner.invoke(evals_app, ["qualify", "--bank", str(bank), "--history", str(hp)])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert payload["qualified"] is True
    assert payload["valid_grades"] == 1
    assert payload["passes"] == 1
    assert payload["attempts"] == {"graded": 1, "infrastructure": 1}
    assert payload["excluded"] == {"wrong_rev": 1, "legacy": 1}
    assert payload["declared_tasks"] == 1
    assert payload["missing_tasks"] == 0


def test_qualify_missing_tasks_are_reported_not_dropped(tmp_path, monkeypatch) -> None:
    root, bank, rev = _repo_with_bank(tmp_path, ["q1", "q2"])
    _write_decl(bank, train=[], validation=[], qualification=["q1", "q2"], bank_rev=rev)
    _patch_repo_root(monkeypatch, root)
    _stub_verdict(monkeypatch, ok=True, roles={"q1": "qualification", "q2": "qualification"})
    hp = tmp_path / "h.jsonl"
    _history_append(hp, _grader_row("q1", passed=False, bank_rev=rev))  # q2 never ran
    fake = _fake_batch_binary(tmp_path, [
        {"line": 1, "status": "graded", "graded": False, "retryable": False, "rev_match": True},
    ])
    monkeypatch.setattr("fno.rust_binary.find_dev_binary", lambda: fake)
    res = runner.invoke(evals_app, ["qualify", "--bank", str(bank), "--history", str(hp)])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert payload["declared_tasks"] == 2
    assert payload["missing_tasks"] == 1
    assert payload["valid_grades"] == 1
    assert payload["passes"] == 0  # the one valid grade is a failure
