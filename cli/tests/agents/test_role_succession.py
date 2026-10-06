"""Succession: an stepping_down lead hands its role to the successor it spawns.

This replaces `fno agents role --succeed`. The verb is gone, but the behavior it
existed for is not optional, and it cannot simply become "exit, then let the next
lead role itself": a session that has already exited spawns nothing. The handoff
has to happen while the lead still terms, so it happens at the moment it creates
its successor.

The mechanism is the one-live-role guard reading WHO holds the scope. A stranger
holding it means decline (spawn unpromoted - recoverable). The caller holding it
means transfer, in the same registry write that stamps the successor, so no reader
sees two live roles over one scope and none sees zero.
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
def team(tmp_path, monkeypatch, native_backlog_door):
    """An fno home with a fake claude, and the CALLER identified as a live lead.
    Pins the role-settle occupancy call to this checkout's dev build (role
    occupancy runs through Rust now)."""
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CALLER_SESSION)
    # The store gate checks that a level-0 stamp names members that RESOLVE as
    # projects, so the fixture declares the two the refusal tests spawn over.
    from fno.projects import resolve as proj_resolve

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\nprojects = [{ name = "alpha" }, { name = "beta" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()
    yield tmp_path
    proj_resolve._clear_cache()


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
                role_level=2 if scope else None,
                role_scope=scope,
                role_grantor=(grantor or "human") if scope else None,
            )
        ]
    )


def _spawn_successor(name: str = "successor", *, succeed: bool = False):
    from fno.agents.dispatch import dispatch_spawn

    return dispatch_spawn(
        name=name,
        message="term",
        harness="claude",
        cwd=Path("/tmp"),
        role_level=2,
        role_scope=SCOPE,
        succession=succeed,
    )


def _row(name: str):
    return next((e for e in load_registry() if e.name == name), None)


def test_the_caller_s_role_transfers_to_the_successor(team) -> None:
    """The departure that could not otherwise happen: a live lead spawns its
    successor and the role moves in that one write."""
    _seat("sitting-lead", CALLER_SESSION)

    _spawn_successor(succeed=True)

    holders = [e for e in load_registry() if e.role_scope == SCOPE]
    assert len(holders) == 1
    assert holders[0].name == "successor"

    lead, successor = _row("sitting-lead"), _row("successor")
    assert (lead.role_level, lead.role_scope, lead.role_grantor) == (None, None, None)
    assert successor.role_level == 2
    assert successor.role_scope == SCOPE


def test_the_scope_is_never_doubly_ruled_nor_unruled(team) -> None:
    """The invariant the atomic write buys: after succession exactly one live row
    holds the scope. Checked over the whole registry, not just the two rows."""
    _seat("sitting-lead", CALLER_SESSION)

    _spawn_successor(succeed=True)


def test_same_scope_spawn_refuses_without_explicit_succession(team) -> None:
    """Naming the caller's territory is a transfer, never an implicit grant."""
    from fno.agents.dispatch import DispatchAskError

    _seat("sitting-lead", CALLER_SESSION)

    with pytest.raises(DispatchAskError, match="--hand-off"):
        _spawn_successor()

    lead = _row("sitting-lead")
    assert (lead.role_level, lead.role_scope) == (2, SCOPE)
    assert _row("successor") is None


def test_a_shell_spawn_over_a_held_scope_refuses_before_launch(team, monkeypatch) -> None:
    """A shell (no agent identity) is authorized to grant any scope, but a live
    holder still means the successor would launch with no role - so without
    --succeed the spawn refuses before anything is created, rather than
    launching unpromoted and stranding the successor at its role check."""
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("other-lead", "a-different-session")

    with pytest.raises(DispatchAskError, match="--hand-off"):
        _spawn_successor()

    assert _row("other-lead").role_level == 2, "another lead's role must not move"
    assert _row("successor") is None, "a refused role must launch nothing"


