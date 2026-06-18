"""Coverage test for fno.handoff.exit_codes.ExitCode.

Enforces that every member of the ExitCode enum is either referenced by
the codebase OR explicitly tagged as PLANNED for a future phase. Prevents
silent bit-rot where a code is defined but no subcommand ever raises it.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from fno.handoff.exit_codes import ExitCode

CLI_SRC = Path(__file__).resolve().parents[1] / "src" / "fno"

PLANNED: dict[ExitCode, str] = {
    ExitCode.DISPATCH_REQUIRED: "phase 02b handoff dispatch",
    ExitCode.RESOURCE_LOCKED: "phase 03 review orchestrator lock",
    ExitCode.BLOCKING_FINDINGS: "phase 03 review verdict",
    ExitCode.SIGINT: "phase 03 review signal handler",
}


def _iter_py_sources() -> list[Path]:
    return sorted(p for p in CLI_SRC.rglob("*.py") if "__pycache__" not in p.parts)


def _references_member(member: ExitCode, sources: list[Path]) -> bool:
    """Return True if `member` is referenced anywhere under cli/src.

    A reference counts as either:
      * a symbolic mention (``ExitCode.NAME``), OR
      * a literal numeric exit of the same value via ``sys.exit(N)``,
        ``typer.Exit(code=N)``, or ``raise typer.Exit(N)``.

    The symbolic form is preferred; the literal form keeps this test
    satisfiable during the migration window where call sites still use
    plain integers.
    """
    symbolic = rf"ExitCode\.{member.name}\b"
    literal_patterns = [
        rf"sys\.exit\(\s*{member.value}\s*\)",
        rf"typer\.Exit\(\s*(?:code\s*=\s*)?{member.value}\s*[\),]",
        rf"raise\s+typer\.Exit\(\s*{member.value}\s*\)",
    ]
    combined = re.compile("|".join([symbolic, *literal_patterns]))
    for src in sources:
        if src.is_relative_to(CLI_SRC / "handoff"):
            continue
        if combined.search(src.read_text(encoding="utf-8", errors="replace")):
            return True
    return False


@pytest.fixture(scope="module")
def sources() -> list[Path]:
    return _iter_py_sources()


def test_success_is_referenced(sources: list[Path]) -> None:
    """AC1-HP: SUCCESS (0) must be referenced by at least one module."""
    assert _references_member(ExitCode.SUCCESS, sources), (
        "ExitCode.SUCCESS (0) not referenced by any module. "
        "At least one subcommand must exit 0 on the happy path."
    )


def test_error_is_referenced(sources: list[Path]) -> None:
    """AC1-HP: ERROR (2) must be referenced by at least one module."""
    assert _references_member(ExitCode.ERROR, sources), (
        "ExitCode.ERROR (2) not referenced by any module. "
        "At least one subcommand must exit 2 on hard error."
    )


def test_every_member_used_or_planned(sources: list[Path]) -> None:
    """AC1-ERR: no ExitCode member may be defined yet unused and untagged.

    If a code is defined in ExitCode but no module raises it AND it isn't
    listed in PLANNED, this test fails - someone added a code without a
    caller or a migration plan.
    """
    orphans: list[str] = []
    for member in ExitCode:
        if _references_member(member, sources):
            continue
        if member in PLANNED:
            continue
        orphans.append(member.name)
    assert not orphans, (
        f"ExitCode members defined but neither referenced nor PLANNED: {orphans}. "
        "Either raise this code somewhere or add an entry to PLANNED with the "
        "phase that will wire it up."
    )
