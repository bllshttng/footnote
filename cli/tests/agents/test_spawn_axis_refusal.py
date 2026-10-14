"""AC4-HP: a retired spawn axis refuses, naming the native form.

The twelve axis flags stay DECLARED as hidden refusal stubs so the door
never silently reopens; each callback names the lane template and points
at docs/HARNESSES.md.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.agents.cli import agents_app

RETIRED_VALUES = [
    ("--model", "opus"),
    ("--harness", "codex"),
    ("--provider", "zai"),
    ("--substrate", "thread"),
    ("--yolo", None),
    ("--permission-mode", "default"),
    ("--effort", "high"),
    ("--resume", "12345678-1234-1234-1234-123456789012"),
    ("--add-dir", "/w"),
    ("--agent", "reviewer"),
    ("--tools", "Read"),
    ("--deny-tools", "Bash"),
]


@pytest.mark.parametrize(
    "flag,value", RETIRED_VALUES, ids=[f[0] for f in RETIRED_VALUES]
)
def test_a_retired_spawn_axis_refuses_naming_the_native_form(
    flag: str, value: str | None
) -> None:
    argv = ["spawn", "x", "--name", "w", flag, *([value] if value else [])]
    result = CliRunner().invoke(agents_app, argv)
    assert result.exit_code == 2, result.output
    assert "retired" in result.output, result.output
    assert "HARNESSES" in result.output, result.output
