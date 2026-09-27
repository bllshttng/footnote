"""The declared release qualification matrix (AC1-HP / AC1-EDGE).

The manifest at evals/fixtures/product-delivery/qualification.json is the
expected-set authority: every scenario/repository/repeat carries a stable
identity and pinned configuration BEFORE any run begins, and the native fold
(fno-agents evals-trend --qualification) consumes exactly this contract.
These tests pin that contract; the fold itself is tested in Rust.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
MANIFEST_PATH = REPO_ROOT / "evals" / "fixtures" / "product-delivery" / "qualification.json"

EXPECTED_FAMILIES = {
    "install-first-use": "isolated install and first use",
    "viewer-detach-cold-resume": "viewer detach plus separately tested cold native resume",
    "two-worker-missing-result": "two-worker coordination with a missing required result",
    "delivery-evidence-failure": "failed or stale delivery evidence under current policy",
    "operator-effort-per-outcome": "human effort per accepted outcome",
}

SHA40 = re.compile(r"^[0-9a-f]{40}$")


@pytest.fixture(scope="module")
def manifest() -> dict:
    return json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))


# --- AC1-HP: stable identity and pinned configuration before any run --------


def test_manifest_is_declared_at_version_1(manifest: dict) -> None:
    assert manifest["manifest_version"] == 1
    assert manifest["kind"] == "product-qualification"


def test_release_pins_are_real_revisions(manifest: dict) -> None:
    release = manifest["release"]
    assert release["product"] == "footnote"
    assert release["footnote_version"]
    assert SHA40.match(release["footnote_rev"]), release["footnote_rev"]
    assert SHA40.match(release["bank_rev"]), release["bank_rev"]
    assert release["declared_at"]


def test_declared_tool_versions_are_non_empty_strings(manifest: dict) -> None:
    tools = manifest["tool_versions"]
    assert isinstance(tools, dict) and tools
    assert all(isinstance(v, str) and v for v in tools.values())


def test_units_are_declared_for_every_measured_axis(manifest: dict) -> None:
    units = manifest["units"]
    assert units["duration"] == "seconds"
    assert units["effort"] == "minutes"
    assert units["spend"] == "usd"


def test_five_scenario_families_with_stable_ids(manifest: dict) -> None:
    scenarios = manifest["scenarios"]
    assert {s["id"] for s in scenarios} == set(EXPECTED_FAMILIES)
    assert len(scenarios) == 5
    for s in scenarios:
        assert s["family"] == EXPECTED_FAMILIES[s["id"]]


def test_every_expected_case_has_a_stable_identity(manifest: dict) -> None:
    repeats = manifest["comparison"]["repeats_per_scenario"]
    seen: set[str] = set()
    for s in manifest["scenarios"]:
        assert s["repeats"] == repeats
        expected = [f"{s['id']}/{s['repo']}/r{i}" for i in range(1, repeats + 1)]
        assert s["expected_cases"] == expected
        assert not seen.intersection(s["expected_cases"]), f"duplicate case in {s['id']}"
        seen.update(s["expected_cases"])


def test_scenarios_pin_bank_task_cohort_and_serving(manifest: dict) -> None:
    for s in manifest["scenarios"]:
        assert s["bank_task"] == manifest["bank_task"]
        assert s["cohort"] == s["id"], "the cohort tag IS the scenario id"
        assert s["repo"] in manifest["comparison"]["runnable_repositories"]
        assert s["serving"] in ("grade-only", "headless-worker")
        assert "lane" in s


def test_grade_only_and_live_trials_are_distinct_cohorts(manifest: dict) -> None:
    by_class: dict[str, set[str]] = {}
    for s in manifest["scenarios"]:
        by_class.setdefault(s["cohort_class"], set()).add(s["id"])
    assert set(by_class) == {"conformance", "live-trial"}
    for s in manifest["scenarios"]:
        if s["cohort_class"] == "conformance":
            assert s["serving"] == "grade-only", s["id"]
        if s["cohort_class"] == "live-trial":
            assert s["serving"] == "headless-worker", s["id"]


# --- AC1-EDGE: missing, unsupported and foreign work stays visible -----------


def test_unmeasured_measurements_stay_declared_not_hidden(manifest: dict) -> None:
    measurements = manifest["measurements"]
    assert set(measurements) >= {"operator_active_minutes", "observed_spend_usd"}
    for name, m in measurements.items():
        assert m["unit"], name
        assert m["status"] in ("measured", "not_measured"), name
        if m["status"] == "not_measured":
            assert m["source"] is None, name


def test_foreign_targets_declare_why_they_cannot_run(manifest: dict) -> None:
    runnable = set(manifest["comparison"]["runnable_repositories"])
    for target in manifest["comparison"]["import_only_targets"]:
        repo = target["repository"]
        assert repo not in runnable
        assert target["reason"], repo


def test_import_entries_carry_provenance_or_are_absent(manifest: dict) -> None:
    for imp in manifest["imports"]:
        assert imp["repository"] in {
            t["repository"] for t in manifest["comparison"]["import_only_targets"]
        }
        for key in ("tool_version", "scenario", "result", "evidence"):
            assert imp.get(key), imp
        assert imp["result"] in ("pass", "fail", "unmeasured")
        assert isinstance(imp["evidence"], dict) and imp["evidence"]
        assert isinstance(imp.get("false_success", False), bool)


def test_every_scenario_is_accounted_as_runnable_or_import_only(manifest: dict) -> None:
    # No expected case may silently disappear: each declared case belongs to a
    # runnable repository, and the repeat count is the comparison constant.
    runnable = set(manifest["comparison"]["runnable_repositories"])
    repeats = manifest["comparison"]["repeats_per_scenario"]
    total_expected = 0
    for s in manifest["scenarios"]:
        assert s["repo"] in runnable, s["id"]
        total_expected += repeats
    assert total_expected == 10
