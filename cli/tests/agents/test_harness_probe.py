from __future__ import annotations

import inspect
import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents import harness_probe


def test_verdicts_require_positive_markers() -> None:
    with pytest.raises(ValueError, match="marker"):
        harness_probe.LineVerdict(line="SPAWN", status="pass", marker="")

    with pytest.raises(ValueError, match="marker"):
        harness_probe.LineVerdict(line="SPAWN", status="fail", marker="")


def test_unexpected_negative_retries_three_times() -> None:
    calls = 0

    def read_marker() -> str:
        nonlocal calls
        calls += 1
        return "unexpected-negative"

    verdict = harness_probe.retry_marker(
        line="IDENTITY",
        marker_name="cross-process recall nonce",
        read_marker=read_marker,
        matches=lambda value: value == "nonce-returned",
    )

    assert calls == 3
    assert verdict.status == "fail"
    assert verdict.attempts == 3
    assert verdict.marker == "cross-process recall nonce"


def test_probe_dry_run_has_eight_lines_and_does_not_spawn(monkeypatch) -> None:
    runner = CliRunner()
    spawned: list[object] = []
    monkeypatch.setattr(harness_probe.subprocess, "run", lambda *a, **k: spawned.append(a))

    result = runner.invoke(harness_probe.app, ["claude"])

    assert result.exit_code == 0, result.output
    assert len([line for line in result.output.splitlines() if line.startswith(("PASS", "FAIL", "SKIP"))]) == 8
    assert "would run" in result.output.lower()
    assert not any("claude" in str(call) for call in spawned)


def test_dry_run_reports_composed_pane_argv(monkeypatch) -> None:
    expected = ["claude", "--session-id", "probe-session"]
    monkeypatch.setattr(harness_probe, "_probe_argv", lambda harness: expected)

    report = harness_probe.run_probe("claude", live=False)

    assert report["argv"] == expected


def test_live_missing_harness_binary_returns_a_verdict(monkeypatch) -> None:
    def missing(*args, **kwargs):
        raise FileNotFoundError("missing harness")

    monkeypatch.setattr(harness_probe.subprocess, "run", missing)

    report = harness_probe.run_probe("missing-harness", live=True)

    assert any(item["status"] == "fail" for item in report["lines"])
    assert any(item["marker"] == "harness binary" for item in report["lines"])


def test_doctor_command_exposes_dry_run_without_spawning(monkeypatch) -> None:
    from fno.cli import app

    spawned: list[object] = []
    monkeypatch.setattr(harness_probe.subprocess, "run", lambda *a, **k: spawned.append(a))
    result = CliRunner().invoke(app, ["doctor", "harness", "claude"])

    assert result.exit_code == 0, result.output
    assert result.output.count("marker=") == 8
    assert not any("claude" in str(call) for call in spawned)


def test_json_dry_run_has_structured_lines() -> None:
    result = CliRunner().invoke(harness_probe.app, ["--json", "claude"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert len(payload["lines"]) == 8
    assert all(item["marker"] for item in payload["lines"])


def test_lines_other_than_row_match_do_not_read_capability_table() -> None:
    forbidden = ("harness_map", "harness_names", "harness_capabilities.toml")
    for name in (
        "line_spawn",
        "line_identity",
        "line_claim",
        "line_mail",
        "line_view",
        "line_survive",
        "line_manifest_pinned",
    ):
        source = inspect.getsource(getattr(harness_probe, name))
        assert not any(token in source for token in forbidden), name


def test_founder_defect_missing_registry_row_is_a_failure() -> None:
    verdict = harness_probe.line_spawn(
        receipt_output="status: live",
        registry_row=None,
        gate_output="",
    )

    assert verdict.status == "fail"
    assert verdict.marker == "registry row"
    assert "registry row" in verdict.detail


def test_identity_accepts_cross_process_recall_without_local_store() -> None:
    verdict = harness_probe.line_identity(
        session_id="chat-id",
        store_match=False,
        recalled_nonce="nonce-returned",
        expected_nonce="nonce-returned",
    )

    assert verdict.status == "pass"
    assert verdict.marker == "cross-process recall nonce"


def test_line_seven_reports_instrument_results(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(harness_probe, "run_instrument", lambda *args, **kwargs: (0, "clean"))
    for relative in (
        "crates/fno-agents/src/harness_capabilities.toml",
        "cli/src/fno/harness_names.py",
        "cli/src/fno/agents/harnesses/__init__.py",
        "cli/src/fno/agents/mux_spawn.py",
        "crates/fno-agents/src/provider.rs",
        "cli/src/fno/hermetic.py",
    ):
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("'pi'\n[harness.pi]\n", encoding="utf-8")
    verdict = harness_probe.line_row_matches("pi", repo_root=tmp_path)

    assert verdict.status == "pass"
    assert "freshness" in verdict.detail
    assert "registration" in verdict.detail
