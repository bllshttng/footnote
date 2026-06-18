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

    assert entry.provider == "claude"
    assert entry.claude_short_id == "ef9982cc-2543-4cea-9a20-081cca7119f6"
    # Registered NON-live: a hand-started session has no live transport, so it
    # must not be a resolve_to_project anycast target (else default sends
    # dead-letter to inbox/<agent-name>/, which its wake hook never reads).
    assert entry.status == "idle"
    # Derived name is non-empty and provider-prefixed so a peer can address it.
    assert entry.name.startswith("claude-")

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
    ids = {r.claude_short_id for r in rows}
    assert ids == {"11111111-aaaa", "22222222-bbbb"}


def test_ac7_edge_name_collision_disambiguated(tmp_path: Path, monkeypatch) -> None:
    """Two session ids sharing the first 8 chars still get distinct names."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, register_existing_session

    a = register_existing_session(provider="claude", session_id="abcd1234-XXXX", cwd="/s")
    b = register_existing_session(provider="claude", session_id="abcd1234-YYYY", cwd="/s")

    assert a.name == "claude-abcd1234"
    assert b.name == "claude-abcd1234-2"  # suffix disambiguation
    assert len(load_registry()) == 2


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
        ["--provider", "claude", "--session-id", "sess-x", "--cwd", "/s"]
    )

    assert rc == 0  # session start is never blocked
    kinds = [e["kind"] for e in _events(tmp_path)]
    assert "session_register_failed" in kinds


def test_main_success_emits_registered_event(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import register_session
    from fno.agents.registry import load_registry

    rc = register_session.main(
        ["--provider", "claude", "--session-id", "sess-ok", "--cwd", "/proj"]
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
        ["--provider", "claude", "--session-id", "", "--cwd", "/proj"]
    )

    assert rc == 0
    assert load_registry() == []
    assert _events(tmp_path) == []  # no noise when there's nothing to register


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


def test_ac7_fr_unreachable_registered_session_orphaned(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents import dispatch
    from fno.agents.dispatch import reconcile_agents
    from fno.agents.providers import claude as claude_mod
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
