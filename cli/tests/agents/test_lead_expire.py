"""The expire half of the role lifecycle: `fno agents lead done`.

A role that nothing expires makes every later lead pay `--force`, so the
departure path must clear both halves the role owns: the registry row (the
authority) and the scope manifest (the loop arm). The ordering is the
succession ordering - vacate under the registry lock first, then clean the
file - so a scope that moved to a successor mid-call is never disarmed.
"""
from __future__ import annotations

import json
from dataclasses import replace
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.registry import AgentEntry, load_registry, update_registry
from fno.lead.state import lead_manifest_path, lead_state_root, parse_manifest, write_manifest
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "0c1f2f9a-1111-4000-8000-000000000001"
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
    """An fno home with a fake claude, and the CALLER identified as a live
    lead. Pins the lock-time identity match to this checkout's dev build
    (the departure vacate runs through Rust's role-identity now)."""
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CALLER_SESSION)
    # The verb resolves the manifest state root from the caller's cwd; seat
    # the team so that resolution lands on the tmp state root too.
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _seat(
    name: str,
    session: str,
    *,
    scope: str | None = SCOPE,
    status: str = "busy",
    level: int | None = None,
    cwd: str = "/tmp",
):
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name=name,
                cwd=cwd,
                log_path="",
                harness="claude",
                harness_session_id=session,
                status=status,
                role_level=level if level is not None else (2 if scope else None),
                role_scope=scope,
                role_grantor="human" if scope else None,
            )
        ]
    )


def _manifest(team, scope: str = SCOPE, session: str = CALLER_SESSION):
    # The verb keys the manifest root on the caller row's cwd, so the fixture
    # writes through the same call the resolver makes.
    path = lead_manifest_path(scope, state_root=lead_state_root(Path(team)))
    write_manifest(path, scope=scope, harness_session_id=session)
    return path


def _row(name: str):
    return next((e for e in load_registry() if e.name == name), None)


def _done(*args: str):
    from fno.lead.cli import agents_lead_app

    return CliRunner().invoke(agents_lead_app, ["done", *args])


def _vacates() -> list:
    """Every committed row of the tmp journal. The real events.emit commits
    through the store; a monkeypatched list would re-prove only the call."""
    from fno import paths

    from tests._event_rows import event_rows

    return event_rows(paths.state_dir() / "events.jsonl")


def test_done_expires_the_manifest_and_arms_a_successor_without_force(team) -> None:
    """The departure contract: both role halves clear, and the next init
    over the same scope writes without --force."""
    _seat("sitting-lead", CALLER_SESSION, cwd=str(team))
    manifest = _manifest(team)

    result = _done()

    assert result.exit_code == 0, result.output
    lead = _row("sitting-lead")
    assert (lead.role_level, lead.role_scope, lead.role_grantor) == (None, None, None)
    assert not manifest.exists(), "the scope manifest must be cleared"
    write_manifest(manifest, scope=SCOPE, harness_session_id="successor-session")


def test_done_refuses_a_scope_the_caller_does_not_hold(team) -> None:
    _seat("sitting-lead", CALLER_SESSION, scope="epic-own")
    other = _manifest(team, scope="epic-other")

    result = _done("--scope", "epic-other")

    assert result.exit_code == 2, result.output
    assert "only its own role" in result.output
    assert other.exists(), "a refused expire must leave the manifest alone"
    assert _row("sitting-lead").role_scope == "epic-own"


def test_done_refuses_when_the_role_moved_before_the_write(team, monkeypatch) -> None:
    """The vacate closure re-reads the row under the registry lock, so a scope
    that already moved to a successor is refused rather than disarming the successor's
    manifest. Simulated by moving the role between the CLI's identity read
    and its registry write."""
    _seat("sitting-lead", CALLER_SESSION, cwd=str(team))
    manifest = _manifest(team)
    from fno.agents import registry as registry_mod

    real_update = registry_mod.update_registry

    def _move_then_update(fn):
        # The successor is promoted by a concurrent succession between the CLI's
        # caller read and its vacate write.
        real_update(
            lambda rows: [
                (
                    replace(row, role_scope=None, role_level=None, role_grantor=None)
                    if row.name == "sitting-lead"
                    else row
                )
                for row in rows
            ]
            + [
                AgentEntry(
                    name="successor",
                    cwd="/tmp",
                    log_path="",
                    harness="claude",
                    harness_session_id="successor-session",
                    status="busy",
                    role_level=2,
                    role_scope=SCOPE,
                    role_grantor="sitting-lead",
                )
            ]
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _move_then_update)

    result = _done()

    assert result.exit_code == 1, result.output
    assert "no longer holds" in result.output
    assert manifest.exists(), "the successor's manifest must survive the refusal"
    assert _row("successor").role_scope == SCOPE