def test_a_dead_lead_does_not_need_succession(team) -> None:
    """A terminal holder never blocked the role, so this stays a plain grant -
    the recovery path for an orphaned scope."""
    _seat("dead-lead", CALLER_SESSION, status="exited")

    _spawn_successor()

    assert _row("successor").role_level == 2
    from fno.agents.registry import register_existing_session

    register_existing_session(
        session_id=CALLER_SESSION,
        cwd="/tmp",
        harness="claude",
        name="dead-lead",
    )
    dead = _row("dead-lead")
    assert (dead.role_level, dead.role_scope, dead.role_grantor) == (
        None,
        None,
        None,
    )
    assert [row.name for row in load_registry() if row.role_scope == SCOPE] == ["successor"]


def test_succession_matches_cc_session_id_for_a_partially_backfilled_row(team) -> None:
    """A claude row born with harness_session_id=None (a raced uuid-resolution
    miss reconciled later) but carrying its id in cc_session_id must still be
    recognized as the caller, so its departure TRANSFERS rather than being
    declined. calling_agent_row finds the row via cc_session_id (the same field
    _find_by_session matches for claude); the succession check (is_caller_row)
    must agree, or a sitting lead spawns an unpromoted successor."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="sitting-lead",
                cwd="/tmp",
                # x-7bcd: needs a resolvable handle; harness_session_id is
                # deliberately None here (that's what this test is about), so
                # this stands in for the fallback log a real mint would touch.
                log_path="/tmp/sitting-lead.log",
                harness="claude",
                harness_session_id=None,
                cc_session_id=CALLER_SESSION,
                short_id="sk",
                status="busy",
                role_level=2,
                role_scope=SCOPE,
                role_grantor="human",
            )
        ]
    )

    _spawn_successor(succeed=True)

    lead, successor = _row("sitting-lead"), _row("successor")
    assert (lead.role_level, lead.role_scope) == (None, None), "the lead vacates"
    assert successor.role_level == 2, "the successor receives the transferred role"
    assert successor.role_scope == SCOPE


def test_an_unpromoted_caller_grants_normally(team, monkeypatch) -> None:
    """No sitting holder at all: nothing to transfer, nothing to decline. The
    caller is an attended human (authorized to grant any scope); with no holder,
    the spawn is a plain grant. An unpromoted AGENT cannot grant at all - that
    refusal is covered by test_an_unpromoted_agent_cannot_grant_a_role."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _spawn_successor()

    assert _row("successor").role_scope == SCOPE
    assert _row("successor").role_grantor == "human"


def test_reclaim_returns_the_role_to_its_grantor_without_spawning(team) -> None:
    """The recorded grantor is a return address, not a reason to create a row."""
    _seat("grantor", "grantor-session", scope=None)
    _seat(
        "successor",
        CALLER_SESSION,
        scope=SCOPE,
        grantor="grantor-session",
    )

    from fno.agents.role import reclaim_role

    receipt = reclaim_role()

    grantor, successor = _row("grantor"), _row("successor")
    assert receipt["reclaimed"] == "grantor"
    assert (grantor.role_level, grantor.role_scope) == (2, SCOPE)
    assert (successor.role_level, successor.role_scope) == (None, None)
    assert len(load_registry()) == 2


def test_reclaim_command_uses_the_current_successor_without_a_new_session(team) -> None:
    _seat("grantor", "grantor-session", scope=None)
    _seat("successor", CALLER_SESSION, scope=SCOPE, grantor="grantor-session")

    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(agents_app, ["role", "--reclaim"])

    assert result.exit_code == 0, result.output
    assert _row("grantor").role_scope == SCOPE
    assert _row("successor").role_scope is None
    assert len(load_registry()) == 2


def test_a_registered_agent_cannot_reclaim_by_naming_a_peers_handle(team) -> None:
    """The handle form is attended-only: a worker that learns the successor's
    handle must not strip that role, or the guard that succession built
    (no agent path off your own role) has a side door."""
    _seat("grantor", "grantor-session", scope=None)
    _seat("successor", "successor-session", scope=SCOPE, grantor="grantor-session")

    from fno.agents.role import RolePromotionError, reclaim_role

    with pytest.raises(RolePromotionError, match="attended-shell"):
        reclaim_role(handle="successor")

    assert _row("successor").role_scope == SCOPE, "the role never moved"
    assert _row("grantor").role_scope is None


