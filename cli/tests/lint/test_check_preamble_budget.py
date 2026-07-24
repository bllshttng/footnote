"""Regression tests for scripts/ci/check-preamble-budget.sh.

Gate output is captured directly with subprocess.run, never piped through a
formatter, so each assertion reads the gate's own return code.
"""
from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest
import yaml
from typer.testing import CliRunner

ROOT = Path(__file__).resolve().parents[3]
GATE = ROOT / "scripts" / "ci" / "check-preamble-budget.sh"
WORKFLOW = ROOT / ".github" / "workflows" / "preamble-budget.yml"
CEILING_BYTES = 38_000


def _run(repo_root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(GATE), str(repo_root)],
        capture_output=True,
        text=True,
        timeout=5,
    )


def _write_fixed_roots(
    repo_root: Path,
    *,
    agents_bytes: int,
    claude_bytes: int = 10,
    skill_bytes: int = 100,
) -> None:
    (repo_root / ".claude" / "rules").mkdir(parents=True)
    (repo_root / "skills" / "using-fno").mkdir(parents=True)
    (repo_root / "AGENTS.md").write_bytes(b"a" * agents_bytes)
    (repo_root / "CLAUDE.md").write_bytes(b"c" * claude_bytes)
    (repo_root / "skills" / "using-fno" / "SKILL.md").write_bytes(
        b"s" * skill_bytes
    )


def _reported_total(result: subprocess.CompletedProcess[str]) -> int:
    match = re.search(r"^check-preamble-budget: (\d+) /", result.stdout, re.MULTILINE)
    assert match is not None, result.stdout
    return int(match.group(1))


def _stub_doctor_command(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno import doctor, update

    monkeypatch.setattr(doctor, "_resolve_source", lambda source: ROOT)
    monkeypatch.setattr(doctor, "_source_rev", lambda source: "abc123")
    monkeypatch.setattr(doctor, "_read_marker", lambda: "abc123")
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(
        doctor,
        "_rust_report",
        lambda: {"binary": None, "revision": None},
    )
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: None)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)
    monkeypatch.setattr(doctor, "_deployed_config_keys", lambda: frozenset())
    monkeypatch.setattr(doctor, "_source_config_keys", lambda source: frozenset())
    monkeypatch.setattr(doctor, "_python_content_drift", lambda source: 0)
    monkeypatch.setattr(doctor, "_mux_front_door_report", lambda: {})
    monkeypatch.setattr(update, "stale_mux_servers", lambda: [])
    monkeypatch.setattr(doctor, "_orphan_report", lambda: [])
    monkeypatch.setattr(doctor, "_pr_watch_liveness", lambda: {})
    monkeypatch.setattr(doctor, "_dead_letter_report", lambda: {})
    monkeypatch.setattr(doctor, "_managed_block_report", lambda: {})
    monkeypatch.setattr(doctor, "_harness_surface_report", lambda: {})
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {
            "state": "ran",
            "hours": 1.0,
            "stale": False,
            "agent_installed": True,
        },
    )
    monkeypatch.setattr(
        doctor,
        "_post_merge_sync_health",
        lambda: {"state": "fresh", "stale": False},
    )
    monkeypatch.setattr(
        doctor,
        "_launch_agent_failures",
        lambda: {"applicable": True, "dead": []},
    )


def test_relocating_bytes_into_new_rule_preserves_total(tmp_path: Path) -> None:
    """AC4-EDGE: discovery follows the rules glob, not today's filenames."""
    _write_fixed_roots(tmp_path, agents_bytes=3_400)
    before = _run(tmp_path)
    assert before.returncode == 0, before.stderr
    total_before = _reported_total(before)

    (tmp_path / "AGENTS.md").write_bytes(b"a" * 400)
    (tmp_path / ".claude" / "rules" / "relocated.md").write_bytes(b"r" * 3_000)
    after = _run(tmp_path)

    assert after.returncode == 0, after.stderr
    assert abs(_reported_total(after) - total_before) <= 200
    assert ".claude/rules/relocated.md" in after.stdout


def test_report_is_sorted_and_marks_consumer(tmp_path: Path) -> None:
    """AC1-HP: the visible report orders every discovered path by bytes."""
    _write_fixed_roots(tmp_path, agents_bytes=500)
    (tmp_path / ".claude" / "rules" / "largest.md").write_bytes(b"r" * 1_000)

    result = _run(tmp_path)

    assert result.returncode == 0, result.stderr
    assert result.stdout.index(".claude/rules/largest.md") < result.stdout.index(
        "AGENTS.md"
    )
    assert "skills/using-fno/SKILL.md  [shipped to every consumer]" in result.stdout


def test_exact_ceiling_passes_and_one_byte_over_fails(tmp_path: Path) -> None:
    """AC2-HP: the comparison is strictly greater than the ceiling."""
    _write_fixed_roots(tmp_path, agents_bytes=CEILING_BYTES - 110)
    at_ceiling = _run(tmp_path)
    assert at_ceiling.returncode == 0, at_ceiling.stderr
    assert _reported_total(at_ceiling) == CEILING_BYTES

    (tmp_path / "AGENTS.md").write_bytes(b"a" * (CEILING_BYTES - 109))
    over_ceiling = _run(tmp_path)
    assert over_ceiling.returncode == 1
    assert _reported_total(over_ceiling) == CEILING_BYTES + 1
    assert "ceiling by 1 " in over_ceiling.stderr


