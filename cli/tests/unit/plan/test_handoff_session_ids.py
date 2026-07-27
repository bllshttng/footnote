"""The self-handoff writer is the fourth writer of plan frontmatter.

`skills/target/scripts/handoff.sh` section 8d appends this session's id to the
plan's `session_ids`. It is a separate implementation from `_stamp.py` and
`mutate_doc.py`, and it is reachable on its own - so a round-trip guard that
covers only the other two is decorative.

These tests exec the real embedded snippet extracted from the shipped script,
not a copy, so the guard cannot drift from the code it protects.
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

import pytest

from fno.plan._stamp import parse_frontmatter

_REPO_ROOT = Path(__file__).resolve().parents[4]
_HANDOFF = _REPO_ROOT / "skills" / "target" / "scripts" / "handoff.sh"


def _section_8d() -> str:
    """Pull the 8d python heredoc out of the shipped shell script."""
    text = _HANDOFF.read_text(encoding="utf-8")
    marker = "# 8d."
    start = text.index(marker)
    body = text[start:]
    snippet = re.search(r"<<'PYEOF'\n(.*?)\nPYEOF", body, re.DOTALL)
    assert snippet, "section 8d python heredoc not found in handoff.sh"
    return snippet.group(1)


def _run_8d(plan: Path, sid: str) -> None:
    proc = subprocess.run(
        [sys.executable, "-", str(plan), sid],
        input=_section_8d(),
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 0, proc.stderr


@pytest.mark.parametrize(
    "frontmatter, existing",
    [
        ("session_ids: [OLD_SID]\n", ["OLD_SID"]),          # inline
        ("session_ids:\n  - OLD_SID\n", ["OLD_SID"]),        # block
        ("session_ids: []\n", []),                            # empty inline
        ("session_ids:\n", []),                               # bare header
    ],
)
def test_appends_without_losing_existing_ids(
    tmp_path: Path, frontmatter: str, existing: list[str]
) -> None:
    """Whatever form the doc arrived in, the new id is added and none are lost.

    The block case used to rewrite the bare header as `session_ids: [NEW]` and
    leave the indented children stranded underneath it - malformed YAML that
    also silently dropped every id already recorded.
    """
    plan = tmp_path / "plan.md"
    plan.write_text(f"---\ntitle: t\n{frontmatter}status: ready\n---\nbody\n", encoding="utf-8")

    _run_8d(plan, "NEW_SID")

    fields, _block, _rest = parse_frontmatter(plan.read_text(encoding="utf-8"))
    assert fields["session_ids"] == existing + ["NEW_SID"]
    assert fields["status"] == "ready"


def test_missing_key_is_created(tmp_path: Path) -> None:
    plan = tmp_path / "plan.md"
    plan.write_text("---\ntitle: t\n---\nbody\n", encoding="utf-8")

    _run_8d(plan, "NEW_SID")

    fields, _block, _rest = parse_frontmatter(plan.read_text(encoding="utf-8"))
    assert fields["session_ids"] == ["NEW_SID"]


def test_block_form_append_keeps_later_keys_parseable(tmp_path: Path) -> None:
    """The regression shape: keys AFTER the block list must survive."""
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\ntitle: t\nsession_ids:\n  - A\n  - B\nstatus: ready\nurls: [u1]\n---\nbody\n",
        encoding="utf-8",
    )

    _run_8d(plan, "C")

    fields, _block, _rest = parse_frontmatter(plan.read_text(encoding="utf-8"))
    assert fields["session_ids"] == ["A", "B", "C"]
    assert fields["urls"] == ["u1"]