def test_an_attended_shell_reclaims_by_handle(team, monkeypatch) -> None:
    """The operator path: no agent identity, the successor named by handle."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("grantor", "grantor-session", scope=None)
    _seat("successor", "successor-session", scope=SCOPE, grantor="grantor-session")

    from fno.agents.role import reclaim_role

    receipt = reclaim_role(handle="successor")

    assert receipt["reclaimed"] == "grantor"
    assert _row("grantor").role_scope == SCOPE
    assert _row("successor").role_scope is None


def test_a_reclaim_refuses_a_holder_name_rebound_inside_the_lock_window(
    team, monkeypatch
) -> None:
    """The holder resolved before the lock is matched by name AND
    session under the lock. A name rebound to a row promoted over the same
    scope passes the old scope-and-level check and would return the role
    to the grantor from a session that never held it."""
    from dataclasses import replace

    import fno.agents.registry as registry_mod
    from fno.agents.role import RolePromotionError, reclaim_role

    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("grantor", "grantor-session", scope=None)
    _seat("successor", "successor-session", scope=SCOPE, grantor="grantor-session")
    stale = _row("successor")
    # The rebind: same name, same role, a new session inside the window.
    update_registry(
        lambda rows: [replace(stale, harness_session_id="successor-session-2") if row.name == "successor" else row for row in rows]
    )

    class _StaleResolution:
        entry = stale

    monkeypatch.setattr(
        registry_mod, "resolve_agent", lambda handle, **kw: _StaleResolution()
    )

    with pytest.raises(RolePromotionError, match="no longer live"):
        reclaim_role(handle="successor")
    assert _row("successor").role_scope == SCOPE, "the rebound row keeps its role"
    assert _row("successor").harness_session_id == "successor-session-2"
    assert _row("grantor").role_scope is None, "the role never returned"


# --- you cannot hand down authority you do not hold --------------------------
#
# The strict-subset rule was documented from the start and enforced by the
# deleted promotion verb. Its replacement (`scope_contains`) shipped with no
# caller at all, so for one commit any spawned worker could mint portfolio
# authority for its child. These exercise the seam, not the helper: a test that
# called `scope_contains` directly passed the whole time the rule was unenforced.


def _spawn_over(scope: str, name: str = "successor"):
    from fno.agents.dispatch import dispatch_spawn

    level = 0 if "," in scope else 1
    return dispatch_spawn(
        name=name,
        message="term",
        harness="claude",
        cwd=Path("/tmp"),
        role_level=level,
        role_scope=scope,
    )


def test_an_unpromoted_agent_cannot_grant_a_role(team) -> None:
    """The caller is a registered agent (a row keyed by its session) holding no
    role. It has nothing to hand down, so the grant is refused before launch."""
    from fno.agents.dispatch import DispatchAskError

    _seat("caller", CALLER_SESSION, scope=None, status="busy")

    with pytest.raises(DispatchAskError) as exc:
        _spawn_over("alpha")
    assert exc.value.exit_code == 2
    assert "holds none" in str(exc.value)
    assert _row("successor") is None, "an unauthorized grant must launch nothing"


def test_a_lead_cannot_grant_outside_its_own_scope(team) -> None:
    """A lead over one epic cannot mint a portfolio role - that is authority it
    does not hold, and the escalation the subset rule exists to stop."""
    from fno.agents.dispatch import DispatchAskError

    _seat("caller", CALLER_SESSION, scope="epic-x", status="busy")

    with pytest.raises(DispatchAskError) as exc:
        _spawn_over("alpha,beta")
    assert exc.value.exit_code == 2
    assert "neither contains nor equals" in str(exc.value)


def test_an_attended_human_may_grant_anything(team, monkeypatch) -> None:
    """No agent identity in the environment means a human at a keyboard, and
    there is nobody above a human to check the grant against."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)

    _spawn_over("alpha")

    assert _row("successor").role_scope == "alpha"


def _events() -> list:
    """Every committed row of the tmp journal. The real events.emit commits
    through the store; a monkeypatched list would re-prove only the call."""
    from fno import paths

    from tests._event_rows import event_rows

    return event_rows(paths.state_dir() / "events.jsonl")


