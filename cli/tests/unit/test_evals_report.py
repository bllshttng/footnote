"""Report fold, graduation, and the native forwarders (d-b6cc1a2a).

The windowed alarm, the trend windows, variant compare, and graduation
candidates are native in `crates/fno-agents/src/evals_trend/` and tested
there. Python keeps the all-rows fold, load_rows, the graduation file
rewrite, and the summary caller; these tests pin the Python surface and the
forwarder argv contract.
"""
from __future__ import annotations

import subprocess
from datetime import datetime, timedelta, timezone
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
    load_rows,
)

runner = CliRunner()


def _row(task_id: str, tier: str, passed: bool) -> dict:
    return {"task_id": task_id, "tier": tier, "pass": passed}


# --- the all-rows fold that stayed in Python -------------------------------

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
    assert build_report(rows)["regression_alarm"] == ["r"]


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
    assert report["regression_alarm"] == []
    task = report["tasks"][0]
    assert task["tier"] == "regression"
    assert task["runs"] == 1 and task["passes"] == 1
    assert report["tiers"]["regression"]["pass_rate"] == 1.0


def test_regression_alarm_still_fires_on_real_post_graduation_failure() -> None:
    rows = [
        _row("t", "capability", True),
        _row("t", "regression", True),
        _row("t", "regression", False),
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


# --- graduation (unchanged semantics) ---------------------------------------

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


# --- the summary caller (x-ab72 age fields; native alarm/regressed) ---------

_NOW = datetime(2026, 9, 15, 12, 0, 0, tzinfo=timezone.utc)


def _ts(dt: datetime) -> str:
    return dt.isoformat().replace("+00:00", "Z")


def _days_ago(n: float) -> str:
    return _ts(_NOW - timedelta(days=n))


def _quiet_native(monkeypatch: pytest.MonkeyPatch) -> None:
    """Pin the native summary read so the test never spawns a binary."""
    monkeypatch.setattr(
        "fno.evals.report._native_summary_reads", lambda _p, _d: ([], [])
    )


def test_evals_health_summary(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.evals.report._native_summary_reads", lambda _p, _d: (["r"], [])
    )
    hp = tmp_path / "h.jsonl"
    recent = _ts(_NOW - timedelta(hours=1))
    _history.append_row(hp, {**_row("r", "regression", True), "ts": recent})
    _history.append_row(hp, {**_row("r", "regression", False), "ts": recent})
    summary = evals_health_summary(hp, now=_NOW)
    assert summary is not None
    assert summary["flake_count"] == 1
    assert summary["regression_pass_rate"] == 0.5
    assert summary["regression_alarm"] == ["r"]
    assert summary["regressed"] == []


def test_health_summary_stale_when_newest_regression_old(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _quiet_native(monkeypatch)
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": _days_ago(9)})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["stale"] is True
    assert summary["age_days"] == pytest.approx(9.0, abs=0.01)
    assert summary["never_ran"] is False


def test_health_summary_fresh_inside_window(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _quiet_native(monkeypatch)
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": _days_ago(2)})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["stale"] is False
    assert summary["age_days"] == pytest.approx(2.0, abs=0.01)


def test_health_summary_never_ran_when_no_regression_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _quiet_native(monkeypatch)
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("cap", "capability", True), "ts": _days_ago(1)})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["never_ran"] is True
    assert summary["stale"] is False
    assert summary["age_days"] is None
    assert summary["regression_pass_rate"] is None


def test_health_summary_unreadable_ts_never_asserts_stale(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _quiet_native(monkeypatch)
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": "not-a-timestamp"})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["age_days"] is None
    assert summary["stale"] is False
    assert summary["never_ran"] is False


def test_health_summary_rows_without_ts_never_assert_stale(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _quiet_native(monkeypatch)
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, _row("r", "regression", True))
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["stale"] is False


def test_health_summary_stale_days_resolves_from_config(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    class _Evals:
        stale_days = 2

    class _Settings:
        evals = _Evals()

    monkeypatch.setattr("fno.evals.report.load_settings", lambda: _Settings())
    _quiet_native(monkeypatch)
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": _days_ago(3)})
    summary = evals_health_summary(hp, now=_NOW)
    assert summary is not None
    assert summary["stale"] is True


def test_health_summary_reads_the_native_pair(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        "fno.evals.report._native_summary_reads", lambda _p, _d: (["r"], ["r"])
    )
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": _days_ago(1)})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["regression_alarm"] == ["r"]
    assert summary["regressed"] == ["r"]
    assert summary["window_days"] == 7


# --- variant axis: a missing variant key reads as baseline ------------------

def test_load_rows_missing_variant_reads_as_baseline(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, _row("t", "regression", True))
    assert len(load_rows(hp)) == 1  # the 28 legacy rows keep folding


def test_load_rows_filters_variant(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("t", "regression", True), "variant": "baseline"})
    _history.append_row(hp, {**_row("t", "regression", True), "variant": "v1"})
    assert len(load_rows(hp)) == 1               # default fold: baseline only
    assert len(load_rows(hp, variant="v1")) == 1
    assert len(load_rows(hp, variant=None)) == 2  # None folds every round


def test_since_applies_after_variant_filter(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("t", "regression", False), "variant": "baseline"})
    _history.append_row(hp, {**_row("t", "regression", True), "variant": "v1"})
    _history.append_row(hp, {**_row("t", "regression", True), "variant": "v1"})
    rows = load_rows(hp, since=1, variant="v1")
    assert len(rows) == 1 and rows[0]["variant"] == "v1"


# --- CLI forwarders: argv contract + exit propagation -----------------------

def _forwarder_fakes(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, *, returncode: int = 0) -> dict:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: tmp_path / "fno-agents")
    captured: dict = {}

    def fake_run(argv, check=False, **kw):
        captured["argv"] = argv
        return subprocess.CompletedProcess(argv, returncode)

    monkeypatch.setattr(subprocess, "run", fake_run)
    return captured


