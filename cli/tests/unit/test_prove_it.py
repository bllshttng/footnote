"""prove-it: runtime evidence with a refusal where /verify would shrug.

AC6-MARKER is the load-bearing case: a PASS whose Steps list carries no
marked probe is REFUSED, and the refusal names the missing probe. Asserting
that a good run passes proves nothing about the case the rule exists for, so
every case here pins the refusal or the no-verdict state it produces.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = REPO_ROOT / "skills/review/scripts/validate-prove-it.sh"
REFERENCE = REPO_ROOT / "skills/review/references/prove-it.md"
ROUTER = REPO_ROOT / "skills/review/SKILL.md"
PLAN_VALIDATOR = REPO_ROOT / "skills/blueprint/scripts/validate-plan.sh"


def _report(tmp_path: Path, name: str, body: str, verdict: str) -> Path:
    path = tmp_path / name
    path.write_text(
        f"{body}\nfno-prove-it: {{\"verdict\":\"{verdict}\",\"claim\":\"c\"}}\n",
        encoding="utf-8",
    )
    return path


def _validate(report: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(VALIDATOR), str(report)], capture_output=True, text=True
    )


def test_selftest_passes():
    out = subprocess.run(
        ["bash", str(VALIDATOR), "--selftest"], capture_output=True, text=True
    )
    assert out.returncode == 0, out.stdout + out.stderr
    assert "6 passed, 0 failed" in out.stdout


def test_a_pass_with_no_probe_is_refused_and_named(tmp_path):
    """AC6-MARKER: the refusal is the behavior under test."""
    report = _report(
        tmp_path,
        "no-probe.md",
        "### Steps\n1. ✅ ran the route -> 200\n### Findings\n(none)",
        "PASS",
    )
    out = _validate(report)
    assert out.returncode == 1
    assert "REFUSED" in out.stderr
    assert "no marked probe" in out.stderr


def test_a_pass_with_a_marked_probe_is_accepted(tmp_path):
    report = _report(
        tmp_path,
        "good.md",
        "### Steps\n1. ✅ ran the route -> 200\n2. 🔍 empty value -> clean error\n",
        "PASS",
    )
    out = _validate(report)
    assert out.returncode == 0, out.stderr
    assert "accepted" in out.stdout


def test_fail_blocked_skip_carry_no_verdict_and_pass_through(tmp_path):
    """AC6-ERR: the literal verdict strings, each untouched by the validator."""
    for verdict in ("FAIL", "BLOCKED", "SKIP"):
        report = _report(
            tmp_path,
            f"{verdict.lower()}.md",
            "### Steps\n1. ❌ drove the route -> nothing",
            verdict,
        )
        out = _validate(report)
        assert out.returncode == 0, (verdict, out.stderr)
        assert verdict in out.stdout


def test_a_malformed_terminal_record_is_rejected(tmp_path):
    path = tmp_path / "broken.md"
    path.write_text("no terminal record here\n", encoding="utf-8")
    assert _validate(path).returncode == 2


def test_a_probe_that_finds_nothing_still_gets_its_line():
    """AC6-HP: the reference requires the held-probe line, not a bare PASS."""
    text = REFERENCE.read_text(encoding="utf-8")
    assert "still gets its line" in text
    assert "still a step" in text


def test_captured_output_travels_in_the_row_not_by_path():
    """AC6-EDGE: the reference and the probe row contract both name it."""
    text = REFERENCE.read_text(encoding="utf-8")
    assert "Captured output is evidence" in text
    assert "exit 0 with nothing captured reads SKIP" in text
    runtime = (
        REPO_ROOT / "cli/src/fno/agents/rust_runtime.py"
    ).read_text(encoding="utf-8")
    assert "exit 0 with no output reads SKIP, not pass" in runtime


def test_the_router_names_prove_it_and_its_validator():
    text = ROUTER.read_text(encoding="utf-8")
    assert "running prove-it (runtime evidence)" in text
    assert "validate-prove-it.sh" in text
    assert "REFUSES a PASS whose Steps list carries no marked probe" in text


def test_a_code_plan_without_probes_warns_advisory(tmp_path):
    """The advisory names the absence and records the reason it is not a gate."""
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\ntitle: t\nstatus: ready\nproject: fno\ndomain: code\n---\n# t\n",
        encoding="utf-8",
    )
    out = subprocess.run(
        ["bash", str(PLAN_VALIDATOR), str(plan)], capture_output=True, text=True
    )
    joined = out.stdout + out.stderr
    assert "code plan declares no done_probes" in joined
    assert "advisory" in joined


def test_a_plan_with_probes_does_not_warn(tmp_path):
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\ntitle: t\nstatus: ready\nproject: fno\ndomain: code\n"
        "done_probes:\n  - \"echo marker\"\n---\n# t\n",
        encoding="utf-8",
    )
    out = subprocess.run(
        ["bash", str(PLAN_VALIDATOR), str(plan)], capture_output=True, text=True
    )
    joined = out.stdout + out.stderr
    assert "declares done_probes" in joined
    assert "declares no done_probes" not in joined
