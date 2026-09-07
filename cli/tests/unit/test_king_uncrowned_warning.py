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
    assert "is not that scope" in err
    assert "L2 x-4be7" in err
    assert "--scope x-b76b" in err


def test_a_crown_with_no_scope_warns_rather_than_passing(monkeypatch, capsys):
    """``crown_label`` renders "L2 ?" here, so an any-crown gate let it pass."""
    row = _Row(crown_level=2, crown_scope=None)
    err = _warn(monkeypatch, capsys, row)
    assert "is not that scope" in err
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
    "held,requested,matches",
    [
        ("x-b76b", "x-b76b", True),
        ("x-4be7", "x-b76b", False),
        (None, "x-b76b", False),
        ("", "x-b76b", False),
        ("x-b76b", "", False),
        # A rung-2 set: same set spelled in any order is one territory, but a
        # MEMBER is not the whole scope - the readers key on the stored scope,
        # so a set crown must not satisfy a single-member manifest.
        ("x-1,x-2", "x-2,x-1", True),
        ("x-1,x-2", "x-1", False),
        ("x-1", "x-1,x-2", False),
        ("x-1,x-2", "x-1,x-3", False),
    ],
)
def test_crown_scope_matches_answers_equality_and_absence(held, requested, matches):
    """Territory equality, which is the question the row-keyed readers ask."""
    from fno.agents.crown import crown_scope_matches

    assert crown_scope_matches(held, requested) is matches


def test_a_containing_crown_does_not_match_a_narrower_manifest(monkeypatch, capsys):
    """The second defect this gate had, and the opposite of the first.

    A first fix accepted ``scope_contains``, reasoning that the grant path
    accepts a strict container. The readers do not: ``king_manifest_path``
    builds ``kings/{crown_scope}.md`` from the stored scope verbatim and
    ``done_cmd`` refuses on ``own != scope``. So a project-level crown over
    an epic manifest must still warn, or the warning goes quiet on exactly
    the state it exists to expose.
    """
    from fno.agents.crown import crown_scope_matches

    assert crown_scope_matches("fno", "x-b76b") is False

    row = _Row(crown_level=3, crown_scope="fno")
    err = _warn(monkeypatch, capsys, row, scope="x-b76b")
    assert "is not that scope" in err
    assert "L3 fno" in err
    # The message must not DENY the containment, because it is real here.
    # Saying "neither equals nor contains it" to a crown that does contain it
    # is runtime text drifting from behavior.
    assert "neither equals nor contains" not in err
    assert "EXACT crown_scope" in err


def test_the_reader_agrees_with_the_gate_on_a_containing_crown(monkeypatch, tmp_path):
    """The gate's claim, driven through the real reader rather than restated.

    Asserting the predicate alone would only prove the predicate, and the
    warning's whole justification is a claim about what a DIFFERENT function
    does. So this runs ``resolve_king_manifest_path`` against a row whose
    crown contains the armed scope, with the armed manifest really on disk,
    and asserts it still answers None. That None is what makes
    ``manifest-path`` exit 1 and the warning correct.
    """
    from fno.king import state as king_state

    armed = king_state.king_manifest_path("x-b76b", state_root=tmp_path)
    armed.parent.mkdir(parents=True, exist_ok=True)
    armed.write_text("# armed manifest\n", encoding="utf-8")

    # resolve_king_manifest_path takes the registry as a parameter and looks
    # the row up through fno.agents.whoami._find_by_session, so the seam is
    # there rather than on this module.
    def _resolve(row):
        monkeypatch.setattr(
            "fno.agents.whoami._find_by_session", lambda rows, *a, **k: rows[0]
        )
        return king_state.resolve_king_manifest_path(
            "some-session-id",
            "claude",
            state_root=tmp_path,
            registry=[row],
        )

    containing = _Row(crown_level=3, crown_scope="fno")
    containing.status = "live"  # type: ignore[attr-defined]
    assert _resolve(containing) is None, (
        "a containing crown resolved a manifest it does not name; the warning's "
        "premise would then be wrong"
    )

    # Positive control: the SAME call with an exactly-matching crown resolves
    # the manifest, so the None above is the containment and not a dead stub.
    exact = _Row(crown_level=3, crown_scope="x-b76b")
    exact.status = "live"  # type: ignore[attr-defined]
    assert _resolve(exact) == armed