def test_forwarder_refuses_without_the_binary(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno import rust_binary

    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: None)
    for leaf in ("report", "trend"):
        res = runner.invoke(evals_app, [leaf])
        assert res.exit_code == 2
        assert "binary was not found" in res.output


def test_forwarder_argv_composition(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    class _Evals:
        stale_days = 9

    class _Settings:
        evals = _Evals()

    monkeypatch.setattr("fno.config.load_settings", lambda: _Settings())
    hp = tmp_path / "h.jsonl"
    monkeypatch.setattr("fno.paths.evals_history", lambda: hp)
    captured = _forwarder_fakes(monkeypatch, tmp_path)
    for leaf, mode in (("report", "report"), ("trend", "trend")):
        captured.clear()
        res = runner.invoke(evals_app, [leaf, "--json", "--since", "5"])
        assert res.exit_code == 0
        argv = captured["argv"]
        assert argv[0] == str(tmp_path / "fno-agents")
        assert argv[1] == "evals-trend"
        assert argv[argv.index("--mode") + 1] == mode
        assert argv[argv.index("--history") + 1] == str(hp)
        assert argv[argv.index("--stale-days") + 1] == "9"
        assert "--json" in argv
        assert argv[argv.index("--since") + 1] == "5"


def test_forwarder_exit_code_propagation(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fno.paths.evals_history", lambda: tmp_path / "h.jsonl")
    captured = _forwarder_fakes(monkeypatch, tmp_path, returncode=4)
    res = runner.invoke(evals_app, ["report"])
    assert res.exit_code == 4
    assert captured["argv"][1] == "evals-trend"


# --- graduate CLI (unchanged) -----------------------------------------------

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


# --- regression guards from the port (d-b6cc1a2a) ---------------------------

def test_fold_symbols_are_gone_from_python() -> None:
    """The windowed/compare/graduation-candidate folds live in Rust now; the
    Python module must not carry a second implementation (law d-b6cc1a2a)."""
    import fno.evals.report as report_module

    for gone in ("compare_windows", "window_rows", "graduation_candidates",
                 "compare_variants", "_pair_verdict", "_common_rev"):
        assert not hasattr(report_module, gone)
