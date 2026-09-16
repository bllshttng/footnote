"""Report graduation and the native summary reader (d-b6cc1a2a).

The fold lives native in `crates/fno-agents/src/evals_trend/` and is tested
there (windows, staleness, variant compare, graduation, corrupt-line
tolerance). Python keeps the graduation rewrite and the summary reader;
these tests pin the reader's mapping and the forwarder argv contract.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.evals.cli import evals_app
from fno.evals.report import (
    GraduateError,
    evals_health_summary,
    graduate_task_file,
)

runner = CliRunner()


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


# --- the summary reader: a thin mapping of the native payload ---------------

def _payload(**overrides) -> dict:
    """One native summary payload, defaults valid and fresh."""
    base = {
        "regression_alarm": ["r"],
        "regressed": [],
        "regression_pass_rate": 0.5,
        "flake_count": 1,
        "row_count": 2,
        "never_ran": False,
        "age_days": 1.0,
        "stale": False,
    }
    base.update(overrides)
    return base


def _stub_summary(monkeypatch: pytest.MonkeyPatch, payload) -> None:
    """Pin the native summary read so the test never spawns a binary."""
    monkeypatch.setattr("fno.evals.report._native_summary", lambda _p, _d: payload)


def test_evals_health_summary_maps_the_native_payload(tmp_path, monkeypatch) -> None:
    _stub_summary(monkeypatch, _payload())
    hp = tmp_path / "h.jsonl"
    hp.touch()
    summary = evals_health_summary(hp, stale_days=7)
    assert summary is not None
    assert summary["regression_alarm"] == ["r"]
    assert summary["regressed"] == []
    assert summary["regression_pass_rate"] == 0.5
    assert summary["flake_count"] == 1
    assert summary["window_days"] == 7
    assert summary["age_days"] == 1.0
    assert summary["stale"] is False
    assert summary["never_ran"] is False


def test_health_summary_none_when_row_count_zero(tmp_path, monkeypatch) -> None:
    _stub_summary(monkeypatch, _payload(row_count=0))
    hp = tmp_path / "h.jsonl"
    hp.touch()
    assert evals_health_summary(hp) is None


def test_health_summary_none_when_door_unreachable(tmp_path, monkeypatch) -> None:
    """An unreachable native fold degrades to None, never a Python re-fold."""
    _stub_summary(monkeypatch, None)
    hp = tmp_path / "h.jsonl"
    hp.touch()
    assert evals_health_summary(hp) is None


def test_health_summary_stale_days_resolves_from_config(tmp_path, monkeypatch) -> None:
    seen: dict = {}

    def fake_summary(_p: Path, stale_days: int):
        seen["stale_days"] = stale_days
        return _payload()

    class _Evals:
        stale_days = 2

    class _Settings:
        evals = _Evals()

    monkeypatch.setattr("fno.evals.report.load_settings", lambda: _Settings())
    monkeypatch.setattr("fno.evals.report._native_summary", fake_summary)
    hp = tmp_path / "h.jsonl"
    hp.touch()
    summary = evals_health_summary(hp)
    assert summary is not None
    assert summary["window_days"] == 2
    assert seen["stale_days"] == 2


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
    """The fold (windows, staleness, compare, row reading) lives in Rust now;
    the Python module must not carry a second implementation (law d-b6cc1a2a)."""
    import fno.evals.report as report_module
    import fno.evals.history as history_module

    for gone in ("compare_windows", "window_rows", "graduation_candidates",
                 "compare_variants", "_pair_verdict", "_common_rev",
                 "build_report", "_stats", "_by_task", "TaskStat",
                 "load_rows", "_parse_ts", "_native_summary_reads"):
        assert not hasattr(report_module, gone)
    assert not hasattr(history_module, "iter_rows_tolerant")
