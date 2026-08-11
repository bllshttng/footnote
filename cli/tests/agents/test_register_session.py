"""Tests for register-existing-session (bus epic G4 / US7).

Covers AC7:
  AC7-HP   register makes a hand-started session addressable by name
  AC7-ERR  registry failure is fail-open + emits a warning event
  AC7-UI   the registered row carries provider/cwd/status (verifiable)
  AC7-EDGE two sessions in one cwd register under distinct names
  AC7-FR   a registered session that exits reconciles to orphaned

The registration core lives in ``fno.agents.registry`` and the
fail-open SessionStart entry point in ``fno.agents.register_session``.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _events(tmp_path: Path) -> list[dict]:
    path = tmp_path / ".fno" / "events.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


# ---------------------------------------------------------------------------
# AC7-HP / AC7-UI: registration creates an addressable, verifiable row
# ---------------------------------------------------------------------------


def test_ac7_hp_registers_addressable_entry(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, register_existing_session

    entry = register_existing_session(
        provider="claude",
        session_id="ef9982cc-2543-4cea-9a20-081cca7119f6",
        cwd="/home/user/project",
    )

    assert entry.harness == "claude"
    assert entry.short_id == "ef9982cc-2543-4cea-9a20-081cca7119f6"
    # Registered NON-live: a hand-started session has no live transport, so it
    # must not be a resolve_to_project anycast target (else default sends
    # dead-letter to inbox/<agent-name>/, which its wake hook never reads).
    assert entry.status == "idle"
    # Derived name is the canonical first-eight, the mailbox id a peer addresses.
    assert entry.name == "ef9982cc"

    # AC7-UI: a fresh load shows the row with provider/cwd/status intact.
    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].name == entry.name
    assert rows[0].cwd == "/home/user/project"
    assert rows[0].status == "idle"


def test_ac7_hp_idempotent_on_resame_session(tmp_path: Path, monkeypatch) -> None:
    """The hook re-firing for the same session refreshes, never duplicates."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, register_existing_session

    register_existing_session(provider="claude", session_id="sess-1", cwd="/a")
    second = register_existing_session(provider="claude", session_id="sess-1", cwd="/b")

    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].cwd == "/b"  # refreshed in place
    assert rows[0].status == "idle"
    assert second.name == rows[0].name


# ---------------------------------------------------------------------------
# AC7-EDGE: two sessions in one cwd register under distinct names
# ---------------------------------------------------------------------------