def test_a_succession_journals_the_vacate_and_the_grant(team) -> None:
    """The handoff lands in the journal: one vacate naming the sitting lead
    and its successor, one grant naming the successor, and the registry still shows
    exactly one live holder."""
    _seat("sitting-lead", CALLER_SESSION)

    _spawn_successor(succeed=True)

    vacates = [e for e in _events() if e["kind"] == "agent_role_vacated"]
    assert len(vacates) == 1
    assert vacates[0]["cause"] == "succession"
    assert vacates[0]["holder"] == "sitting-lead"
    assert vacates[0]["successor"] == "successor"
    assert vacates[0]["scope"] == SCOPE
    roles = [e for e in _events() if e["kind"] == "agent_promoted"]
    assert [c["name"] for c in roles] == ["successor"]
    assert roles[0]["scope"] == SCOPE
    holders = [e for e in load_registry() if e.role_scope == SCOPE]
    assert [h.name for h in holders] == ["successor"]


def test_a_refused_spawn_journals_no_role_event(team, monkeypatch) -> None:
    """A refusal launches nothing, so the journal stays silent."""
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("other-lead", "a-different-session")

    with pytest.raises(DispatchAskError, match="--hand-off"):
        _spawn_successor()

    assert _row("successor") is None
    role_events = [
        e
        for e in _events()
        if e["kind"] in ("agent_role_vacated", "agent_promoted")
    ]
    assert role_events == []


def test_a_shell_succession_transfers_a_live_lead_s_role(team, monkeypatch) -> None:
    """An attended shell spawning with --promote --succeed over a scope another
    live lead holds transfers the role rather than declining: a human may
    grant any scope, and --succeed names the transfer explicitly."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("other-lead", "a-different-session")

    _spawn_successor(succeed=True)

    successor = _row("successor")
    assert successor.role_level == 2
    assert successor.role_scope == SCOPE
    assert successor.role_grantor == "human"
    assert _row("other-lead").role_level is None, "the old lead is vacated"

    vacates = [e for e in _events() if e["kind"] == "agent_role_vacated"]
    assert len(vacates) == 1
    assert vacates[0]["holder"] == "other-lead"
    assert vacates[0]["cause"] == "succession"
    roles = [e for e in _events() if e["kind"] == "agent_promoted"]
    assert [c["name"] for c in roles] == ["successor"]


def test_a_race_holder_still_declines_in_the_write(team, monkeypatch, capsys) -> None:
    """The plan is a snapshot read before the lock: a holder that appears
    between the pre-launch check and the registry write still declines there -
    the one case two live roles over one scope cannot be undone from."""
    from fno.agents import dispatch as dispatch_mod

    refusal, plan = dispatch_mod.plan_spawn_role(SCOPE, None, False)
    assert refusal is None
    assert plan is not None
    _seat("other-lead", "a-different-session")
    monkeypatch.setattr(
        dispatch_mod,
        "plan_spawn_role",
        lambda *a, **k: (None, plan),
    )

    _spawn_successor()

    assert _row("successor").role_level is None
    assert _row("other-lead").role_level == 2, "the actual holder is untouched"
    assert "role declined" in capsys.readouterr().err


def test_a_name_rebound_since_the_plan_keeps_its_role(team, monkeypatch, capsys) -> None:
    """A reclaimed name with a new session is not the holder the plan saw."""
    from fno.agents import role, dispatch as dispatch_mod

    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("other-lead", "old-session")
    refusal, plan = role.plan_spawn_role(SCOPE, None, True)
    assert refusal is None
    assert plan is not None

    update_registry(lambda rows: [row for row in rows if row.name != "other-lead"])
    _seat("other-lead", "new-session")
    monkeypatch.setattr(dispatch_mod, "plan_spawn_role", lambda *a, **k: (None, plan))

    _spawn_successor(succeed=True)

    successor = _row("successor")
    holder = _row("other-lead")
    assert successor is not None and successor.role_level is None
    assert holder is not None
    assert (holder.role_level, holder.harness_session_id) == (2, "new-session")
    assert "role declined" in capsys.readouterr().err
    assert not any(
        event["kind"] == "agent_role_vacated" and event["holder"] == "other-lead"
        for event in _events()
    )
