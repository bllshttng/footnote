"""Regression tests for scripts/ci/check-preamble-budget.sh.

Gate output is captured directly with subprocess.run, never piped through a
formatter, so each assertion reads the gate's own return code.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import time
from pathlib import Path

import pytest
import yaml
from typer.testing import CliRunner

ROOT = Path(__file__).resolve().parents[3]
GATE = ROOT / "scripts" / "ci" / "check-preamble-budget.sh"
WORKFLOW = ROOT / ".github" / "workflows" / "preamble-budget.yml"


def _ceiling_bytes_from_script() -> int:
    """Read CEILING_BYTES from the gate so a raise can never desync the test."""
    match = re.search(r"^CEILING_BYTES=(\d+)", GATE.read_text(), re.MULTILINE)
    assert match is not None, f"CEILING_BYTES assignment not found in {GATE}"
    return int(match.group(1))


CEILING_BYTES = _ceiling_bytes_from_script()


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


def test_json_manifest_reports_hashes_without_changing_exit_semantics(
    tmp_path: Path,
) -> None:
    """Task 1.1: the static gate doubles as a machine-readable census seam."""
    _write_fixed_roots(tmp_path, agents_bytes=500)
    result = subprocess.run(
        ["bash", str(GATE), "--json", str(tmp_path)],
        capture_output=True,
        text=True,
        timeout=5,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["total_bytes"] == 610
    assert payload["estimated_tokens"] == 153
    assert payload["ceiling_bytes"] == CEILING_BYTES
    by_path = {record["path"]: record for record in payload["sources"]}
    assert by_path["AGENTS.md"] == {
        "path": "AGENTS.md",
        "bytes": 500,
        "estimated_tokens": 125,
        "content_hash": hashlib.sha256(b"a" * 500).hexdigest(),
    }


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
    assert f" / {CEILING_BYTES} bytes" not in result.stdout


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


def _covered_by(path: str, filters: list[str]) -> bool:
    """True when a GitHub paths filter would make `path` fire the job."""
    for entry in filters:
        if entry == path:
            return True
        if entry.endswith("/**") and path.startswith(entry[:-2]):
            return True
    return False


def test_workflow_filter_covers_every_measured_path() -> None:
    """AC9-EDGE: no measured path can change without making the job reachable.

    Coverage, not literal membership: `skills/**` subsumes the individual
    `skills/using-fno/SKILL.md` entry it replaced, and it has to, because the
    gate now measures every `skills/*/SKILL.md` description. A filter that named
    only using-fno would leave the descriptions ceiling unreachable from CI.
    """
    raw = WORKFLOW.read_text(encoding="utf-8")
    workflow = yaml.safe_load(raw)
    triggers = workflow[True]
    push_paths = triggers["push"]["paths"]
    pull_request_paths = triggers["pull_request"]["paths"]
    measured = [
        "AGENTS.md",
        "CLAUDE.md",
        ".claude/rules/worktrees.md",
        "skills/using-fno/SKILL.md",
        "skills/mail/SKILL.md",
        "skills/target/SKILL.md",
        "scripts/ci/check-preamble-budget.sh",
        ".github/workflows/preamble-budget.yml",
    ]

    uncovered = [p for p in measured if not _covered_by(p, push_paths)]
    assert not uncovered, f"paths the workflow would not fire on: {uncovered}"
    assert push_paths == pull_request_paths
    assert "&preamble_budget_paths" in raw
    assert "*preamble_budget_paths" in raw


def test_workflow_filter_would_miss_an_unlisted_path() -> None:
    """Negative control: the coverage helper can actually return False."""
    workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
    push_paths = workflow[True]["push"]["paths"]

    assert not _covered_by("cli/src/fno/doctor.py", push_paths)


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
        lambda argv: (0, f"preamble: 36213 / {CEILING_BYTES} B (~9.1K tok/turn)\n", ""),
    )

    line = doctor._preamble_budget_line(ROOT, cwd=ROOT / "cli")

    assert line == f"preamble: 36213 / {CEILING_BYTES} B (~9.1K tok/turn)"


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


def test_doctor_surfaces_a_missing_quiet_report(
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
        lambda argv: (0, "unexpected output\n", ""),
    )

    assert doctor._preamble_budget_line(ROOT, cwd=ROOT) == (
        "preamble: unavailable (check emitted no report)"
    )


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
        (f"preamble: 36213 / {CEILING_BYTES} B (~9.1K tok/turn)", 1),
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


def test_doctor_timeout_kills_gate_descendants(tmp_path: Path) -> None:
    """A timed-out gate must not leave its byte-counting child behind."""
    from fno import doctor

    pid_path = tmp_path / "child.pid"
    result = doctor._bounded_command(
        [
            "bash",
            "-c",
            'sleep 30 & child=$!; printf "%s" "$child" > "$1"; wait "$child"',
            "bash",
            str(pid_path),
        ]
    )

    assert result is None
    child_pid = int(pid_path.read_text(encoding="utf-8"))
    deadline = time.monotonic() + 1
    while time.monotonic() < deadline:
        try:
            os.kill(child_pid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    else:
        pytest.fail(f"timed-out gate descendant {child_pid} is still alive")


def test_doctor_resolves_the_worktree_from_a_subdirectory() -> None:
    """The report stays visible below the repo root without trusting other repos."""
    from fno import doctor

    line = doctor._preamble_budget_line(ROOT / "cli", cwd=ROOT / "cli")

    assert line is not None
    assert line.startswith("preamble: ")
    assert f" / {CEILING_BYTES} B (" in line


# --- The second budget: always-loaded skill descriptions ---------------------


def _gate_constant(name: str) -> int:
    match = re.search(rf"^{name}=(\d+)", GATE.read_text(), re.MULTILINE)
    assert match is not None, f"{name} assignment not found in {GATE}"
    return int(match.group(1))


def _add_skill(
    repo_root: Path, name: str, description: str, *, model_invoked: bool = True
) -> None:
    skill = repo_root / "skills" / name / "SKILL.md"
    skill.parent.mkdir(parents=True, exist_ok=True)
    lines = ["---", f"name: {name}", f'description: "{description}"']
    if not model_invoked:
        lines.append("disable-model-invocation: true")
    lines += ["---", "", "body", ""]
    skill.write_text("\n".join(lines), encoding="utf-8")


def _measured_tree(
    tmp_path: Path, *, descriptions: dict[str, str] | None = None
) -> Path:
    """A fixture repo the gate can measure, with real skill frontmatter."""
    _write_fixed_roots(tmp_path, agents_bytes=500)
    for name, description in (descriptions or {"alpha": "a" * 100}).items():
        _add_skill(tmp_path, name, description)
    return tmp_path


def _run_at(
    repo_root: Path, *, force_band: bool = False
) -> subprocess.CompletedProcess[str]:
    env = dict(os.environ)
    if force_band:
        env["PREAMBLE_BAND_FORCE"] = "1"
    return subprocess.run(
        ["bash", str(GATE), str(repo_root)],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )


def test_descriptions_are_measured_and_reported() -> None:
    """The gate measured ~85% of the always-loaded text before this budget."""
    result = subprocess.run(
        ["bash", str(GATE)], cwd=ROOT, capture_output=True, text=True, timeout=30
    )

    assert result.returncode == 0, result.stderr
    assert "descriptions (always-loaded skill pointers):" in result.stdout
    assert "always loaded, both budgets:" in result.stdout


def test_descriptions_total_matches_an_independent_reader() -> None:
    """Cross-check the gate's regex reader against a yaml-based one.

    Two readers that disagree mean one of them is wrong, and the budget is only
    worth having if the number under it is the real number.
    """
    expected = 0
    for skill in sorted((ROOT / "skills").glob("*/SKILL.md")):
        text = skill.read_text(encoding="utf-8")
        match = re.match(r"^---\n(.*?)\n---\n", text, re.S)
        if not match:
            continue
        front = yaml.safe_load(match.group(1)) or {}
        if front.get("disable-model-invocation"):
            continue
        description = front.get("description")
        if not description:
            continue
        expected += len(str(description).encode("utf-8"))

    payload = json.loads(
        subprocess.run(
            ["bash", str(GATE), "--json"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        ).stdout
    )

    assert payload["descriptions"]["total_bytes"] == expected
    assert payload["always_loaded_bytes"] == (
        payload["total_bytes"] + payload["descriptions"]["total_bytes"]
    )


def test_user_invoked_skill_costs_nothing(tmp_path: Path) -> None:
    """disable-model-invocation strips the pointer, so it leaves the budget."""
    repo = _measured_tree(tmp_path, descriptions={"counted": "c" * 50})
    _add_skill(repo, "hidden", "h" * 500, model_invoked=False)

    payload = json.loads(
        subprocess.run(
            ["bash", str(GATE), "--json", str(repo)],
            capture_output=True,
            text=True,
            timeout=30,
        ).stdout
    )

    assert payload["descriptions"]["count"] == 1
    assert payload["descriptions"]["total_bytes"] == 50


def test_description_growth_past_the_ceiling_fails(tmp_path: Path) -> None:
    """A fatter pointer is a real per-turn cost and has to be paid for."""
    ceiling = _gate_constant("DESCRIPTIONS_CEILING_BYTES")
    repo = _measured_tree(tmp_path, descriptions={"padding": "x" * (ceiling + 1)})

    result = _run_at(repo)

    assert result.returncode == 1
    assert "skill descriptions are" in result.stderr
    assert "over the" in result.stderr


def test_unbanked_description_cut_fails_and_names_the_value(tmp_path: Path) -> None:
    """The band is two-sided here too: a cut is banked or it is not a cut."""
    band = _gate_constant("DESCRIPTIONS_BAND_BYTES")
    ceiling = _gate_constant("DESCRIPTIONS_CEILING_BYTES")
    tiny = ceiling - band - 100
    repo = _measured_tree(tmp_path, descriptions={"small": "s" * tiny})

    result = _run_at(repo, force_band=True)

    assert result.returncode == 1
    assert f"DESCRIPTIONS_CEILING_BYTES={tiny + band}" in result.stderr


def test_broken_reader_cannot_pass_as_zero(tmp_path: Path) -> None:
    """A zero must never read as 'the skills are free'.

    The grep cross-check exists because the reader and the budget would
    otherwise fail the same way: a parser that stops matching returns 0 bytes,
    indistinguishable from a repo that genuinely has no model-invoked skills.
    """
    repo = _measured_tree(tmp_path)
    broken = repo / "skills" / "broken" / "SKILL.md"
    broken.parent.mkdir(parents=True, exist_ok=True)
    # A description line grep can see, with no frontmatter fence for the reader.
    broken.write_text("description: visible to grep only\n", encoding="utf-8")
    (repo / "skills" / "alpha" / "SKILL.md").unlink()

    result = _run_at(repo)

    assert result.returncode == 1
    assert "the frontmatter reader broke" in result.stderr


def test_no_skills_at_all_is_not_a_failure(tmp_path: Path) -> None:
    """A consumer checkout that vendors the rules without the skills is normal."""
    _write_fixed_roots(tmp_path, agents_bytes=500)

    result = _run_at(tmp_path)

    assert result.returncode == 0, result.stderr
    assert "descriptions (always-loaded skill pointers):" not in result.stdout


def test_multiline_description_is_refused_not_undercounted(tmp_path: Path) -> None:
    """A folded scalar would measure as its marker, a silent undercount."""
    repo = _measured_tree(tmp_path)
    folded = repo / "skills" / "folded" / "SKILL.md"
    folded.parent.mkdir(parents=True, exist_ok=True)
    folded.write_text(
        "---\nname: folded\ndescription: >\n  folded across lines\n---\n\nbody\n",
        encoding="utf-8",
    )

    result = _run_at(repo)

    assert result.returncode == 1
    assert "multi-line description scalar" in result.stderr


# --- The two-sided file ceiling ----------------------------------------------


def test_live_repo_sits_inside_the_band() -> None:
    """The ceiling tracks the floor: this is what a banked cut looks like."""
    band = _gate_constant("RATCHET_NUDGE_BYTES")
    result = subprocess.run(
        ["bash", str(GATE)], cwd=ROOT, capture_output=True, text=True, timeout=30
    )

    assert result.returncode == 0, result.stderr
    spare = int(re.search(r"tok at 4 B/tok\), (\d+) to spare", result.stdout).group(1))
    assert spare <= band, f"{spare} B spare exceeds the {band} B band"


def test_unbanked_file_cut_fails_and_names_the_value(tmp_path: Path) -> None:
    """This path used to print an advisory and exit 0, so no cut was banked."""
    band = _gate_constant("RATCHET_NUDGE_BYTES")
    # Park the descriptions budget inside its own band, or it fails first and
    # this test reads the wrong gate's message.
    in_band = _gate_constant("DESCRIPTIONS_CEILING_BYTES") - 100
    repo = _measured_tree(tmp_path, descriptions={"alpha": "a" * in_band})

    result = _run_at(repo, force_band=True)

    assert result.returncode == 1
    assert "more than the" in result.stderr
    total = int(re.search(r"check-preamble-budget: (\d+) /", result.stderr).group(1))
    assert f"CEILING_BYTES={total + band}" in result.stderr


def test_band_is_scoped_to_the_gates_own_repo(tmp_path: Path) -> None:
    """A fixture root is not what the constants budget, so the band stays off."""
    repo = _measured_tree(tmp_path)

    result = _run_at(repo)

    assert result.returncode == 0, result.stderr
    assert "more than the" not in result.stderr