def test_ac7_edge_two_sessions_one_cwd_distinct_names(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, register_existing_session

    a = register_existing_session(provider="claude", session_id="11111111-aaaa", cwd="/shared")
    b = register_existing_session(provider="claude", session_id="22222222-bbbb", cwd="/shared")

    assert a.name != b.name
    rows = load_registry()
    assert len(rows) == 2
    ids = {r.short_id for r in rows}
    assert ids == {"11111111-aaaa", "22222222-bbbb"}


def test_ac4_err_generated_name_collision_fails_closed(tmp_path: Path, monkeypatch) -> None:
    """A canonical collision never mints an order-dependent numeric address."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import (
        AgentResolutionError,
        load_registry,
        register_existing_session,
    )

    a = register_existing_session(
        provider="claude", session_id="tail0001-aaaa-bbbb-cccc-dddddddddddd", cwd="/s"
    )
    with pytest.raises(AgentResolutionError, match="canonical handle.*collision"):
        register_existing_session(
            provider="claude", session_id="tail0001-1111-2222-3333-444455556666", cwd="/s"
        )

    assert a.name == "tail0001"
    assert len(load_registry()) == 1


def test_named_row_does_not_hide_its_canonical_handle_collision(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import (
        AgentResolutionError,
        load_registry,
        register_existing_session,
    )

    register_existing_session(
        provider="claude", session_id="session-A-tail0001", cwd="/s", name="friendly"
    )
    with pytest.raises(AgentResolutionError, match="canonical handle.*collision"):
        register_existing_session(
            provider="claude",
            session_id="session-B-tail0001",
            cwd="/s",
            name="another-friendly",
        )
    assert [row.name for row in load_registry()] == ["friendly"]


def test_friendly_name_is_suffixed_away_from_existing_handle_namespace(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import register_existing_session

    register_existing_session(
        provider="claude", session_id="deadbeef-0000-0000-0000-000000000000", cwd="/s", name="worker"
    )
    entry = register_existing_session(
        provider="claude", session_id="cafebabe-0000-0000-0000-000000000000", cwd="/s", name="deadbeef"
    )
    assert entry.name == "deadbeef-2"


def test_transport_short_id_cannot_collide_with_existing_handle_namespace(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentResolutionError, register_existing_session

    register_existing_session(
        provider="codex", session_id="deadbeef-0000-0000-0000-000000000000", cwd="/s", name="worker"
    )
    with pytest.raises(AgentResolutionError, match="transport short id.*collision"):
        register_existing_session(
            provider="codex",
            session_id="cafebabe-0000-0000-0000-000000000000",
            cwd="/s",
            name="other",
            short_id="deadbeef",
        )


def test_reregister_cannot_refresh_transport_short_id_into_a_collision(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentResolutionError, register_existing_session

    register_existing_session(
        provider="codex", session_id="deadbeef-0000-0000-0000-000000000000", cwd="/s", name="worker"
    )
    register_existing_session(
        provider="codex",
        session_id="cafebabe-0000-0000-0000-000000000000",
        cwd="/s",
        name="other",
        short_id="transport",
    )
    with pytest.raises(AgentResolutionError, match="refreshing session"):
        register_existing_session(
            provider="codex",
            session_id="cafebabe-0000-0000-0000-000000000000",
            cwd="/s",
            short_id="deadbeef",
        )


# ---------------------------------------------------------------------------
# Input validation
# ---------------------------------------------------------------------------


def test_register_rejects_unknown_provider(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import register_existing_session

    with pytest.raises(ValueError, match="unknown provider"):
        register_existing_session(provider="bogus", session_id="x", cwd="/s")


def test_register_rejects_empty_session_id(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import register_existing_session

    with pytest.raises(ValueError, match="session_id"):
        register_existing_session(provider="claude", session_id="", cwd="/s")


# ---------------------------------------------------------------------------
# AC7-ERR: the SessionStart entry point is fail-open + emits a warning event
# ---------------------------------------------------------------------------


def test_ac7_err_main_failopen_emits_event(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import register_session

    def _boom(**_kwargs):
        raise OSError("registry locked")

    monkeypatch.setattr(register_session, "register_existing_session", _boom)

    rc = register_session.main(
        ["--harness", "claude", "--session-id", "sess-x", "--cwd", "/s"]
    )

    assert rc == 0  # session start is never blocked
    kinds = [e["kind"] for e in _events(tmp_path)]
    assert "session_register_failed" in kinds


def test_main_success_emits_registered_event(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import register_session
    from fno.agents.registry import load_registry

    rc = register_session.main(
        ["--harness", "claude", "--session-id", "sess-ok", "--cwd", "/proj"]
    )

    assert rc == 0
    assert len(load_registry()) == 1
    kinds = [e["kind"] for e in _events(tmp_path)]
    assert "session_registered" in kinds


def test_main_empty_session_id_is_silent_noop(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import register_session
    from fno.agents.registry import load_registry

    rc = register_session.main(
        ["--harness", "claude", "--session-id", "", "--cwd", "/proj"]
    )

    assert rc == 0
    assert load_registry() == []
    assert _events(tmp_path) == []  # no noise when there's nothing to register


def test_main_accepts_provider_alias(tmp_path: Path, monkeypatch) -> None:
    # --provider is the axis-rename alias for --harness (x-bab1); it must still
    # register the same harness so the fail-soft SessionStart hook survives any
    # cutover skew between hook and module.
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import register_session
    from fno.agents.registry import load_registry

    rc = register_session.main(
        ["--provider", "codex", "--session-id", "sess-alias", "--cwd", "/proj"]
    )

    assert rc == 0
    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].harness == "codex"


# ---------------------------------------------------------------------------
# P1 (codex review): a registered session is not a live anycast target, so
# `send --to-project` queues durable to the PROJECT (delivered to the inbox the
# session drains) instead of dead-lettering under inbox/<agent-name>/.
# ---------------------------------------------------------------------------


def test_registered_session_not_a_live_anycast_target(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import resolve_to_project
    from fno.agents.registry import register_existing_session

    # A project dir whose settings name it "myproj"; the session runs there.
    proj = tmp_path / "myproj"
    (proj / ".fno").mkdir(parents=True)
    (proj / ".fno" / "settings.yaml").write_text(
        "project: myproj\n", encoding="utf-8"
    )
    register_existing_session(
        provider="claude", session_id="hand-started", cwd=str(proj)
    )

    res = resolve_to_project("myproj")
    # Idle (transportless) -> no live candidate -> durable queue to the project.
    assert res.durable is True
    assert res.recipient is None


# ---------------------------------------------------------------------------
# `fno agents register` verb (the /fno-me seam): self-service join resolved
# from the ambient harness identity, no --session-id argument.
# ---------------------------------------------------------------------------

_MARKERS = (
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "GEMINI_SESSION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "FNO_AGENT_SELF",
)


def test_register_verb_joins_under_canonical_handle(tmp_path: Path, monkeypatch) -> None:
    """A claude session self-registers under its canonical ambient-id first-eight."""
    use_tmpdir(monkeypatch, tmp_path)
    for m in _MARKERS:
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "deadbeef-1111-2222-3333-444455556666")
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app
    from fno.agents.registry import load_registry

    result = CliRunner().invoke(agents_app, ["register"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {"registered": True, "name": "deadbeef", "harness": "claude"}
    rows = load_registry()
    assert len(rows) == 1 and rows[0].name == "deadbeef" and rows[0].status == "idle"


def test_register_then_whoami_reports_registered(tmp_path: Path, monkeypatch) -> None:
    """Regression (codex PR#451 P2): a /fno-me claude session must resolve as
    registered. register stores the FULL uuid in short_id for claude, and
    whoami's session-id fallback (_find_by_session) must still find it via the
    de-hyphenated prefix match - else whoami reports unregistered despite the
    written row (exit 3), defeating the verb."""
    use_tmpdir(monkeypatch, tmp_path)
    for m in _MARKERS:
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "ef9982cc-2543-4cea-9a20-081cca7119f6")
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    runner = CliRunner()
    assert runner.invoke(agents_app, ["register"]).exit_code == 0
    result = runner.invoke(agents_app, ["whoami", "--json"])
    assert result.exit_code == 0, result.output  # exit 3 == unregistered (the bug)
    assert '"registered": true' in result.output
    assert '"name": "ef9982cc"' in result.output


def test_register_verb_exit3_without_ambient_identity(tmp_path: Path, monkeypatch) -> None:
    """No harness marker in env -> nothing addressable -> exit 3, no row written."""
    use_tmpdir(monkeypatch, tmp_path)
    for m in _MARKERS:
        monkeypatch.delenv(m, raising=False)
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app
    from fno.agents.registry import load_registry

    result = CliRunner().invoke(agents_app, ["register"])
    assert result.exit_code == 3
    assert load_registry() == []


def test_ac7_fr_unreachable_registered_session_orphaned(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import dispatch
    from fno.agents.dispatch import reconcile_agents
    from fno.agents.harnesses import claude as claude_mod
    from fno.agents.registry import load_registry, register_existing_session

    register_existing_session(provider="claude", session_id="dead-sess", cwd="/proj")

    # claude installed, but the hand-started session is no longer reachable
    # (it exited without dereg). reconcile must flip it to orphaned so a
    # later send demotes to the durable queue rather than a dead transport.
    monkeypatch.setattr(dispatch, "is_provider_available", lambda name: True)
    monkeypatch.setattr(claude_mod, "claude_logs_reachable", lambda *a, **k: False)

    reconcile_agents()

    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].status == "orphaned"


# ---------------------------------------------------------------------------
# x-1e34: a harness that re-mints the session id footnote passed at spawn
#
# Observed on claude: a worker launched as `claude --session-id <uuid>` carried
# on under a DIFFERENT uuid ~35s in, taking its transcript with it (identical
# message uuids on both sides -- a rename with carry-over, not a fork into two
# live sessions). The row kept the birth id and stopped addressing anything.
#
# The row NAME survives that, which is why the restamp keys on it.
# ---------------------------------------------------------------------------

BIRTH = "e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
REMINT = "08054b1d-a907-47ab-a3d2-4a1e7a87eb4e"


def _spawned_row(name: str = "target-x-f0c2", session_id: str = BIRTH):
    """A footnote-spawned claude worker row as `fno agents spawn` writes it."""
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            harness="claude",
            harness_session_id=session_id,
            short_id=session_id.split("-", 1)[0],
            cwd="/proj",
            log_path="",
            status="live",
        )
    ])


def test_restamp_repoints_row_at_the_reminted_id(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, restamp_harness_session_id

    _spawned_row()
    entry = restamp_harness_session_id(
        name="target-x-f0c2", harness="claude", session_id=REMINT
    )

    assert entry is not None
    rows = load_registry()
    # One worker, one row: the correction lands in place, never as a second row.
    assert len(rows) == 1
    assert rows[0].harness_session_id == REMINT
    # claude addresses by the 8-hex jobId in short_id, so a restamp that fixed
    # only harness_session_id would leave attach/resume on the dead id.
    assert rows[0].short_id == "08054b1d"
    # Untouched: the restamp corrects identity, not lifecycle.
    assert rows[0].status == "live"
    assert rows[0].name == "target-x-f0c2"


def test_restamp_is_a_noop_when_the_id_already_matches(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import restamp_harness_session_id

    _spawned_row()
    assert restamp_harness_session_id(
        name="target-x-f0c2", harness="claude", session_id=BIRTH
    ) is None


def test_restamp_ignores_an_unknown_name_or_a_harness_mismatch(
    tmp_path: Path, monkeypatch
) -> None:
    """The name is a PK, not a search: a miss corrects nothing and creates nothing."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, restamp_harness_session_id

    _spawned_row()
    assert restamp_harness_session_id(
        name="someone-else", harness="claude", session_id=REMINT
    ) is None
    assert restamp_harness_session_id(
        name="target-x-f0c2", harness="codex", session_id=REMINT
    ) is None

    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].harness_session_id == BIRTH


def test_restamp_keeps_an_independent_short_id(tmp_path: Path, monkeypatch) -> None:
    """A short that is NOT the stale uuid's prefix is a transport key of its own
    (a bg jobId minted separately), so we have no basis to re-derive it."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, restamp_harness_session_id, write_registry

    write_registry([
        AgentEntry(
            name="w1",
            harness="claude",
            harness_session_id=BIRTH,
            short_id="deadbeef",  # unrelated to BIRTH's leading segment
            cwd="/proj",
            log_path="",
        )
    ])
    restamp_harness_session_id(name="w1", harness="claude", session_id=REMINT)

    rows = load_registry()
    assert rows[0].harness_session_id == REMINT
    assert rows[0].short_id == "deadbeef"


def test_main_agent_self_restamps_instead_of_registering(tmp_path: Path, monkeypatch) -> None:
    """The whole point of the --agent-self branch: registration keys its upsert
    on harness_session_id, so routing a re-minted worker through it would MISS
    and append a second row for one worker."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.register_session import main
    from fno.agents.registry import load_registry

    _spawned_row()
    assert main([
        "--harness", "claude",
        "--session-id", REMINT,
        "--cwd", "/proj",
        "--agent-self", "target-x-f0c2",
    ]) == 0

    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].name == "target-x-f0c2"
    assert rows[0].harness_session_id == REMINT
    assert "session_id_restamped" in [e["kind"] for e in _events(tmp_path)]


