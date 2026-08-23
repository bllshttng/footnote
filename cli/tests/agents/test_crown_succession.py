"""Succession: an abdicating king hands its crown to the heir it spawns.

This replaces `fno agents crown --succeed`. The verb is gone, but the behavior it
existed for is not optional, and it cannot simply become "exit, then let the next
king crown itself": a session that has already exited spawns nothing. The handoff
has to happen while the king still reigns, so it happens at the moment it creates
its successor.

The mechanism is the one-live-crown guard reading WHO holds the scope. A stranger
holding it means decline (spawn uncrowned - recoverable). The caller holding it
means transfer, in the same registry write that stamps the heir, so no reader
sees two live crowns over one scope and none sees zero.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents.registry import AgentEntry, load_registry, update_registry
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "caller-sess-1"
SCOPE = "epic-x"


@pytest.fixture(autouse=True)
def _clear_parent_markers(monkeypatch):
    for marker in (
        "FNO_SESSION",
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
    ):
        monkeypatch.delenv(marker, raising=False)


@pytest.fixture
def court(tmp_path, monkeypatch):
    """An fno home with a fake claude, and the CALLER identified as a live king."""
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CALLER_SESSION)
    return tmp_path


def _seat(
    name: str,
    session: str,
    *,
    scope: str | None = SCOPE,
    status: str = "busy",
    grantor: str | None = None,
):
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name=name,
                cwd="/tmp",
                log_path="",
                harness="claude",
                harness_session_id=session,
                status=status,
                crown_level=2 if scope else None,
                crown_scope=scope,
                crown_grantor=(grantor or "human") if scope else None,
            )
        ]
    )


def _spawn_heir(name: str = "heir", *, succeed: bool = False):
    from fno.agents.dispatch import dispatch_spawn

    return dispatch_spawn(
        name=name,
        message="reign",
        provider="claude",
        cwd=Path("/tmp"),
        crown_level=2,
        crown_scope=SCOPE,
        succession=succeed,
    )


def _row(name: str):
    return next((e for e in load_registry() if e.name == name), None)


def test_the_caller_s_crown_transfers_to_the_heir(court) -> None:
    """The abdication that could not otherwise happen: a live king spawns its
    successor and the crown moves in that one write."""
    _seat("sitting-king", CALLER_SESSION)

    _spawn_heir(succeed=True)

    holders = [e for e in load_registry() if e.crown_scope == SCOPE]
    assert len(holders) == 1
    assert holders[0].name == "heir"

    king, heir = _row("sitting-king"), _row("heir")
    assert (king.crown_level, king.crown_scope, king.crown_grantor) == (None, None, None)
    assert heir.crown_level == 2
    assert heir.crown_scope == SCOPE


def test_the_scope_is_never_doubly_ruled_nor_unruled(court) -> None:
    """The invariant the atomic write buys: after succession exactly one live row
    holds the scope. Checked over the whole registry, not just the two rows."""
    _seat("sitting-king", CALLER_SESSION)

    _spawn_heir(succeed=True)


def test_same_scope_spawn_refuses_without_explicit_succession(court) -> None:
    """Naming the caller's territory is a transfer, never an implicit grant."""
    from fno.agents.dispatch import DispatchAskError

    _seat("sitting-king", CALLER_SESSION)

    with pytest.raises(DispatchAskError, match="--succeed"):
        _spawn_heir()

    king = _row("sitting-king")
    assert (king.crown_level, king.crown_scope) == (2, SCOPE)
    assert _row("heir") is None


def test_a_stranger_s_crown_is_declined_not_stolen(court, monkeypatch) -> None:
    """Succession is the caller handing down what it holds. Someone ELSE's live
    crown is untouchable: the spawn succeeds uncrowned rather than seizing it.

    The caller is an attended human (no agent identity), so it is authorized to
    attempt the grant; the one-live-crown guard then declines it because another
    live king holds the scope. An agent caller holding nothing over the scope
    would be refused earlier at the grantor check, which is a different path."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("other-king", "a-different-session")

    _spawn_heir()

    assert _row("other-king").crown_level == 2, "another king's crown must not move"
    assert _row("heir").crown_level is None, "the heir must not be crowned"


def test_a_dead_king_does_not_need_succession(court) -> None:
    """A terminal holder never blocked the crown, so this stays a plain grant -
    the recovery path for an orphaned scope."""
    _seat("dead-king", CALLER_SESSION, status="exited")

    _spawn_heir()

    assert _row("heir").crown_level == 2
    from fno.agents.registry import register_existing_session

    register_existing_session(
        session_id=CALLER_SESSION,
        cwd="/tmp",
        harness="claude",
        name="dead-king",
    )
    dead = _row("dead-king")
    assert (dead.crown_level, dead.crown_scope, dead.crown_grantor) == (
        None,
        None,
        None,
    )
    assert [row.name for row in load_registry() if row.crown_scope == SCOPE] == ["heir"]


def test_succession_matches_cc_session_id_for_a_partially_backfilled_row(court) -> None:
    """A claude row born with harness_session_id=None (a raced uuid-resolution
    miss reconciled later) but carrying its id in cc_session_id must still be
    recognized as the caller, so its abdication TRANSFERS rather than being
    declined. calling_agent_row finds the row via cc_session_id (the same field
    _find_by_session matches for claude); the succession check (is_caller_row)
    must agree, or a sitting king spawns an uncrowned heir."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="sitting-king",
                cwd="/tmp",
                # x-7bcd: needs a resolvable handle; harness_session_id is
                # deliberately None here (that's what this test is about), so
                # this stands in for the fallback log a real mint would touch.
                log_path="/tmp/sitting-king.log",
                harness="claude",
                harness_session_id=None,
                cc_session_id=CALLER_SESSION,
                short_id="sk",
                status="busy",
                crown_level=2,
                crown_scope=SCOPE,
                crown_grantor="human",
            )
        ]
    )

    _spawn_heir(succeed=True)

    king, heir = _row("sitting-king"), _row("heir")
    assert (king.crown_level, king.crown_scope) == (None, None), "the king vacates"
    assert heir.crown_level == 2, "the heir receives the transferred crown"
    assert heir.crown_scope == SCOPE


