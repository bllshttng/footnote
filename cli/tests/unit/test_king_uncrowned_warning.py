"""`king init`'s uncrowned-row warning, in all four of its outcomes.

The warning exists because three row-keyed readers (`king done`,
`king manifest-path`, `king-postcompact-reinject.sh`) all fail CLOSED and
SILENTLY when the row carries no crown for the armed scope. It never
refuses: the manifest is written and the loop arms on the FILE.

Review 2026-09-03 found the first cut gated on "does this row carry ANY
crown", which `crown_label` answers from `crown_level` alone. A session
crowned over other territory therefore armed a second scope in silence,
which is the exact state the warning was added to make loud. Each branch
asserts the MESSAGE it produces, never merely that something was printed.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

import pytest


@dataclass
class _Row:
    """Enough of an ``AgentEntry`` for the warning, including its property.

    ``crown_label`` is a real property on the registry model and derives from
    ``crown_level`` alone, rendering ``"L2 ?"`` when the scope is unset. That
    asymmetry is what the reviewed defect turned on, so the double is faithful
    to it rather than carrying a plain label field.
    """

    name: str = "a5cdfd52"
    crown_level: Optional[int] = None
    crown_scope: Optional[str] = None
    crown_grantor: Optional[str] = None

    @property
    def crown_label(self) -> Optional[str]:
        if self.crown_level is None:
            return None
        return f"L{self.crown_level} {self.crown_scope or '?'}"


def _warn(monkeypatch, capsys, row, scope="x-b76b"):
    from fno.king import cli as king_cli

    monkeypatch.setattr("fno.agents.crown.calling_agent_row", lambda: row)
    king_cli._warn_uncrowned_row(scope)
    return capsys.readouterr().err


def test_a_crown_over_this_scope_is_silent(monkeypatch, capsys):
    row = _Row(crown_level=2, crown_scope="x-b76b")
    assert _warn(monkeypatch, capsys, row) == ""


def test_no_crown_names_the_grant_command(monkeypatch, capsys):
    err = _warn(monkeypatch, capsys, _Row())
    assert "NO crown" in err
    assert "fno agents crown a5cdfd52 --scope x-b76b" in err
    # The consequence must be stated truthfully: manifest-path raises
    # typer.Exit(1) and prints nothing, it does not answer empty at exit 0.
    assert "exit non-zero without printing a path" in err


def test_a_crown_over_other_territory_still_warns(monkeypatch, capsys):
    """The reviewed defect: this row IS crowned, just not for this scope."""
    row = _Row(crown_level=2, crown_scope="x-4be7")
    err = _warn(monkeypatch, capsys, row)
    assert "neither equals nor contains it" in err
    assert "L2 x-4be7" in err
    assert "--scope x-b76b" in err


def test_a_crown_with_no_scope_warns_rather_than_passing(monkeypatch, capsys):
    """``crown_label`` renders "L2 ?" here, so an any-crown gate let it pass."""
    row = _Row(crown_level=2, crown_scope=None)
    err = _warn(monkeypatch, capsys, row)
    assert "neither equals nor contains it" in err
    assert "L2 ?" in err


def test_an_unresolvable_row_makes_a_different_claim(monkeypatch, capsys):
    """An unanswered question is not a finding, and must not read as one."""
    from fno.agents import crown as crown_mod

    err = _warn(monkeypatch, capsys, crown_mod.AGENT_UNREGISTERED)
    assert "resolves to no registry row" in err
    assert "/fno-me" in err
    assert "NO crown" not in err


def test_the_warning_never_raises_when_the_registry_is_unreadable(
    monkeypatch, capsys
):
    """A warning must never break a manifest that was already written."""
    from fno.king import cli as king_cli

    def _boom():
        raise RuntimeError("registry unreadable")

    monkeypatch.setattr("fno.agents.crown.calling_agent_row", _boom)
    king_cli._warn_uncrowned_row("x-b76b")
    assert capsys.readouterr().err == ""


@pytest.mark.parametrize(
    "held,requested,covers",
    [
        ("x-b76b", "x-b76b", True),
        ("x-4be7", "x-b76b", False),
        (None, "x-b76b", False),
        ("", "x-b76b", False),
        ("x-b76b", "", False),
    ],
)
def test_crown_covers_answers_equality_and_absence(held, requested, covers):
    """The shared rule the warning asks. Containment has its own suite."""
    from fno.agents.crown import crown_covers

    assert crown_covers(held, requested) is covers
