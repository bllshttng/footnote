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


def test_dry_run_prints_composed_pane_argv(monkeypatch) -> None:
    monkeypatch.setattr(
        harness_probe,
        "_probe_argv",
        lambda harness: ["claude", "--model", "probe-model"],
    )

    result = CliRunner().invoke(harness_probe.app, ["claude"])

    assert result.exit_code == 0, result.output
    assert "argv would run: claude --model probe-model" in result.output


def test_dry_run_names_pre_registration_harness_without_crashing(monkeypatch) -> None:
    def unsupported(harness: str) -> list[str]:
        raise RuntimeError(f"{harness} has no pane argv")

    monkeypatch.setattr(harness_probe, "_probe_argv", unsupported)

    report = harness_probe.run_probe("hermes", live=False)

    assert len(report["lines"]) == 8
    assert "unsupported" in report["argv_detail"]
    assert all("unsupported" in item["detail"] for item in report["lines"])


def test_live_missing_harness_binary_returns_a_verdict(monkeypatch) -> None:
    def missing(*args, **kwargs):
        raise FileNotFoundError("missing harness")

    monkeypatch.setattr(harness_probe.subprocess, "run", missing)

    report = harness_probe.run_probe("missing-harness", live=True)

    assert any(item["status"] == "fail" for item in report["lines"])
    assert any(item["marker"] == "harness binary" for item in report["lines"])


def test_live_mode_runs_the_lifecycle_runner(monkeypatch) -> None:
    expected = [
        harness_probe.LineVerdict("SPAWN", "pass", "registry row"),
        harness_probe.LineVerdict("IDENTITY", "pass", "local store artifact"),
        harness_probe.LineVerdict("CLAIM", "pass", "live claim holder"),
        harness_probe.LineVerdict("MAIL BOTH WAYS", "pass", "worker response"),
        harness_probe.LineVerdict("VIEW", "skip", "harness-owned screen"),
        harness_probe.LineVerdict("SURVIVE", "pass", "prior turn after process stop"),
        harness_probe.LineVerdict("ROW MATCHES", "pass", "honesty sweep"),
        harness_probe.LineVerdict("MANIFEST PINNED", "skip", "live readiness-grid capture"),
    ]
    monkeypatch.setattr(harness_probe.shutil, "which", lambda _: "/bin/true")
    monkeypatch.setattr(
        harness_probe,
        "_run_live_probe",
        lambda harness, root: expected,
    )

    report = harness_probe.run_probe("claude", live=True)

    assert [item["line"] for item in report["lines"]] == [item.line for item in expected]


def test_recall_marker_is_not_reported_as_local_store() -> None:
    verdict = harness_probe._identity_from_marker(
        session_id="remote-id",
        observed="cross-process recall nonce",
        expected_nonce="nonce",
        attempts=2,
    )

    assert verdict.status == "pass"
    assert verdict.marker == "cross-process recall nonce"


def test_mail_requires_a_distinct_worker_reply() -> None:
    assert harness_probe._mail_response_marker(
        "PROBE_MAIL=nonce", "PROBE_MAIL=nonce", "nonce"
    ) == ""
    assert harness_probe._mail_response_marker(
        "PROBE_MAIL=nonce", "PROBE_MAIL=nonce\nPROBE_REPLY=nonce", "nonce"
    ) == "PROBE_REPLY=nonce"


def test_survive_requires_successful_resume_and_new_marker() -> None:
    assert harness_probe._survive_marker(
        1, "PROBE_SEED=nonce", "PROBE_SEED=nonce", "nonce"
    ) == ""
    assert harness_probe._survive_marker(
        0, "PROBE_SEED=nonce", "PROBE_SEED=nonce", "nonce"
    ) == ""
    assert harness_probe._survive_marker(
        0,
        "PROBE_SEED=nonce",
        "PROBE_SEED=nonce\nPROBE_SURVIVE=nonce",
        "nonce",
    ) == "PROBE_SURVIVE=nonce"