def test_main_agent_self_is_failopen(tmp_path: Path, monkeypatch) -> None:
    """SessionStart must never block on a locked or unwritable registry."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import register_session

    def _boom(**kwargs):
        raise OSError("registry unwritable")

    monkeypatch.setattr(register_session, "restamp_harness_session_id", _boom)
    assert register_session.main([
        "--harness", "claude",
        "--session-id", REMINT,
        "--cwd", "/proj",
        "--agent-self", "w1",
    ]) == 0
    assert "session_restamp_failed" in [e["kind"] for e in _events(tmp_path)]


def test_restamp_waits_for_a_row_the_spawner_has_not_written_yet(
    tmp_path: Path, monkeypatch
) -> None:
    """The spawner appends the row after `mux pane run` returns, so a fast worker
    can run this hook first. Losing that race is unrecoverable on any route where
    the restamp is the only path to an id, so the hook waits for its own row.

    The row appearing between the existence check and the restamp must still be
    restamped: checking existence first is what makes that safe.
    """
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.register_session as rs

    calls = {"n": 0}

    def _late_row(name: str, harness: str) -> bool:
        # Absent on the first look; the spawner writes it just before the second.
        calls["n"] += 1
        if calls["n"] == 2:
            _spawned_row()
        return calls["n"] > 2

    monkeypatch.setattr(rs, "_row_exists", _late_row)
    monkeypatch.setattr(rs, "_RESTAMP_ROW_POLL_S", 0.0)
    monkeypatch.setenv("FNO_AGENT_ROW_PENDING", "target-x-f0c2")

    assert rs._restamp("target-x-f0c2", "claude", REMINT) == 0

    from fno.agents.registry import load_registry

    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].harness_session_id == REMINT, "the late row must still be restamped"


def test_restamp_ignores_a_pending_marker_inherited_from_its_parent(
    tmp_path: Path, monkeypatch
) -> None:
    """A pane worker passes its whole environment to any one-shot it launches, so
    the marker is inherited by children that are not the pane. It names the pane
    and the child overwrites FNO_AGENT_SELF, so the mismatch cancels the wait
    without any spawn path having to remember to clear it."""
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.register_session as rs

    monkeypatch.setenv("FNO_AGENT_ROW_PENDING", "the-parent-pane")

    def _no_sleep(_s: float) -> None:
        raise AssertionError("an inherited marker must not re-enable the wait")

    monkeypatch.setattr(rs.time, "sleep", _no_sleep)
    monkeypatch.setattr(rs, "_row_exists", lambda *_a: False)

    assert rs._restamp("the-nested-one-shot", "claude", REMINT) == 0


def test_restamp_never_waits_for_a_row_a_headless_one_shot_will_not_get(
    tmp_path: Path, monkeypatch
) -> None:
    """A rowless headless one-shot sets FNO_AGENT_SELF too. Keying the wait on
    that alone made every one-shot sit out the whole deadline before its first
    prompt, waiting for a row that is never coming."""
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.register_session as rs

    monkeypatch.delenv("FNO_AGENT_ROW_PENDING", raising=False)

    def _no_sleep(_s: float) -> None:
        raise AssertionError("a rowless substrate must not wait")

    monkeypatch.setattr(rs.time, "sleep", _no_sleep)
    monkeypatch.setattr(rs, "_row_exists", lambda *_a: False)

    assert rs._restamp("worker-headless", "claude", REMINT) == 0


def test_restamp_does_not_poll_when_the_row_is_already_current(
    tmp_path: Path, monkeypatch
) -> None:
    """The ordinary pinned path writes an already-correct row. Waiting out the
    deadline there would add the whole wait to every spawned session start."""
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.register_session as rs

    _spawned_row()

    def _no_sleep(_s: float) -> None:
        raise AssertionError("must not sleep when the row already exists")

    monkeypatch.setattr(rs.time, "sleep", _no_sleep)

    assert rs._restamp("target-x-f0c2", "claude", BIRTH) == 0


def test_restamp_promotes_a_spawning_row_once_the_worker_names_itself(
    tmp_path: Path, monkeypatch
) -> None:
    """A happy-hosted claude pane cannot be given an id at spawn (happy discards
    the pinned one), so its row parks at `spawning` and the restamp is the ONLY
    path to an id. Leaving it `spawning` after the worker has proven its own
    identity is the same lie as calling it `live` before, just inverted.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, restamp_harness_session_id, write_registry

    write_registry([
        AgentEntry(
            name="worker-happy-pane",
            harness="claude",
            harness_session_id=None,
            cwd="/proj",
            log_path="",
            status="spawning",
            mux={"session": "main", "pane_id": 11},
        )
    ])
    entry = restamp_harness_session_id(
        name="worker-happy-pane", harness="claude", session_id=REMINT
    )

    assert entry is not None
    rows = load_registry()
    assert rows[0].harness_session_id == REMINT
    assert rows[0].status == "live", "a worker that named itself is addressable"
    assert rows[0].short_id == "", "still a mux row: exactly one live ref"