def test_missing_fixed_root_fails_loud(tmp_path: Path) -> None:
    """AC3-ERR: an absent fixed root is never treated as zero bytes."""
    _write_fixed_roots(tmp_path, agents_bytes=100)
    (tmp_path / "AGENTS.md").unlink()

    result = _run(tmp_path)

    assert result.returncode != 0
    assert "AGENTS.md" in result.stderr
    assert " / 38000 bytes" not in result.stdout


def test_empty_rules_glob_is_legal(tmp_path: Path) -> None:
    """AC5-EDGE: no rule files contributes zero without a literal glob path."""
    _write_fixed_roots(tmp_path, agents_bytes=100)

    result = _run(tmp_path)

    assert result.returncode == 0, result.stderr
    assert _reported_total(result) == 210
    assert "*.md" not in result.stdout


def test_all_zero_byte_roots_pass(tmp_path: Path) -> None:
    """AC5-EDGE: a zero-byte preamble is valid and remains measurable."""
    _write_fixed_roots(
        tmp_path,
        agents_bytes=0,
        claude_bytes=0,
        skill_bytes=0,
    )

    result = _run(tmp_path)

    assert result.returncode == 0, result.stderr
    assert _reported_total(result) == 0


def test_breach_teaches_trade_before_raise(tmp_path: Path) -> None:
    """AC6-FR: failure output makes the recurring cost and escape explicit."""
    _write_fixed_roots(
        tmp_path,
        agents_bytes=20_000,
        claude_bytes=7_000,
        skill_bytes=12_000,
    )

    result = _run(tmp_path)

    assert result.returncode == 1
    assert "Largest:" in result.stderr
    assert "AGENTS.md 20000" in result.stderr
    assert "skills/using-fno/SKILL.md 12000" in result.stderr
    assert "CLAUDE.md 7000" in result.stderr
    assert "tok/turn" in result.stderr
    assert result.stderr.index("Trade:") < result.stderr.index("Raise CEILING_BYTES")