def test_failed_pane_read_is_not_a_screen() -> None:
    assert harness_probe._screen_marker(1, "pane not found") == ""
    assert harness_probe._screen_marker(0, "idle prompt") == "idle prompt"


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
    monkeypatch.setattr(
        harness_probe,
        "run_instrument",
        lambda *args, **kwargs: (0, "=== 3. findings ===\n=== 4. lists ===\n"),
    )
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
        if relative.endswith("harness_capabilities.toml"):
            content = "[harness.pi]\n"
        elif relative.endswith("provider.rs"):
            content = 'pub const KNOWN_PROVIDERS: &[&str] = &["pi"];\n'
        else:
            content = (
                "KNOWN_HARNESSES = ('pi',)\n"
                "READABLE_PROVIDERS = ('pi',)\n"
                "PANE_HOSTABLE_PROVIDERS = ('pi',)\n"
                "_SESSION_BINDING_HARNESSES = ('pi',)\n"
                "_AMBIENT_NAMES = ('PI_HOME',)\n"
            )
        path.write_text(content, encoding="utf-8")
    verdict = harness_probe.line_row_matches("pi", repo_root=tmp_path)

    assert verdict.status == "pass"
    assert "freshness" in verdict.detail
    assert "registration" in verdict.detail


def test_row_match_ignores_harness_name_in_negative_claim_counts(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(
        harness_probe,
        "run_instrument",
        lambda command, **kwargs: (
            0,
            "=== 2. negative claims ===\n  claude: 3\n=== 3. a negative claim beside a harness-NAMED implementation ===\n",
        ),
    )
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
        if relative.endswith("harness_capabilities.toml"):
            content = "[harness.claude]\n"
        elif relative.endswith("provider.rs"):
            content = 'pub const KNOWN_PROVIDERS: &[&str] = &["claude"];\n'
        else:
            content = (
                "KNOWN_HARNESSES = ('claude',)\n"
                "READABLE_PROVIDERS = ('claude',)\n"
                "PANE_HOSTABLE_PROVIDERS = ('claude',)\n"
                "_SESSION_BINDING_HARNESSES = ('claude',)\n"
                "_AMBIENT_NAMES = ('CLAUDE_HOME',)\n"
            )
        path.write_text(content, encoding="utf-8")

    verdict = harness_probe.line_row_matches("claude", repo_root=tmp_path)

    assert verdict.status == "pass"


def test_row_match_fails_missing_required_registration(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(harness_probe, "run_instrument", lambda *args, **kwargs: (0, ""))
    table = tmp_path / "crates/fno-agents/src/harness_capabilities.toml"
    table.parent.mkdir(parents=True)
    table.write_text("[harness.claude]\n", encoding="utf-8")
    for relative in (
        "cli/src/fno/harness_names.py",
        "cli/src/fno/agents/harnesses/__init__.py",
        "cli/src/fno/agents/mux_spawn.py",
        "crates/fno-agents/src/provider.rs",
        "cli/src/fno/hermetic.py",
    ):
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("", encoding="utf-8")

    verdict = harness_probe.line_row_matches("claude", repo_root=tmp_path)

    assert verdict.status == "fail"
    assert "KNOWN_HARNESSES" in verdict.detail


def test_manifest_pinned_requires_readiness_marker() -> None:
    verdict = harness_probe.line_manifest_pinned(
        harness="claude",
        result=(0, "capture-readiness-grid: wrote fixture"),
    )

    assert verdict.status == "fail"
    assert verdict.marker == "live readiness-grid capture"


def test_instrument_timeout_is_longer_than_status_timeout(monkeypatch, tmp_path: Path) -> None:
    seen = {}

    def run(*args, **kwargs):
        seen["timeout"] = kwargs["timeout"]
        return harness_probe.subprocess.CompletedProcess(args[0], 0, "ok", "")

    monkeypatch.setattr(harness_probe.subprocess, "run", run)

    assert harness_probe.run_instrument(["true"], cwd=tmp_path) == (0, "ok")
    assert seen["timeout"] == harness_probe.INSTRUMENT_TIMEOUT_S
