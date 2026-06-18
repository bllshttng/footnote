"""Runtime dispatch proofs for the Phase 3 two-spelling canonicalization
(ab-a04f3f1a, US3).

``test_short_flag_convention.py`` proves the declaration shape (canonical
long visible, legacy long hidden) via static AST scan. These tests drive the
REAL root app so each touched sub-app imports, registers, and parses - and
prove AC5 end-to-end on one representative command: the deprecated spelling
still works, warns on stderr, and resolves to the same value as the
canonical spelling.
"""
from __future__ import annotations

import re

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()

_ANSI = re.compile(r"\x1b\[[0-9;]*m")


def _clean(text: str) -> str:
    """Strip ANSI styling so substring asserts see contiguous flag names."""
    return _ANSI.sub("", text)

# --------------------------------------------------------------------------- #
# Registration smoke: every surface touched by the canonicalization.
# --------------------------------------------------------------------------- #

PHASE3_HELP_SURFACES: dict[str, list[str]] = {
    # loop removed: the exit-12 `fno loop` stub was deleted in step-5
    # group 3 (ab-9fd662c6); the unified loop lives at `fno-agents loop run`.
    "review": ["review", "--help"],
    # gate-check removed: the `fno gate` sub-app was deleted by the
    # control-plane collapse wedge (ab-d0337fbc).
    "backlog-cost": ["backlog", "cost", "--help"],
    "retro-run": ["retro", "run", "--help"],
    "worker-review": ["worker", "review", "--help"],
    "worker-external": ["worker", "external", "--help"],
    "reality-check-gh": ["reality-check", "gh", "--help"],
    "done": ["done", "--help"],
}


@pytest.mark.parametrize(
    "argv",
    list(PHASE3_HELP_SURFACES.values()),
    ids=list(PHASE3_HELP_SURFACES.keys()),
)
def test_phase3_surface_registers(argv: list[str]) -> None:
    """Each touched sub-app imports and Click accepts its flag decls."""
    result = runner.invoke(app, argv)
    assert result.exit_code == 0, result.output


@pytest.mark.parametrize(
    "argv,canonical,legacy",
    [
        # gate-check removed with the fno gate sub-app (ab-d0337fbc).
        (["backlog", "cost", "--help"], "--session-id", "--session"),
        (["worker", "external", "--help"], "--pr-number", "--pr"),
        (["reality-check", "gh", "--help"], "--pr-number", "--pr"),
    ],
    ids=["backlog-cost", "worker-external", "reality-check-gh"],
)
def test_legacy_spelling_hidden_from_help(
    argv: list[str], canonical: str, legacy: str
) -> None:
    """US3: help shows the canonical spelling and hides the deprecated alias.

    The legacy spelling is a prefix of the canonical one, so a plain
    substring check would match the canonical form. Instead: every
    occurrence of the legacy spelling must be part of a canonical
    occurrence (equal counts means no standalone legacy flag in help).
    """
    result = runner.invoke(app, argv)
    assert result.exit_code == 0, result.output
    out = _clean(result.output)
    assert canonical in out, f"{canonical} missing from {argv}"
    assert out.count(legacy) == out.count(canonical), (
        f"deprecated {legacy} visible in {argv}"
    )


# --------------------------------------------------------------------------- #
# AC5 equivalence on a real command path: reality-check gh.
# --------------------------------------------------------------------------- #

@pytest.fixture
def fake_check_gh(monkeypatch):
    """Record check_gh calls without touching the network/gh binary."""
    calls: list[dict] = []

    def _fake(pr_number: int, expect: str, timeout: int) -> dict:
        calls.append({"pr_number": pr_number, "expect": expect, "timeout": timeout})
        return {"ok": True, "pr": pr_number, "state": expect}

    import fno.reality_check.gh as gh_mod

    monkeypatch.setattr(gh_mod, "check_gh", _fake)
    return calls


def test_pr_number_and_pr_resolve_to_same_value(fake_check_gh) -> None:
    """AC5: --pr (deprecated) behaves exactly like --pr-number."""
    canonical = runner.invoke(app, ["reality-check", "gh", "--pr-number", "7"])
    legacy = runner.invoke(app, ["reality-check", "gh", "--pr", "7"])
    assert canonical.exit_code == 0, canonical.output
    assert legacy.exit_code == 0, legacy.output
    assert fake_check_gh[0]["pr_number"] == fake_check_gh[1]["pr_number"] == 7
    assert canonical.stdout == legacy.stdout
    # Over-eager-warning guard: the canonical spelling must never warn.
    assert "deprecated" not in _clean(canonical.output)


def test_deprecated_spelling_warns_on_stderr(fake_check_gh) -> None:
    """The hidden alias still works but tells the user to migrate."""
    result = runner.invoke(app, ["reality-check", "gh", "--pr", "7"])
    assert result.exit_code == 0, result.output
    warning = _clean(result.output)
    assert "deprecated" in warning
    assert "--pr-number" in warning


def test_both_spellings_together_is_a_usage_error(fake_check_gh) -> None:
    """Passing --pr-number AND --pr is ambiguous: refuse, exit 2."""
    result = runner.invoke(
        app, ["reality-check", "gh", "--pr-number", "7", "--pr", "8"]
    )
    assert result.exit_code == 2, result.output
    assert not fake_check_gh, "check_gh must not run on a usage error"


def test_missing_required_canonical_is_a_usage_error(fake_check_gh) -> None:
    """A required option canonicalized to Optional+merge still demands a value."""
    result = runner.invoke(app, ["reality-check", "gh"])
    assert result.exit_code == 2, result.output
    assert "--pr-number" in _clean(result.output)
