"""The probe reports features beside fields (x-a3e8).

The feature declarations ride the SAME runner as the field declarations:
one loop, one authority executor, one vocabulary. These tests pin the
comparison rules for the declared catalog keys, the row-state detail on
behavioral keys, and the inherited UNKNOWN handling (binary absent,
authority failure) - never a second executor.
"""
from __future__ import annotations

import pytest

from fno.agents import capability_probe
from fno.agents.capability_probe import probe_harness
from fno.agents.harness_map import probe_declarations

FEATURE_FIELDS = sorted(
    field
    for field in probe_declarations()
    if field.startswith("features.")
)
assert len(FEATURE_FIELDS) == 10, "the closed feature set drifted; update this test"


def _report_fields(report: dict) -> dict[str, dict]:
    return {item["field"]: item for item in report["fields"]}


def test_every_declared_feature_gets_a_report_row_beside_the_fields(monkeypatch):
    monkeypatch.setattr(
        capability_probe.shutil, "which", lambda _: "/fake/agy"
    )
    monkeypatch.setattr(
        capability_probe, "_run_authority", lambda argv, cwd=None: (1, "no")
    )
    report = probe_harness("agy")
    fields = _report_fields(report)
    for field in FEATURE_FIELDS:
        assert field in fields, field
        assert fields[field]["verdict"] in capability_probe.VERDICTS
    # The pre-existing field rows are still reported beside them.
    assert "model_switch_strategy" in fields
    assert "thread" in fields


def test_a_declared_feature_against_an_unmeasured_row_is_not_a_contradiction(
    monkeypatch,
):
    monkeypatch.setattr(capability_probe.shutil, "which", lambda _: "/fake/agy")
    output = "usage: agy [OPTIONS] --mcp-server NAME"

    def fake_authority(argv, cwd=None):
        return (0, output)

    monkeypatch.setattr(capability_probe, "_run_authority", fake_authority)
    report = probe_harness("agy")
    row = _report_fields(report)["features.mcp"]
    # unmeasured makes no claim, so nothing can disagree; the evidence is
    # recorded for the row edit that follows.
    assert row["verdict"] == "AGREES"
    assert "no claim to contradict" in row["detail"]
    assert row["evidence"]


def test_a_declared_feature_contradicting_a_claiming_row_disagrees(monkeypatch):
    monkeypatch.setattr(capability_probe.shutil, "which", lambda _: "/fake/claude")
    monkeypatch.setattr(
        capability_probe, "_run_authority", lambda argv, cwd=None: (0, "plain help, no surface")
    )
    monkeypatch.setattr(
        capability_probe,
        "capabilities_or_undeclared",
        lambda harness: {
            "features": {"rpc": {"state": "native"}},
        },
    )
    report = probe_harness("claude")
    row = _report_fields(report)["features.rpc"]
    assert row["verdict"] == "DISAGREES"
    assert "reads native" in row["detail"]


def test_a_confirmed_claim_agrees(monkeypatch):
    monkeypatch.setattr(capability_probe.shutil, "which", lambda _: "/fake/claude")
    monkeypatch.setattr(
        capability_probe,
        "_run_authority",
        lambda argv, cwd=None: (0, "--serve performs the server"),
    )
    monkeypatch.setattr(
        capability_probe,
        "capabilities_or_undeclared",
        lambda harness: {
            "features": {"server": {"state": "native"}},
        },
    )
    report = probe_harness("claude")
    row = _report_fields(report)["features.server"]
    assert row["verdict"] == "AGREES"


def test_an_absent_binary_is_unknown_never_a_disagreement(monkeypatch):
    monkeypatch.setattr(capability_probe.shutil, "which", lambda _: None)
    report = probe_harness("agy")
    row = _report_fields(report)["features.rpc"]
    assert row["verdict"] == "UNKNOWN"
    assert "not on PATH" in row["detail"]


def test_a_failing_authority_is_unknown_with_its_reason(monkeypatch):
    monkeypatch.setattr(capability_probe.shutil, "which", lambda _: "/fake/agy")
    monkeypatch.setattr(
        capability_probe,
        "_run_authority",
        lambda argv, cwd=None: (124, "authority timed out after 15s: agy"),
    )
    report = probe_harness("agy")
    row = _report_fields(report)["features.rpc"]
    assert row["verdict"] == "UNKNOWN"
    assert "timed out" in row["detail"]


def test_behavioral_features_name_the_row_state_and_stay_unknown_dry():
    report = probe_harness("claude")
    fields = _report_fields(report)
    spawn_row = fields["features.spawn"]
    assert spawn_row["verdict"] == "UNKNOWN"
    assert "--live" in spawn_row["detail"]
    assert "the row declares native" in spawn_row["detail"]
    # agy carries no review stanza: unmeasured reads as no claim.
    agy_report = probe_harness("agy")
    agy_review = _report_fields(agy_report)["features.review"]
    assert agy_review["verdict"] == "UNKNOWN"
    assert "features.review" in agy_review["field"]
    assert "the row declares" not in agy_review["detail"]


def test_features_rides_the_one_existing_executor():
    # No second executor: the feature rows are produced by the same loop
    # over probe_declarations() as the field rows, so a second iteration
    # source or a second authority runner would show up as a duplicate.
    report = probe_harness("agy")
    names = [item["field"] for item in report["fields"]]
    assert len(names) == len(set(names)), "a row was emitted twice"
