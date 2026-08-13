"""Tests for scripts/lib/node-id.sh, the shared shell id classifier.

The classifier is the shell-side counterpart of fno.graph._constants.
is_wellformed_node_id. The two live in different languages on purpose: the
shell resolvers keep a legacy fallback for environments where the fno Python
package is unavailable, so they cannot defer to Python. The alignment test
pins that the shell's fno verdict matches the Python authority, so the two
cannot silently drift.
"""
from __future__ import annotations

import shlex
import subprocess
from pathlib import Path

import pytest

from fno.graph._constants import is_wellformed_node_id

REPO = Path(__file__).resolve().parents[3]
NODE_ID_SH = REPO / "scripts" / "lib" / "node-id.sh"


def _kind(arg: str) -> str:
    cmd = f'source "{NODE_ID_SH}"; node_id_kind {shlex.quote(arg)}'
    out = subprocess.run(
        ["bash", "-c", cmd], capture_output=True, text=True, check=True
    )
    return out.stdout.strip()


@pytest.mark.parametrize(
    "arg",
    ["ab-55ba9adb", "x-69ad", "fno-abcd", "f-1234", "abcdefgh-12345678"],
)
def test_classifies_fno(arg):
    assert _kind(arg) == "fno"


@pytest.mark.parametrize("arg", ["ENG-441", "PROJ-88", "owner/repo#123"])
def test_classifies_external(arg):
    assert _kind(arg) == "external"


@pytest.mark.parametrize("arg", ["fix the login bug", "path/to/plan.md", ""])
def test_classifies_none(arg):
    assert _kind(arg) == "none"


@pytest.mark.parametrize(
    "arg",
    [
        "ab-55ba9adb", "x-69ad", "fno-abcd", "f-1234", "abcdefgh-12345678",
        "ENG-441", "owner/repo#123", "fix the login", "plan.md", "ab-12", "not-an-id",
    ],
)
def test_shell_matches_python_authority(arg):
    # The shell "fno" verdict must equal is_wellformed_node_id. If these ever
    # disagree, a node id accepted by one layer is rejected by the other.
    assert (_kind(arg) == "fno") == is_wellformed_node_id(arg)
