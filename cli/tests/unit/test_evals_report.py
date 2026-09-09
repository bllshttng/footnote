"""Report fold + graduation (US3): AC2-HP, AC6-HP."""
from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.evals import history as _history
from fno.evals.cli import evals_app
from fno.evals.report import (
    CohortSpec,
    GraduateError,
    build_report,
    compare_cohorts,
    compare_variants,
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


# --- age fields (x-ab72): the demand side ---

_NOW = datetime(2026, 9, 15, 12, 0, 0, tzinfo=timezone.utc)


def _ts(dt: datetime) -> str:
    return dt.isoformat().replace("+00:00", "Z")


def test_health_summary_stale_when_newest_regression_old(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": _ts(_NOW - timedelta(days=9))})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["stale"] is True
    assert summary["age_days"] == pytest.approx(9.0, abs=0.01)
    assert summary["never_ran"] is False


def test_health_summary_fresh_inside_window(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": _ts(_NOW - timedelta(days=2))})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["stale"] is False
    assert summary["age_days"] == pytest.approx(2.0, abs=0.01)


def test_health_summary_never_ran_when_no_regression_row(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("cap", "capability", True), "ts": _ts(_NOW - timedelta(days=1))})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["never_ran"] is True
    assert summary["stale"] is False
    assert summary["age_days"] is None
    assert summary["regression_pass_rate"] is None


def test_health_summary_unreadable_ts_never_asserts_stale(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": "not-a-timestamp"})
    summary = evals_health_summary(hp, stale_days=7, now=_NOW)
    assert summary is not None
    assert summary["age_days"] is None
    assert summary["stale"] is False
    assert summary["never_ran"] is False


def test_health_summary_rows_without_ts_never_assert_stale(tmp_path: Path) -> None:
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
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("r", "regression", True), "ts": _ts(_NOW - timedelta(days=3))})
    summary = evals_health_summary(hp, now=_NOW)
    assert summary is not None
    assert summary["stale"] is True


# --- variant axis: a missing variant key reads as baseline ---

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


def test_compare_variants_improved() -> None:
    rows = [
        {**_row("t", "regression", True), "variant": "baseline"},
        {**_row("t", "regression", False), "variant": "baseline"},
        {**_row("t", "regression", True), "variant": "v1"},
        {**_row("t", "regression", True), "variant": "v1"},
    ]
    cmp = compare_variants(rows, "v1")
    t = cmp["tasks"]["t"]
    assert t["delta"] == 0.5 and t["verdict"] == "improved"
    assert t["baseline"]["runs"] == 2 and t["variant"]["runs"] == 2


def test_variant_fails_do_not_fire_baseline_alarm(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {**_row("t", "regression", True), "variant": "baseline"})
    _history.append_row(hp, {**_row("t", "regression", False), "variant": "v1"})
    report = build_report(load_rows(hp))
    assert report["regression_alarm"] == []
    assert report["tasks"][0]["runs"] == 1


def test_compare_missing_sides() -> None:
    rows = [
        {**_row("u", "regression", True), "variant": "baseline"},
        {**_row("w", "regression", True), "variant": "v1"},
    ]
    cmp = compare_variants(rows, "v1")
    assert cmp["missing_in_variant"] == ["u"]
    assert cmp["missing_in_baseline"] == ["w"]
    assert "u" not in cmp["tasks"] and "w" not in cmp["tasks"]


def test_compare_scores_one_revision_pair() -> None:
    rows = [
        {**_row("t", "regression", True), "variant": "baseline", "bank_rev": "new"},
        {**_row("t", "regression", True), "variant": "baseline", "bank_rev": "new"},
        {**_row("t", "regression", False), "variant": "baseline", "bank_rev": "old"},
        {**_row("t", "regression", True), "variant": "v1", "bank_rev": "v1rev"},
    ]
    cmp = compare_variants(rows, "v1")
    t = cmp["tasks"]["t"]
    # only the modal-rev baseline rows score: the "old" failure is excluded
    assert t["baseline"]["runs"] == 2
    assert t["baseline"]["pass_at_1"] == 1.0
    assert cmp["baseline_rev"] == "new" and cmp["variant_rev"] == "v1rev"


def test_common_rev_tie_breaks_deterministically() -> None:
    from fno.evals.report import _common_rev

    assert _common_rev([{"bank_rev": "bbb"}, {"bank_rev": "aaa"}]) == "aaa"


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


# --------------------------------------------------------------------------- #
# cohort comparison (x-fd52 wave 2): AC2-HP, AC2-EDGE
# --------------------------------------------------------------------------- #

def _lane_row(cohort: str, lane: str, passed: bool, **kw) -> dict:
    row = {"task_id": "t", "tier": "regression", "pass": passed,
           "experiment_id": cohort, "requested_lane": lane,
           "lane_status": "ok", "bank_rev": "rev1", "duration_s": 1.0}
    row.update(kw)
    return row


def test_cohort_hp_shows_samples_reliability_duration_usage_and_criteria() -> None:
    rows = [
        _lane_row("cohort-a", "claude-sonnet", True, usage={"source": "api", "unit": "usd", "amount": 0.5}),
        _lane_row("cohort-a", "claude-sonnet", True, usage={"source": "api", "unit": "usd", "amount": 0.5}),
        _lane_row("cohort-b", "astra-high", True, usage={"source": "api", "unit": "usd", "amount": 0.9}),
        _lane_row("cohort-b", "astra-high", True, usage={"source": "api", "unit": "usd", "amount": 0.9}),
    ]
    cohorts = [CohortSpec("cohort-a", repeats=2), CohortSpec("cohort-b", repeats=2)]
    out = compare_cohorts(rows, cohorts, promotion_criteria={
        "baseline": "cohort-a", "candidate": "cohort-b", "min_pass_at_1": 0.9,
    })
    a, b = out["cohorts"]["cohort-a"], out["cohorts"]["cohort-b"]
    assert a["sample_count"] == 2 and a["pass_at_1"] == 1.0
    assert b["sample_count"] == 2 and b["pass_at_1"] == 1.0
    assert a["duration_s"] == {"min": 1.0, "max": 1.0, "mean": 1.0}
    assert a["usage"] == [{"source": "api", "unit": "usd", "amount": 1.0}]
    assert b["usage"] == [{"source": "api", "unit": "usd", "amount": 1.8}]
    assert out["promotion"]["recommended"] is True
    assert out["unattributed_count"] == 0


def test_cohort_edge_missing_evidence_and_legacy_rows_are_explicit() -> None:
    rows = [
        _lane_row("cohort-a", "claude-sonnet", True),  # no usage, no review
        {"task_id": "t", "tier": "regression", "pass": True},  # legacy: no fingerprint
        _lane_row("cohort-a", "claude-sonnet", False, bank_rev="rev2"),  # fixture drift
    ]
    cohorts = [CohortSpec("cohort-a", repeats=2, fixture_rev="rev1")]
    out = compare_cohorts(rows, cohorts)
    a = out["cohorts"]["cohort-a"]
    assert out["unattributed_count"] == 1  # legacy row never joins the cohort
    assert a["usage"] is None  # never a fabricated zero-cost claim
    assert a["review_evidence"] == "unobserved"  # never "clean"
    assert a["mixed_fixture_revisions"] is True
    assert a["fixture_rev_mismatch"] is True


def test_cohort_substituted_and_unavailable_runs_excluded_from_sample() -> None:
    rows = [
        _lane_row("cohort-a", "claude-sonnet", True),
        _lane_row("cohort-a", "claude-sonnet", True, lane_status="substituted"),
        _lane_row("cohort-a", "claude-sonnet", False, lane_status="unavailable"),
    ]
    out = compare_cohorts(rows, [CohortSpec("cohort-a", repeats=1)])
    a = out["cohorts"]["cohort-a"]
    assert a["sample_count"] == 1
    assert a["excluded_substituted"] == 1
    assert a["excluded_unavailable"] == 1


def test_cohort_promotion_blocked_on_regression_or_missing_cohort() -> None:
    rows = [
        _lane_row("cohort-a", "claude-sonnet", True),
        _lane_row("cohort-a", "claude-sonnet", True),
        _lane_row("cohort-b", "astra-high", True),
        _lane_row("cohort-b", "astra-high", False),
    ]
    out = compare_cohorts(rows, [CohortSpec("cohort-a", repeats=2), CohortSpec("cohort-b", repeats=2)],
                          promotion_criteria={"baseline": "cohort-a", "candidate": "cohort-b"})
    assert out["promotion"]["recommended"] is False
    assert out["promotion"]["reasons"]

    missing = compare_cohorts(rows, [CohortSpec("cohort-a", repeats=2)],
                              promotion_criteria={"baseline": "cohort-a", "candidate": "cohort-z"})
    assert missing["promotion"]["recommended"] is False


def test_cohort_report_cli(tmp_path: Path) -> None:
    import json

    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, _lane_row("cohort-a", "claude-sonnet", True))
    _history.append_row(hp, _lane_row("cohort-a", "claude-sonnet", True))
    spec = tmp_path / "cohorts.json"
    spec.write_text(json.dumps({"cohorts": [{"id": "cohort-a", "repeats": 2}]}), encoding="utf-8")
    res = runner.invoke(evals_app, ["report", "--history", str(hp), "--cohort-spec", str(spec), "--json"])
    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    assert payload["cohorts"]["cohort-a"]["sample_count"] == 2


def test_cohort_report_cli_no_cohorts_declared_exits_1(tmp_path: Path) -> None:
    import json

    spec = tmp_path / "cohorts.json"
    spec.write_text(json.dumps({"cohorts": []}), encoding="utf-8")
    res = runner.invoke(evals_app, ["report", "--history", str(tmp_path / "h.jsonl"),
                                    "--cohort-spec", str(spec)])
    assert res.exit_code == 1
