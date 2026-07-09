"""Report fold + graduation (US3): AC2-HP, AC6-HP."""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.evals import history as _history
from fno.evals.cli import evals_app
from fno.evals.report import (
    GraduateError,
    build_report,
    evals_health_summary,
    graduate_task_file,
    graduation_candidates,
    load_rows,
)

runner = CliRunner()


def _row(task_id: str, tier: str, passed: bool) -> dict:
    return {"task_id": task_id, "tier": tier, "pass": passed}


# AC2-HP: a task run 3 times with 2 passes -> pass@1 = 2/3, pass^3 False, flake.
def test_pass_k_report() -> None:
    rows = [_row("t", "capability", True), _row("t", "capability", False),
            _row("t", "capability", True)]
    report = build_report(rows)
    task = report["tasks"][0]
    assert task["runs"] == 3 and task["passes"] == 2
    assert task["pass_at_1"] == pytest.approx(2 / 3, abs=1e-4)
    assert task["pass_k"] is False
    assert task["flake"] is True
    assert "t" in report["flakes"]


def test_regression_alarm_fires_below_100() -> None:
    rows = [_row("r", "regression", True), _row("r", "regression", False)]
    report = build_report(rows)
    assert report["regression_alarm"] == ["r"]


def test_regression_alarm_silent_at_100() -> None:
    rows = [_row("r", "regression", True), _row("r", "regression", True)]
    assert build_report(rows)["regression_alarm"] == []


def test_graduated_task_excludes_pre_graduation_failures() -> None:
    """codex P2: after a capability task graduates, its old capability failures
    must NOT count in the regression pass rate / fire a false alarm."""
    rows = [
        _row("t", "capability", False),  # pre-graduation hill failure
        _row("t", "capability", True),
        _row("t", "capability", True),
        _row("t", "regression", True),   # first post-graduation run, green
    ]
    report = build_report(rows)
    assert report["regression_alarm"] == []  # no false alarm
    task = report["tasks"][0]
    assert task["tier"] == "regression"
    assert task["runs"] == 1 and task["passes"] == 1  # only the post-graduation run
    assert report["tiers"]["regression"]["pass_rate"] == 1.0


def test_regression_alarm_still_fires_on_real_post_graduation_failure() -> None:
    rows = [
        _row("t", "capability", True),
        _row("t", "regression", True),
        _row("t", "regression", False),  # real regression after graduation
    ]
    assert build_report(rows)["regression_alarm"] == ["t"]


def test_no_data() -> None:
    assert build_report([])["no_data"] is True


def test_tier_pass_rate() -> None:
    rows = [_row("a", "regression", True), _row("b", "regression", False)]
    report = build_report(rows)
    assert report["tiers"]["regression"]["pass_rate"] == 0.5


def test_since_folds_recent_only(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    for _ in range(5):
        _history.append_row(hp, _row("t", "regression", False))
    for _ in range(2):
        _history.append_row(hp, _row("t", "regression", True))
    rows = load_rows(hp, since=2)
    assert len(rows) == 2 and all(r["pass"] for r in rows)


def test_graduation_candidates_last_n_pass() -> None:
    rows = [_row("cap", "capability", False)] + [_row("cap", "capability", True)] * 3
    assert graduation_candidates(rows, n=3) == ["cap"]


def test_graduation_needs_n_runs() -> None:
    rows = [_row("cap", "capability", True), _row("cap", "capability", True)]
    assert graduation_candidates(rows, n=3) == []


def test_graduation_skips_regression_tier() -> None:
    rows = [_row("r", "regression", True)] * 3
    assert graduation_candidates(rows, n=3) == []


# AC6-HP: graduate rewrites the YAML tier, preserving comments.
def test_graduate_task_file_rewrites_tier(tmp_path: Path) -> None:
    p = tmp_path / "cap.yaml"
    p.write_text("# a comment\nid: cap\ntier: capability  # hill\ngrade:\n  - {kind: exit, command: pytest}\n",
                 encoding="utf-8")
    graduate_task_file(p)
    text = p.read_text(encoding="utf-8")
    assert "tier: regression  # hill" in text
    assert "# a comment" in text  # comments preserved


def test_graduate_non_capability_raises(tmp_path: Path) -> None:
    p = tmp_path / "reg.yaml"
    p.write_text("id: r\ntier: regression\ngrade:\n  - {kind: exit, command: pytest}\n", encoding="utf-8")
    with pytest.raises(GraduateError):
        graduate_task_file(p)


def test_evals_health_summary_none_without_history(tmp_path: Path) -> None:
    assert evals_health_summary(tmp_path / "absent.jsonl") is None


def test_evals_health_summary(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, _row("r", "regression", True))
    _history.append_row(hp, _row("r", "regression", False))
    summary = evals_health_summary(hp)
    assert summary is not None
    assert summary["flake_count"] == 1
    assert summary["regression_pass_rate"] == 0.5
    assert summary["regression_alarm"] == ["r"]


# --- CLI ---

def test_report_cli_regression_alarm_exit_4(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, _row("r", "regression", False))
    res = runner.invoke(evals_app, ["report", "--history", str(hp)])
    assert res.exit_code == 4
    assert "REGRESSION ALARM" in res.stdout


def test_report_cli_no_data_exit_0(tmp_path: Path) -> None:
    res = runner.invoke(evals_app, ["report", "--history", str(tmp_path / "none.jsonl")])
    assert res.exit_code == 0
    assert "no_data" in res.stdout


def test_graduate_cli(tmp_path: Path) -> None:
    d = tmp_path / "bank"
    d.mkdir()
    (d / "cap.yaml").write_text("id: cap\ntier: capability\ngrade:\n  - {kind: exit, command: pytest}\n",
                                encoding="utf-8")
    res = runner.invoke(evals_app, ["graduate", "cap", "--bank", str(d)])
    assert res.exit_code == 0
    assert "tier: regression" in (d / "cap.yaml").read_text()


def test_graduate_cli_unknown_id_exit_1(tmp_path: Path) -> None:
    d = tmp_path / "bank"
    d.mkdir()
    (d / "cap.yaml").write_text("id: cap\ntier: capability\ngrade:\n  - {kind: exit, command: pytest}\n",
                                encoding="utf-8")
    res = runner.invoke(evals_app, ["graduate", "nope", "--bank", str(d)])
    assert res.exit_code == 1