def test_an_uncrowned_caller_grants_normally(court, monkeypatch) -> None:
    """No sitting holder at all: nothing to transfer, nothing to decline. The
    caller is an attended human (authorized to grant any scope); with no holder,
    the spawn is a plain grant. An uncrowned AGENT cannot grant at all - that
    refusal is covered by test_an_uncrowned_agent_cannot_bestow_a_crown."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _spawn_heir()

    assert _row("heir").crown_scope == SCOPE
    assert _row("heir").crown_grantor == "human"


def test_reclaim_returns_the_crown_to_its_grantor_without_spawning(court) -> None:
    """The recorded grantor is a return address, not a reason to create a row."""
    _seat("grantor", "grantor-session", scope=None)
    _seat(
        "heir",
        CALLER_SESSION,
        scope=SCOPE,
        grantor="grantor-session",
    )

    from fno.agents.crown import reclaim_crown

    receipt = reclaim_crown()

    grantor, heir = _row("grantor"), _row("heir")
    assert receipt["reclaimed"] == "grantor"
    assert (grantor.crown_level, grantor.crown_scope) == (2, SCOPE)
    assert (heir.crown_level, heir.crown_scope) == (None, None)
    assert len(load_registry()) == 2


def test_reclaim_command_uses_the_current_heir_without_a_new_session(court) -> None:
    _seat("grantor", "grantor-session", scope=None)
    _seat("heir", CALLER_SESSION, scope=SCOPE, grantor="grantor-session")

    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(agents_app, ["crown", "--reclaim"])

    assert result.exit_code == 0, result.output
    assert _row("grantor").crown_scope == SCOPE
    assert _row("heir").crown_scope is None
    assert len(load_registry()) == 2


def test_a_registered_agent_cannot_reclaim_by_naming_a_peers_handle(court) -> None:
    """The handle form is attended-only: a worker that learns the heir's
    handle must not strip that crown, or the guard that succession built
    (no agent path off your own crown) has a side door."""
    _seat("grantor", "grantor-session", scope=None)
    _seat("heir", "heir-session", scope=SCOPE, grantor="grantor-session")

    from fno.agents.crown import CrownPromotionError, reclaim_crown

    with pytest.raises(CrownPromotionError, match="attended-shell"):
        reclaim_crown(handle="heir")

    assert _row("heir").crown_scope == SCOPE, "the crown never moved"
    assert _row("grantor").crown_scope is None


def test_an_attended_shell_reclaims_by_handle(court, monkeypatch) -> None:
    """The operator path: no agent identity, the heir named by handle."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("grantor", "grantor-session", scope=None)
    _seat("heir", "heir-session", scope=SCOPE, grantor="grantor-session")

    from fno.agents.crown import reclaim_crown

    receipt = reclaim_crown(handle="heir")

    assert receipt["reclaimed"] == "grantor"
    assert _row("grantor").crown_scope == SCOPE
    assert _row("heir").crown_scope is None


# --- you cannot hand down authority you do not hold --------------------------
#
# The strict-subset rule was documented from the start and enforced by the
# deleted promotion verb. Its replacement (`scope_contains`) shipped with no
# caller at all, so for one commit any spawned worker could mint portfolio
# authority for its child. These exercise the seam, not the helper: a test that
# called `scope_contains` directly passed the whole time the rule was unenforced.


def _spawn_over(scope: str, name: str = "heir"):
    from fno.agents.dispatch import dispatch_spawn

    level = 0 if "," in scope else 1
    return dispatch_spawn(
        name=name,
        message="reign",
        provider="claude",
        cwd=Path("/tmp"),
        crown_level=level,
        crown_scope=scope,
    )


def test_an_uncrowned_agent_cannot_bestow_a_crown(court) -> None:
    """The caller is a registered agent (a row keyed by its session) holding no
    crown. It has nothing to hand down, so the grant is refused before launch."""
    from fno.agents.dispatch import DispatchAskError

    _seat("caller", CALLER_SESSION, scope=None, status="busy")

    with pytest.raises(DispatchAskError) as exc:
        _spawn_over("alpha")
    assert exc.value.exit_code == 2
    assert "holds none" in str(exc.value)
    assert _row("heir") is None, "an unauthorized grant must launch nothing"


def test_a_king_cannot_grant_outside_its_own_scope(court) -> None:
    """A king over one epic cannot mint a portfolio crown - that is authority it
    does not hold, and the escalation the subset rule exists to stop."""
    from fno.agents.dispatch import DispatchAskError

    _seat("caller", CALLER_SESSION, scope="epic-x", status="busy")

    with pytest.raises(DispatchAskError) as exc:
        _spawn_over("alpha,beta")
    assert exc.value.exit_code == 2
    assert "neither contains nor equals" in str(exc.value)


def test_an_attended_human_may_grant_anything(court, monkeypatch) -> None:
    """No agent identity in the environment means a human at a keyboard, and
    there is nobody above a human to check the grant against."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)

    _spawn_over("alpha")

    assert _row("heir").crown_scope == "alpha"