def test_done_refuses_a_row_rebound_inside_the_lock_window(team, monkeypatch) -> None:
    """The vacate matches the caller's row by name AND session under
    the lock. A row re-registered under the same name with a new session (a
    successor promoted over the same scope) keeps its role; the old name-only
    match vacated the wrong session's role."""
    _seat("sitting-lead", CALLER_SESSION, cwd=str(team))
    from fno.agents import registry as registry_mod

    real_update = registry_mod.update_registry

    def _rebind_then_update(fn):
        # The row is dropped and re-registered under the same name with a new
        # session between the CLI's caller read and its vacate write.
        real_update(
            lambda rows: [
                (
                    replace(row, harness_session_id="successor-session")
                    if row.name == "sitting-lead"
                    else row
                )
                for row in rows
            ]
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _rebind_then_update)

    result = _done()

    assert result.exit_code == 1, result.output
    assert "no longer holds" in result.output
    row = _row("sitting-lead")
    assert (row.role_level, row.role_scope) == (2, SCOPE), (
        "the rebound row keeps its role"
    )
    assert row.harness_session_id == "successor-session"


def test_done_leaves_a_successor_manifest_promoted_in_the_vacate_window(
    team, monkeypatch
) -> None:
    """A successor init --force that lands between the row vacate and the
    manifest unlink must survive it: the unlink compares against the session
    id snapshotted before the vacate, so the file this expiry deletes can
    only ever be the one it decided to expire."""
    _seat("sitting-lead", CALLER_SESSION, cwd=str(team))
    manifest = _manifest(team)
    from fno.agents import registry as registry_mod

    real_update = registry_mod.update_registry

    def _role_successor_then_update(fn):
        write_manifest(
            manifest, scope=SCOPE, harness_session_id="successor-session", force=True
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _role_successor_then_update)

    result = _done()

    assert result.exit_code == 1, result.output
    assert "no longer names the session" in result.output
    assert manifest.exists(), "the successor's manifest must survive"
    assert parse_manifest(manifest)["harness_session_id"] == "successor-session"
    assert _row("sitting-lead").role_scope is None, "the row still vacated"


def test_done_from_a_foreign_cwd_clears_the_row_cwd_manifest(
    team, monkeypatch
) -> None:
    """The verb keys the manifest on the caller row's cwd: a lead whose shell
    sits outside the repo still clears ITS manifest. On the old code the file
    survived while the receipt still said cleared (x-8387)."""
    from fno.lead.state import lead_state_root as _ksr

    kingrepo = team / "kingrepo"
    kingrepo.mkdir()
    _seat("sitting-lead", CALLER_SESSION, cwd=str(kingrepo))
    manifest = lead_manifest_path(SCOPE, state_root=_ksr(kingrepo))
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)
    elsewhere = team / "elsewhere"
    elsewhere.mkdir()
    monkeypatch.chdir(elsewhere)

    result = _done()

    assert result.exit_code == 0, result.output
    assert "manifest: cleared" in result.output
    assert not manifest.exists(), "the row-cwd manifest must be cleared"
    assert _row("sitting-lead").role_scope is None


def test_an_attended_human_expires_a_named_scope(team, monkeypatch) -> None:
    """No agent identity means a human at the keyboard: any scope may expire,
    but it must be named - a human holds no role to default to."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    manifest = _manifest(team, scope="orphaned-scope", session="long-gone")

    result = _done()

    assert result.exit_code == 2
    assert "--scope" in result.output

    result = _done("--scope", "orphaned-scope")

    assert result.exit_code == 0, result.output
    assert not manifest.exists()


def test_an_attended_expiry_of_one_set_member_refuses(team, monkeypatch) -> None:
    """A member is not the role: vacating nothing while printing an expiry
    receipt is the false receipt this refusal closes."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("lead-set", "set-session", scope="x-119e,x-4d9b")
    manifest = _manifest(team, scope="x-119e,x-4d9b", session="set-session")

    result = _done("--scope", "x-4d9b")

    assert result.exit_code == 2
    assert "x-119e,x-4d9b" in result.output and "lead-set" in result.output
    assert _row("lead-set").role_scope == "x-119e,x-4d9b"
    assert manifest.exists()
    assert not [e for e in _vacates() if e.get("kind") == "agent_role_vacated"]


