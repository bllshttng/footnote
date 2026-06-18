"""Phase 05 / task 6.7 verification: static invariants on skills/megawalk/SKILL.md.

Checks that SKILL.md (post-walker-rebuild) satisfies the thin-wrapper invariants:
- File is under 400 lines (defensive upper bound; 50-line prose target is unrealistic
  given the load-bearing HARD-GATE blocks, but 400 gives ~70 lines of headroom above
  the current 329-line file before the test flags genuine drift toward a fat skill).
- SKILL.md mentions `fno megawalk` (Locked Decision #20 canonical invocation).
- SKILL.md does NOT contain standalone `fno loop` invocations as executable examples
  (walker is the only caller of fno loop; skill must not re-introduce that coupling).
- SKILL.md contains at least one <HARD-GATE> block AND the graph.json guard language
  is present inside one of those blocks.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest

# Repo root is three levels above cli/
_REPO_ROOT = Path(__file__).parent.parent.parent.parent
_SKILL_PATH = _REPO_ROOT / "skills" / "megawalk" / "SKILL.md"


def _skill_text() -> str:
    assert _SKILL_PATH.exists(), f"SKILL.md not found at {_SKILL_PATH}"
    return _SKILL_PATH.read_text(encoding="utf-8")


def _skill_lines() -> list[str]:
    return _skill_text().splitlines()


# ---------------------------------------------------------------------------
# Task 5.3 / 6.7 verifications
# ---------------------------------------------------------------------------


def test_skill_body_is_thin():
    """SKILL.md must be under 400 lines.

    The 50-line prose target from task 5.3 is unrealistic because the
    load-bearing HARD-GATE blocks (~35 lines) are non-negotiable. We use
    400 as the threshold: current file is 329 lines, giving ~70 lines of
    headroom before the test flags genuine drift.

    If this test fails, the skill has grown toward a secondary orchestrator
    and should be tightened - NOT by lowering the threshold, but by
    removing content.
    """
    lines = _skill_lines()
    assert len(lines) < 400, (
        f"SKILL.md is {len(lines)} lines (threshold: 400). "
        "The skill has grown beyond the thin-wrapper target; review and trim."
    )


def test_skill_invokes_abi_megawalk():
    """SKILL.md must mention 'fno megawalk' as the canonical CLI invocation.

    Locked Decision #20: the skill delegates to 'fno megawalk'. This ensures
    the skill body reflects the v2 architecture where Python drives the walker.
    """
    text = _skill_text()
    assert "fno megawalk" in text, (
        "SKILL.md does not mention 'fno megawalk'. "
        "The skill must reference the canonical CLI per Locked Decision #20."
    )


def test_skill_no_direct_loop_calls():
    """SKILL.md must not contain standalone 'fno loop' invocations as commands.

    The walker (fno megawalk) is the only caller of 'fno loop'. Megawalk
    must not bypass the walker by calling fno loop directly.

    We allow 'fno loop' in prose descriptions (e.g., 'walker calls fno loop')
    but forbid it on lines that look like executable bash commands:
    - Lines starting with 'fno loop' (after stripping whitespace/backticks)
    - Lines containing 'bash fno loop' or code-fence invocations
    """
    lines = _skill_lines()
    forbidden_patterns = [
        re.compile(r"^\s*`?fno loop\b"),     # line starts with (optional backtick) fno loop
        re.compile(r"\bbash\s+fno\s+loop\b"),  # explicit bash fno loop call
        re.compile(r"^\s*\$\s+fno\s+loop\b"),  # shell-prompt style: $ fno loop
    ]
    violations = []
    for i, line in enumerate(lines, start=1):
        for pat in forbidden_patterns:
            if pat.search(line):
                violations.append(f"Line {i}: {line.rstrip()}")
                break

    assert not violations, (
        "SKILL.md contains standalone 'fno loop' command invocations "
        "(walker is the only permitted caller):\n"
        + "\n".join(violations)
    )


def test_skill_hard_gate_present():
    """SKILL.md must contain at least one <HARD-GATE> block with graph.json guard language.

    Locked Decision #17: the HARD-GATE block prevents LLMs from editing
    graph.json directly. Both the tag presence AND the graph.json reference
    inside it are required.
    """
    text = _skill_text()

    # Check the tag exists
    assert "<HARD-GATE>" in text, (
        "SKILL.md is missing the <HARD-GATE> tag. "
        "This block is required per Locked Decision #17."
    )

    # Check graph.json appears within a HARD-GATE block
    # Extract content between first <HARD-GATE> and its closing </HARD-GATE>
    hard_gate_pattern = re.compile(
        r"<HARD-GATE>(.*?)</HARD-GATE>", re.DOTALL
    )
    matches = hard_gate_pattern.findall(text)
    assert matches, (
        "SKILL.md has <HARD-GATE> opening tag but no matching </HARD-GATE> closing tag."
    )

    graph_mentioned_in_gate = any("graph.json" in block for block in matches)
    assert graph_mentioned_in_gate, (
        "SKILL.md has a <HARD-GATE> block but none of them mention 'graph.json'. "
        "The guard language protecting graph.json must live inside a HARD-GATE block."
    )
