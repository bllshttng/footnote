#!/usr/bin/env python3
"""Marker tests for the fence gate in scripts/save-session.py.

A markdown code fence does not nest and does not span messages, so one
message with an odd fence count inverts every block after it in the saved
transcript. format_transcript must close an odd fence at the role heading,
which is a boundary a fence can never legally cross.

Run: python3 tests/test_save_session_fences.py   OR   pytest tests/test_save_session_fences.py
"""
import importlib.util
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
_spec = importlib.util.spec_from_file_location(
    "save_session", REPO_ROOT / "scripts" / "save-session.py"
)
save_session = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(save_session)


def fence_indexes(lines: list[str]) -> list[int]:
    return [i for i, ln in enumerate(lines) if ln.lstrip().startswith("```")]


def test_odd_assistant_fence_closes_before_next_heading():
    """Truncated-block producer: assistant text opens ``` and never closes."""
    rounds = [
        (None, "prose\n```python\nprint(1)\n"),
        ("second question", "fine answer"),
    ]
    out = save_session.format_transcript(rounds, "fm")
    lines = out.splitlines()
    fences = fence_indexes(lines)
    assert fences, "odd-fence input must still emit fences"
    assert len(fences) % 2 == 0, f"fence count must be even, got {len(fences)}"
    next_heading = lines.index("## User", lines.index("## Assistant"))
    assert fences[-1] < next_heading, (
        "closing fence must sit before the next round's role heading"
    )


def test_odd_user_fence_closes_before_assistant_heading():
    """Nested-fence producer: user text wrapped in a bare ``` it also opens."""
    rounds = [("```/target auto-merge <id>```", "fine answer")]
    out = save_session.format_transcript(rounds, "fm")
    lines = out.splitlines()
    fences = fence_indexes(lines)
    assert len(fences) % 2 == 0, f"fence count must be even, got {len(fences)}"
    heading = lines.index("## Assistant")
    assert fences[-1] < heading, (
        "closing fence must sit before the ## Assistant heading"
    )


def test_even_fence_input_unchanged():
    """Balanced input must render byte-identical to the pre-gate writer."""
    rounds = [(None, "```python\nprint(1)\n```")]
    out = save_session.format_transcript(rounds, "fm")
    expected = "\n".join(
        ["fm", "---", "", "## Assistant", "```python\nprint(1)\n```", ""]
    )
    assert out == expected, "balanced input must gain no closing fence"


def test_multi_round_only_middle_odd_stays_balanced():
    rounds = [
        (None, "clean"),
        ("middle", "```bash\nfno doctor test\n"),
        (None, "also clean"),
    ]
    out = save_session.format_transcript(rounds, "fm")
    lines = out.splitlines()
    fences = fence_indexes(lines)
    assert len(fences) % 2 == 0, f"fence count must be even, got {len(fences)}"
    # 2 headings share the text "## Assistant"; the third round's is last.
    headings = [i for i, ln in enumerate(lines) if ln == "## Assistant"]
    assert fences[-1] < headings[-1], "closer must not leak past the last round"


if __name__ == "__main__":
    failed = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS {name}")
            except AssertionError as e:
                failed += 1
                print(f"FAIL {name}: {e}")
    sys.exit(1 if failed else 0)