def test_an_attended_expiry_names_a_damaged_registry(team, monkeypatch) -> None:
    from fno.agents.registry import _registry_path

    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    path = _registry_path(None)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not json", encoding="utf-8")

    result = _done("--scope", "x-4d9b")

    assert result.exit_code == 1
    assert "role expire failed" in result.output


def test_an_attended_expiry_of_a_reordered_set_vacates_it(team, monkeypatch) -> None:
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _seat("lead-set", "set-session", scope="x-119e,x-4d9b")
    manifest = _manifest(team, scope="x-119e,x-4d9b", session="set-session")

    result = _done("--scope", "x-4d9b,x-119e")

    assert result.exit_code == 0, result.output
    assert _row("lead-set").role_scope is None
    assert not manifest.exists()


def test_an_agent_with_no_role_has_nothing_to_expire(team) -> None:
    _seat("plain-worker", CALLER_SESSION, scope=None)

    result = _done()

    assert result.exit_code == 2
    assert "no role" in result.output


def test_done_writes_one_vacate_event_naming_scope_and_holder(team) -> None:
    """The departure lands in the journal, not just the registry: one event
    naming the scope, the holder and its session, from the record alone."""
    _seat("sitting-lead", CALLER_SESSION, cwd=str(team))
    _manifest(team)

    result = _done()

    assert result.exit_code == 0, result.output
    vacates = [e for e in _vacates() if e["kind"] == "agent_role_vacated"]
    assert len(vacates) == 1
    event = vacates[0]
    assert event["scope"] == SCOPE
    assert event["holder"] == "sitting-lead"
    assert event["holder_session"] == CALLER_SESSION
    assert event["cause"] == "stepped_down"
    assert (event["level"], event["grantor"]) == (2, "human")


def test_a_refused_done_leaves_the_journal_silent(team, monkeypatch) -> None:
    """A successful departure first proves the reader finds the event; the
    refused expiry that follows (the role moved before the write) must gain
    no second one."""
    _seat("sitting-lead", CALLER_SESSION, cwd=str(team))
    _manifest(team)
    assert _done().exit_code == 0
    found = [e for e in _vacates() if e["kind"] == "agent_role_vacated"]
    assert len(found) == 1, "the reader must find the first departure"

    update_registry(
        lambda rows: [
            (
                replace(row, role_level=2, role_scope=SCOPE, role_grantor="human")
                if row.name == "sitting-lead"
                else row
            )
            for row in rows
        ]
    )
    from fno.agents import registry as registry_mod

    real_update = registry_mod.update_registry

    def _move_then_update(fn):
        real_update(
            lambda rows: [
                (
                    replace(
                        row, role_scope=None, role_level=None, role_grantor=None
                    )
                    if row.name == "sitting-lead"
                    else row
                )
                for row in rows
            ]
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _move_then_update)

    result = _done()

    assert result.exit_code == 1, result.output
    found = [e for e in _vacates() if e["kind"] == "agent_role_vacated"]
    assert len(found) == 1, "a refused vacate must write nothing"