def test_restamp_does_not_disturb_a_non_spawning_status(
    tmp_path: Path, monkeypatch
) -> None:
    """Only `spawning` is promoted. Every other status is owned by something
    that knows more about the worker's lifecycle than a SessionStart hook does."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, restamp_harness_session_id, write_registry

    write_registry([
        AgentEntry(
            name="worker-idle-pane",
            harness="claude",
            harness_session_id=BIRTH,
            cwd="/proj",
            log_path="",
            status="idle",
            mux={"session": "main", "pane_id": 12},
        )
    ])
    restamp_harness_session_id(
        name="worker-idle-pane", harness="claude", session_id=REMINT
    )

    assert load_registry()[0].status == "idle"


def test_restamp_corrects_a_mux_hosted_row(tmp_path: Path, monkeypatch) -> None:
    """The reported row was PANE-hosted, and a mux row is the one shape where
    filling short_id is illegal: `_validate_single_live_ref` enforces mux XOR
    worker XOR bg, so synthesizing a transport key here makes write_registry
    raise, the fail-open except swallow it, and the id change never persist --
    a restamp that no-ops on precisely the row it exists to fix.

    harness_session_id alone is enough to re-address a mux row: `resolve_agent`
    matches the full id and the short derived FROM it, neither of which reads
    the stored short_id.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, restamp_harness_session_id, write_registry

    write_registry([
        AgentEntry(
            name="target-x-f0c2-pane-identity",
            harness="claude",
            harness_session_id=BIRTH,
            cwd="/proj",
            log_path="",
            status="live",
            mux={"session": "main", "pane_id": 10},
        )
    ])
    entry = restamp_harness_session_id(
        name="target-x-f0c2-pane-identity", harness="claude", session_id=REMINT
    )

    assert entry is not None
    rows = load_registry()
    assert rows[0].harness_session_id == REMINT, "the id change must actually persist"
    assert rows[0].short_id == "", "a mux row holds exactly one live ref"
    assert rows[0].mux == {"session": "main", "pane_id": 10}
