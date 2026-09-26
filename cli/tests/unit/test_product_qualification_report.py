"""Tests for the qualification report path (AC3)."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.evals.cli import evals_app

runner = CliRunner()

REPO_ROOT = Path(__file__).resolve().parents[3]
MANIFEST = REPO_ROOT / "evals" / "fixtures" / "product-delivery" / "qualification.json"

BANK_TASK = "product-delivery-journey"


def _forwarder_fakes(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, *, returncode: int = 0) -> dict:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: tmp_path / "fno-agents")
    captured: dict = {}

    def fake_run(argv, check=False, **kw):
        captured["argv"] = argv
        return subprocess.CompletedProcess(argv, returncode)

    monkeypatch.setattr(subprocess, "run", fake_run)
    return captured


def test_report_forwarder_passes_qualification_through(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    captured = _forwarder_fakes(monkeypatch, tmp_path)
    res = runner.invoke(evals_app, ["report", "--qualification", str(MANIFEST)])
    assert res.exit_code == 0
    argv = captured["argv"]
    assert argv[argv.index("--qualification") + 1] == str(MANIFEST)


def _history_row(cohort: str, repeat: int, bank_rev: str) -> str:
    """One graded-pass attempt row in the runner's exact shape."""
    payload = {
        "ts": "2026-09-26T00:00:00Z",
        "task_id": BANK_TASK,
        "tier": "capability",
        "pass": True,
        "reason": "",
        "duration_s": 12.5,
        "repeat_index": repeat,
        "attempt_index": 0,
        "attempt_id": f"att-{cohort}-{repeat}",
        "run_id": "run-qual-1",
        "obs": {
            "fixture_prepared": True,
            "worker_required": False,
            "grader_ran": True,
            "grader_passed": True,
            "gate_blocked": False,
        },
        "bank_rev": bank_rev,
        "worker_provider": None,
        "variant": "baseline",
        "experiment_id": cohort,
    }
    return json.dumps(payload)


def test_live_native_fold_answers_the_declared_manifest(tmp_path: Path) -> None:
    """The real dev binary folds the real manifest end to end."""
    from fno.rust_binary import find_dev_binary

    binary = find_dev_binary()
    if binary is None:
        pytest.skip("no dev fno-agents binary; the fold is covered in Rust")
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    rev = manifest["release"]["bank_rev"]
    history = tmp_path / "history.jsonl"
    history.write_text(
        "\n".join(
            [
                _history_row("install-first-use", 0, rev),
                _history_row("install-first-use", 1, rev),
                _history_row("delivery-evidence-failure", 0, rev),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    proc = subprocess.run(
        [
            str(binary), "evals-trend",
            "--mode", "report", "--qualification", str(MANIFEST),
            "--history", str(history), "--json",
        ],
        capture_output=True, text=True, timeout=60, check=False,
    )
    assert proc.returncode == 4, proc.stdout  # one conformance case missing
    projection = json.loads(proc.stdout)
    scenarios = projection["qualification"]["scenarios"]
    assert scenarios["install-first-use"]["completed"] == 2
    assert scenarios["delivery-evidence-failure"]["completed"] == 1
    assert scenarios["delivery-evidence-failure"]["missing"] == 1
    assert scenarios["operator-effort-per-outcome"]["missing"] == 2
    totals = projection["qualification"]["totals"]
    assert totals == {"expected": 10, "completed": 3, "failed": 0, "missing": 7,
                      "unsupported": 0}