def test_an_orphaned_scope_writes_the_manifest_clear_event(team, monkeypatch) -> None:
    """No live holder: the manifest clear is the whole vacate, so the event
    names the manifest's session, not a row."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    _manifest(team, scope="orphaned-scope", session="long-gone")

    result = _done("--scope", "orphaned-scope")

    assert result.exit_code == 0, result.output
    vacates = [e for e in _vacates() if e["kind"] == "agent_role_vacated"]
    assert len(vacates) == 1
    event = vacates[0]
    assert (event["cause"], event["scope"]) == ("orphan_manifest", "orphaned-scope")
    assert event["holder"] is None
    assert event["holder_session"] == "long-gone"


# --- Presiding expiry: a lead may expire a DEAD role one rung below it ----


def _stub_graph(monkeypatch, projects: dict[str, str]) -> None:
    """Feed find_presiding_role the epic -> project read the real graph
    gives it, at the seam the CLI reads (team's ``_graph_index``). Stubbing
    the tracker module two layers down made the stub read whatever state a
    sibling test left ``fno.tracker.metadata`` in, and the presiding path
    answered "graph unreadable" for every role in the room."""
    import fno.agents.team as team_mod

    monkeypatch.setattr(
        team_mod,
        "_graph_index",
        lambda: {
            epic: {"id": epic, "type": "epic", "project": proj}
            for epic, proj in projects.items()
        },
    )


def test_presiding_lead_expires_a_dead_role_in_its_territory(team, monkeypatch):
    _seat("l1-lead", CALLER_SESSION, scope="fno", level=1, cwd=str(team))
    _seat("dead-l2", "dead-session", scope="epic-dead", status="exited")
    manifest = _manifest(team, scope="epic-dead", session="dead-session")
    _stub_graph(monkeypatch, {"epic-dead": "fno"})

    result = _done("--scope", "epic-dead")

    assert result.exit_code == 0, result.output
    assert not manifest.exists(), "the dead role's manifest must be cleared"
    dead = _row("dead-l2")
    assert (dead.role_level, dead.role_scope) == (2, "epic-dead")
    assert "no live holder" in result.output
    vacates = [e for e in _vacates() if e.get("kind") == "agent_role_vacated"]
    assert [(v["cause"], v["scope"]) for v in vacates] == [
        ("orphan_manifest", "epic-dead")
    ]


def test_presiding_lead_expires_a_manifest_only_role(team, monkeypatch):
    _seat("l1-lead", CALLER_SESSION, scope="fno", level=1, cwd=str(team))
    manifest = _manifest(team, scope="epic-orphan", session="gone-session")
    _stub_graph(monkeypatch, {"epic-orphan": "fno"})

    result = _done("--scope", "epic-orphan")

    assert result.exit_code == 0, result.output
    assert not manifest.exists()


def test_presiding_refuses_a_live_role_in_its_territory(team, monkeypatch):
    _seat("l1-lead", CALLER_SESSION, scope="fno", level=1)
    _seat("live-l2", "live-session", scope="epic-live")
    manifest = _manifest(team, scope="epic-live", session="live-session")
    _stub_graph(monkeypatch, {"epic-live": "fno"})

    result = _done("--scope", "epic-live")

    assert result.exit_code == 2, result.output
    assert manifest.exists(), "a live role's manifest must survive"


def test_a_foreign_dead_role_still_refuses(team, monkeypatch):
    _seat("l1-lead", CALLER_SESSION, scope="fno", level=1)
    _seat("dead-other", "dead-session", scope="epic-out", status="exited")
    _stub_graph(monkeypatch, {"epic-out": "other-project"})

    result = _done("--scope", "epic-out")

    assert result.exit_code == 2, result.output
    assert "only its own role" in result.output


def test_one_member_of_a_dead_set_role_refuses(team, monkeypatch):
    _seat("l1-lead", CALLER_SESSION, scope="fno", level=1)
    _seat("dead-set", "dead-session", scope="epic-a,epic-b", status="exited")
    _stub_graph(monkeypatch, {"epic-a": "fno", "epic-b": "fno"})

    result = _done("--scope", "epic-a")

    assert result.exit_code == 2, result.output
    assert _row("dead-set").role_scope == "epic-a,epic-b"


def test_a_caller_without_a_level_cannot_preside(team, monkeypatch):
    _seat("half-role", CALLER_SESSION, scope="fno", level=None)
    _seat("dead-l2", "dead-session", scope="epic-dead", status="exited")
    _stub_graph(monkeypatch, {"epic-dead": "fno"})

    result = _done("--scope", "epic-dead")

    assert result.exit_code == 2, result.output


def test_presiding_refuses_a_successor_promoted_mid_call(team, monkeypatch):
    """The pre-check ran outside the registry lock; the vacate closure must
    stop a successor that promoted between the two instead of disarming it."""
    _seat("l1-lead", CALLER_SESSION, scope="fno", level=1, cwd=str(team))
    manifest = _manifest(team, scope="epic-race", session="gone-session")
    _stub_graph(monkeypatch, {"epic-race": "fno"})
    from fno.agents import registry as registry_mod

    real_update = registry_mod.update_registry

    def _role_successor_then_update(fn):
        real_update(
            lambda rows: rows
            + [
                AgentEntry(
                    name="successor",
                    cwd="/tmp",
                    log_path="",
                    harness="claude",
                    harness_session_id="successor-session",
                    status="busy",
                    role_level=2,
                    role_scope="epic-race",
                    role_grantor="human",
                )
            ]
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _role_successor_then_update)

    result = _done("--scope", "epic-race")

    assert result.exit_code == 1, result.output
    assert "mid-expiry" in result.output
    assert _row("successor").role_scope == "epic-race"
    assert manifest.exists()
