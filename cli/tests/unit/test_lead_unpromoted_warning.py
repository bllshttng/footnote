"""`lead init`'s unpromoted-row warning, in all four of its outcomes.

The warning exists because three row-keyed readers (`lead done`,
`lead manifest-path`, `lead-postcompact-reinject.sh`) all fail CLOSED and
SILENTLY when the row carries no role for the armed scope. It never
refuses: the manifest is written and the loop arms on the FILE.

Review 2026-09-03 found the first cut gated on "does this row carry ANY
role", which `role_label` answers from `role_level` alone. A session
promoted over other territory therefore armed a second scope in silence,
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

    ``role_label`` is a real property on the registry model and derives from
    ``role_level`` alone, rendering ``"L2 ?"`` when the scope is unset. That
    asymmetry is what the reviewed defect turned on, so the double is faithful
    to it rather than carrying a plain label field.
    """

    name: str = "a5cdfd52"
    role_level: Optional[int] = None
    role_scope: Optional[str] = None
    role_grantor: Optional[str] = None

    @property
    def role_label(self) -> Optional[str]:
        if self.role_level is None:
            return None
        return f"L{self.role_level} {self.role_scope or '?'}"


def _warn(monkeypatch, capsys, row, scope="x-b76b"):
    from fno.lead import cli as lead_cli

    monkeypatch.setattr("fno.agents.role.calling_agent_row", lambda: row)
    lead_cli._warn_unpromoted_row(scope)
    return capsys.readouterr().err


def test_a_role_over_this_scope_is_silent(monkeypatch, capsys):
    row = _Row(role_level=2, role_scope="x-b76b")
    assert _warn(monkeypatch, capsys, row) == ""


def test_no_role_names_the_grant_command(monkeypatch, capsys):
    err = _warn(monkeypatch, capsys, _Row())
    assert "NO role" in err
    assert "fno agents org promote a5cdfd52 --scope x-b76b" in err
    # The consequence must be stated truthfully: manifest-path raises
    # typer.Exit(1) and prints nothing, it does not answer empty at exit 0.
    assert "exit non-zero without printing a path" in err


def test_a_role_over_other_territory_still_warns(monkeypatch, capsys):
    """The reviewed defect: this row IS promoted, just not for this scope."""
    row = _Row(role_level=2, role_scope="x-4be7")
    err = _warn(monkeypatch, capsys, row)
    assert "is not that scope" in err
    assert "L2 x-4be7" in err
    assert "--scope x-b76b" in err


def test_a_role_with_no_scope_warns_rather_than_passing(monkeypatch, capsys):
    """``role_label`` renders "L2 ?" here, so an any-role gate let it pass."""
    row = _Row(role_level=2, role_scope=None)
    err = _warn(monkeypatch, capsys, row)
    assert "is not that scope" in err
    assert "L2 ?" in err


def test_an_unresolvable_row_makes_a_different_claim(monkeypatch, capsys):
    """An unanswered question is not a finding, and must not read as one."""
    from fno.agents import role as role_mod

    err = _warn(monkeypatch, capsys, role_mod.AGENT_UNREGISTERED)
    assert "resolves to no registry row" in err
    assert "/fno-me" in err
    assert "NO role" not in err


def test_the_warning_never_raises_when_the_registry_is_unreadable(
    monkeypatch, capsys
):
    """A warning must never break a manifest that was already written."""
    from fno.lead import cli as lead_cli

    def _boom():
        raise RuntimeError("registry unreadable")

    monkeypatch.setattr("fno.agents.role.calling_agent_row", _boom)
    lead_cli._warn_unpromoted_row("x-b76b")
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
        # so a set role must not satisfy a single-member manifest.
        ("x-1,x-2", "x-2,x-1", True),
        ("x-1,x-2", "x-1", False),
        ("x-1", "x-1,x-2", False),
        ("x-1,x-2", "x-1,x-3", False),
    ],
)
def test_role_scope_matches_answers_equality_and_absence(held, requested, matches):
    """Territory equality, which is the question the row-keyed readers ask."""
    from fno.agents.role import role_scope_matches

    assert role_scope_matches(held, requested) is matches


def test_a_containing_role_does_not_match_a_narrower_manifest(monkeypatch, capsys):
    """The second defect this gate had, and the opposite of the first.

    A first fix accepted ``scope_contains``, reasoning that the grant path
    accepts a strict container. The readers do not: ``lead_manifest_path``
    builds ``leads/{role_scope}.md`` from the stored scope verbatim and
    ``done_cmd`` refuses on ``own != scope``. So a project-level role over
    an epic manifest must still warn, or the warning goes quiet on exactly
    the state it exists to expose.
    """
    from fno.agents.role import role_scope_matches

    assert role_scope_matches("fno", "x-b76b") is False

    row = _Row(role_level=3, role_scope="fno")
    err = _warn(monkeypatch, capsys, row, scope="x-b76b")
    assert "is not that scope" in err
    assert "L3 fno" in err
    # The message must not DENY the containment, because it is real here.
    # Saying "neither equals nor contains it" to a role that does contain it
    # is runtime text drifting from behavior.
    assert "neither equals nor contains" not in err
    assert "EXACT role_scope" in err


def test_the_reader_agrees_with_the_gate_on_a_containing_role(monkeypatch, tmp_path):
    """The gate's claim, driven through the real reader rather than restated.

    Asserting the predicate alone would only prove the predicate, and the
    warning's whole justification is a claim about what a DIFFERENT function
    does. So this runs ``resolve_lead_manifest_path`` against a row whose
    role contains the armed scope, with the armed manifest really on disk,
    and asserts it still answers None. That None is what makes
    ``manifest-path`` exit 1 and the warning correct.
    """
    from fno.lead import state as lead_state

    armed = lead_state.lead_manifest_path("x-b76b", state_root=tmp_path)
    armed.parent.mkdir(parents=True, exist_ok=True)
    armed.write_text("# armed manifest\n", encoding="utf-8")

    # resolve_lead_manifest_path takes the registry as a parameter and looks
    # the row up through fno.agents.whoami._find_by_session, so the seam is
    # there rather than on this module.
    def _resolve(row):
        monkeypatch.setattr(
            "fno.agents.whoami._find_by_session", lambda rows, *a, **k: rows[0]
        )
        return lead_state.resolve_lead_manifest_path(
            "some-session-id",
            "claude",
            state_root=tmp_path,
            registry=[row],
        )

    containing = _Row(role_level=3, role_scope="fno")
    containing.status = "live"  # type: ignore[attr-defined]
    containing_path, containing_reason = _resolve(containing)
    assert containing_path is None, (
        "a containing role resolved a manifest it does not name; the warning's "
        "premise would then be wrong"
    )
    # The reason names the file the containing scope looked for, so the miss
    # is diagnosable instead of the bare silence the reader used to return.
    assert "leads/fno.md" in containing_reason

    # Positive control: the SAME call with an exactly-matching role resolves
    # the manifest, so the None above is the containment and not a dead stub.
    exact = _Row(role_level=3, role_scope="x-b76b")
    exact.status = "live"  # type: ignore[attr-defined]
    exact_path, exact_reason = _resolve(exact)
    assert exact_path == armed
    assert exact_reason == ""