def test_two_positional_roots_are_rejected(tmp_path: Path) -> None:
    result = subprocess.run(
        ["bash", str(GATE), ".", str(tmp_path)],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    assert "expected at most one repo root" in result.stderr


def test_workflow_filter_covers_discovery_rule() -> None:
    """AC9-EDGE: no measured path can change without making the job reachable."""
    raw = WORKFLOW.read_text(encoding="utf-8")
    workflow = yaml.safe_load(raw)
    triggers = workflow[True]
    push_paths = triggers["push"]["paths"]
    pull_request_paths = triggers["pull_request"]["paths"]
    required = {
        "AGENTS.md",
        "CLAUDE.md",
        ".claude/rules/**",
        "skills/using-fno/SKILL.md",
        "scripts/ci/check-preamble-budget.sh",
        ".github/workflows/preamble-budget.yml",
    }

    assert required <= set(push_paths)
    assert push_paths == pull_request_paths
    assert "&preamble_budget_paths" in raw
    assert "*preamble_budget_paths" in raw


def test_doctor_report_is_silent_when_gate_is_absent(tmp_path: Path) -> None:
    """AC7-FR: consumer checkouts without the gate remain a normal case."""
    from fno import doctor

    assert doctor._preamble_budget_line(ROOT, cwd=tmp_path) is None


def test_doctor_does_not_execute_a_foreign_checkout_gate(tmp_path: Path) -> None:
    """A consumer-controlled path must never become a doctor code-execution hook."""
    init = subprocess.run(
        ["git", "init", "-q", str(tmp_path)],
        capture_output=True,
        text=True,
    )
    assert init.returncode == 0, init.stderr
    scripts = tmp_path / "scripts" / "ci"
    scripts.mkdir(parents=True)
    marker = tmp_path / "executed"
    (scripts / "check-preamble-budget.sh").write_text(
        '#!/usr/bin/env bash\ntouch "$1/executed"\necho "preamble: forged"\n',
        encoding="utf-8",
    )

    from fno import doctor

    assert doctor._preamble_budget_line(ROOT, cwd=tmp_path) is None
    assert not marker.exists()


def test_doctor_resolves_a_footnote_subdirectory(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno import doctor

    common = ROOT / ".git-common"
    monkeypatch.setattr(
        doctor,
        "_git_checkout_identity",
        lambda path: (ROOT, common),
    )
    monkeypatch.setattr(
        doctor,
        "_bounded_command",
        lambda argv: (0, "preamble: 36213 / 38000 B (~9.1K tok/turn)\n", ""),
    )

    line = doctor._preamble_budget_line(ROOT, cwd=ROOT / "cli")

    assert line == "preamble: 36213 / 38000 B (~9.1K tok/turn)"


def test_doctor_surfaces_a_present_gate_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno import doctor

    common = ROOT / ".git-common"
    monkeypatch.setattr(
        doctor,
        "_git_checkout_identity",
        lambda path: (ROOT, common),
    )
    monkeypatch.setattr(
        doctor,
        "_bounded_command",
        lambda argv: (1, "", "check-preamble-budget: required file not found: AGENTS.md\n"),
    )

    line = doctor._preamble_budget_line(ROOT, cwd=ROOT)

    assert line is not None
    assert line.startswith("preamble: unavailable")
    assert "AGENTS.md" in line


def test_doctor_deleted_cwd_degrades_to_silence(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno import doctor

    def _missing_cwd() -> Path:
        raise FileNotFoundError("deleted cwd")

    monkeypatch.setattr(doctor.Path, "cwd", staticmethod(_missing_cwd))

    assert doctor._preamble_budget_line(ROOT) is None


@pytest.mark.parametrize(
    ("preamble_line", "expected_count"),
    [
        ("preamble: 36213 / 38000 B (~9.1K tok/turn)", 1),
        (None, 0),
    ],
)
def test_doctor_command_preserves_status_and_optional_line(
    monkeypatch: pytest.MonkeyPatch,
    preamble_line: str | None,
    expected_count: int,
) -> None:
    from fno import doctor
    from fno.cli import app

    _stub_doctor_command(monkeypatch)
    monkeypatch.setattr(doctor, "_preamble_budget_line", lambda source: preamble_line)

    result = CliRunner().invoke(app, ["doctor"])

    assert result.exit_code == 0, result.exception
    assert result.stdout.count("preamble:") == expected_count


def test_non_regular_rule_fails_instead_of_blocking(tmp_path: Path) -> None:
    """A discovered device or pipe must never leave wc waiting for EOF."""
    _write_fixed_roots(tmp_path, agents_bytes=100)
    (tmp_path / ".claude" / "rules" / "stall.md").symlink_to("/dev/zero")

    result = _run(tmp_path)

    assert result.returncode != 0
    assert "not a regular file" in result.stderr
    assert ".claude/rules/stall.md" in result.stderr


def test_doctor_resolves_the_worktree_from_a_subdirectory() -> None:
    """The report stays visible below the repo root without trusting other repos."""
    from fno import doctor

    line = doctor._preamble_budget_line(ROOT / "cli", cwd=ROOT / "cli")

    assert line is not None
    assert line.startswith("preamble: ")
    assert " / 38000 B (" in line


def _stub_doctor_command(monkeypatch) -> None:
    from fno import doctor, update

    monkeypatch.setattr(doctor, "_resolve_source", lambda source: None)
    monkeypatch.setattr(doctor, "_read_marker", lambda: None)
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(doctor, "_rust_report", lambda: {"binary": None, "revision": None})
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: None)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)
    monkeypatch.setattr(doctor, "_deployed_config_keys", lambda: frozenset())
    monkeypatch.setattr(doctor, "_source_config_keys", lambda source: frozenset())
    monkeypatch.setattr(doctor, "_python_content_drift", lambda source: 0)
    monkeypatch.setattr(doctor, "_mux_front_door_report", lambda: {})
    monkeypatch.setattr(update, "stale_mux_servers", lambda: [])
    monkeypatch.setattr(doctor, "_orphan_report", lambda: [])
    monkeypatch.setattr(doctor, "_pr_watch_liveness", lambda: {})
    monkeypatch.setattr(doctor, "_dead_letter_report", lambda: {})
    monkeypatch.setattr(doctor, "_managed_block_report", lambda: {})
    monkeypatch.setattr(doctor, "_harness_surface_report", lambda: {})
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "ran", "hours": 1.0, "stale": False, "agent_installed": True},
    )
    monkeypatch.setattr(
        doctor,
        "_post_merge_sync_health",
        lambda: {"state": "fresh", "stale": False, "behind": 0, "detail": ""},
    )
    monkeypatch.setattr(
        doctor,
        "_launch_agent_failures",
        lambda: {"applicable": True, "dead": []},
    )


def test_doctor_command_preserves_silence_when_gate_is_absent(monkeypatch) -> None:
    """AC7-FR: missing gate does not alter command output or its normal status."""
    from fno import doctor
    from fno.cli import app
    from typer.testing import CliRunner

    _stub_doctor_command(monkeypatch)
    monkeypatch.setattr(doctor, "_preamble_budget_line", lambda source: None)

    result = CliRunner().invoke(app, ["doctor"])

    assert result.exit_code == 0
    assert "preamble:" not in result.stdout
    assert "check-preamble-budget" not in result.stdout


def test_doctor_command_prints_one_preamble_line(monkeypatch) -> None:
    """The human command wires one quiet report without changing its status."""
    from fno import doctor
    from fno.cli import app
    from typer.testing import CliRunner

    _stub_doctor_command(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_preamble_budget_line",
        lambda source: "preamble: 36213 / 38000 B (~9.1K tok/turn)",
    )

    result = CliRunner().invoke(app, ["doctor"])

    assert result.exit_code == 0
    assert result.stdout.count("preamble:") == 1
