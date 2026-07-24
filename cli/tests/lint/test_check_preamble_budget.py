"""Regression tests for scripts/ci/check-preamble-budget.sh.

Gate output is captured directly with subprocess.run, never piped through a
formatter, so each assertion reads the gate's own return code.
"""
from __future__ import annotations

import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
GATE = ROOT / "scripts" / "ci" / "check-preamble-budget.sh"
CEILING_BYTES = 38_000


def _run(repo_root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(GATE), str(repo_root)],
        capture_output=True,
        text=True,
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
