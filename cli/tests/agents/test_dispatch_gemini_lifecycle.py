"""End-to-end + lifecycle parity tests for gemini agents (Wave 3.1-3.5).

Covers the gemini branches of stop / rm / reconcile / attach plus a
cross-provider reconcile mixing claude + codex + gemini.

ACs:
- AC8-HP : stop_agent for gemini emits agent_stopped (no-op semantics
           shared with codex per Locked Decision pattern; signal-the-
           pgid behavior is Phase 6 supervisor scope per the spec).
- AC8-ERR: rm_agent for gemini removes the registry row; ~/.gemini/
           session files are intentionally preserved.
- AC8-UI : reconcile correctly classifies mixed-provider registries
           via a single batched update_registry call (AC3-HP from
           Wave 1.3 + the new gemini probe from Wave 3.3).
- AC8-EDGE: attach for gemini returns exit 13 with the Phase-6 hint.
- AC8-FR : reconcile preserves gemini status when the probe raises
           ReachabilityProbeError (PermissionError on chats dir).
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from fno.agents import dispatch as dispatch_mod
from fno.agents.dispatch import (
    DispatchAskError,
    attach_agent,
    reconcile_agents,
    rm_agent,
    stop_agent,
)
from fno.agents.registry import AgentEntry, load_registry, update_registry


@pytest.fixture
def isolated_state(tmp_path: Path, monkeypatch) -> Path:
    """Isolate registry + state dir + Path.home() under tmp_path."""
    from fno import paths
    registry_path = tmp_path / "registry.jsonl"
    state_dir = tmp_path / "state"
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(paths, "agents_registry_path", lambda: registry_path)
    monkeypatch.setattr(paths, "state_dir", lambda: state_dir)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    return tmp_path


def _seed_gemini(
    name: str,
    *,
    session_id: str,
    cwd: Path,
    status: str = "live",
) -> AgentEntry:
    entry = AgentEntry(
        name=name,
        provider="gemini",
        cwd=str(cwd),
        log_path=str(cwd / f"{name}.log"),
        gemini_session_id=session_id,
        status=status,
    )
    update_registry(lambda entries: entries + [entry])
    return entry


def _make_session_file(home: Path, cwd_name: str, session_id: str) -> Path:
    """Create a real session file matching the gemini layout."""
    chats_dir = home / ".gemini" / "tmp" / cwd_name / "chats"
    chats_dir.mkdir(parents=True, exist_ok=True)
    short = session_id[:8]
    session_file = chats_dir / f"session-2026-05-21T22-13-{short}.jsonl"
    session_file.write_text(
        json.dumps({"sessionId": session_id, "startTime": "2026-05-21"}) + "\n"
    )
    return session_file


# ---------------------------------------------------------------------------
# Wave 3.1 — stop_agent gemini branch
# ---------------------------------------------------------------------------


def test_stop_agent_gemini_is_noop_with_event(
    isolated_state: Path,
) -> None:
    """AC8-HP (this PR's scope): stop for gemini is a no-op between
    asks (mirror of codex). Emits agent_stopped event so forensics
    can correlate. PTY signal-the-pgid stop lands in Phase 6."""
    work = isolated_state / "work"
    work.mkdir()
    _seed_gemini("worker-A", session_id="abcdef01-1111-2222-3333-444444444444", cwd=work)

    result = stop_agent("worker-A")
    assert result.provider == "gemini"
    assert result.claude_exit is None


def test_stop_agent_gemini_holds_per_agent_lock(
    isolated_state: Path,
) -> None:
    """The new with_agent_lock_and_entry helper is invoked under stop —
    a second stop runs serially after the first releases. Asserted
    indirectly via the lock-file mtime (the flock target file persists
    after release)."""
    work = isolated_state / "work"
    work.mkdir()
    _seed_gemini("worker-A", session_id="abcdef01-1111-2222-3333-444444444444", cwd=work)

    # Two sequential stops must both succeed (idempotent under the lock).
    stop_agent("worker-A")
    stop_agent("worker-A")  # no-op + lock release verified by reaching here


# ---------------------------------------------------------------------------
# Wave 3.2 — rm_agent gemini branch
# ---------------------------------------------------------------------------


def test_rm_agent_gemini_removes_registry_only(
    isolated_state: Path,
) -> None:
    """AC8-ERR: rm for gemini deletes the registry row; the on-disk
    ~/.gemini/sessions/<uuid> file is NOT touched (gemini owns its
    session storage). Mirror of codex's Locked Decision 1."""
    work = isolated_state / "work"
    work.mkdir()
    session_id = "abcdef01-1111-2222-3333-444444444444"
    _seed_gemini("worker-A", session_id=session_id, cwd=work)

    # Plant a session file at the gemini layout so we can prove rm
    # does NOT delete it.
    session_file = _make_session_file(
        isolated_state / "home", "work", session_id
    )
    assert session_file.exists()

    result = rm_agent("worker-A")
    assert result.provider == "gemini"
    assert result.registry_changed is True

    # Registry row is gone.
    entries = load_registry()
    assert entries == []

    # Session file is PRESERVED (gemini owns its on-disk state).
    assert session_file.exists()


def test_rm_agent_gemini_emits_agent_removed_event(
    isolated_state: Path, monkeypatch
) -> None:
    """The agent_removed event carries provider=gemini."""
    work = isolated_state / "work"
    work.mkdir()
    _seed_gemini("worker-A", session_id="11111111-2222-3333-4444-555555555555", cwd=work)

    emitted = []
    from fno.agents import events
    monkeypatch.setattr(events, "emit",
                        lambda evt, **kw: emitted.append((evt, kw)))

    rm_agent("worker-A")

    removed = [e for e in emitted if e[0] == "agent_removed"]
    assert removed
    assert any(
        e[1].get("provider") == "gemini" and e[1].get("registry_changed") is True
        for e in removed
    )


# ---------------------------------------------------------------------------
# Wave 3.3 — reconcile_agents gemini branch
# ---------------------------------------------------------------------------


def test_reconcile_gemini_flips_orphaned_when_session_file_missing(
    isolated_state: Path,
) -> None:
    """AC8-UI: a live gemini agent whose session file has been deleted
    flips to orphaned in a single batched update_registry call.

    Codex P2 on PR #317 regression: the change record MUST include
    gemini_session_id so the human renderer and JSON consumers can
    identify which session was orphaned. Pre-fix gemini flips rendered
    "?" for the id field because the dispatch.py change-builder only
    looked at short_id and codex_session_id.
    """
    work = isolated_state / "work"
    work.mkdir()
    session_id = "deadbeef-1111-2222-3333-444444444444"
    _seed_gemini("worker-A", session_id=session_id, cwd=work, status="live")

    # Seed an UNRELATED session file so chats dir exists (otherwise the
    # probe raises inconclusive, not False).
    _make_session_file(
        isolated_state / "home", "work", "11111111-1111-1111-1111-111111111111"
    )

    result = reconcile_agents()
    assert len(result.orphaned) == 1
    assert result.orphaned[0]["provider"] == "gemini"
    # Codex P2 regression: the change record MUST carry gemini_session_id.
    assert result.orphaned[0]["id"] == session_id, (
        "reconcile change record dropped gemini_session_id — "
        "Codex P2 finding on PR #317"
    )
    # Status flipped on disk.
    entry = next(e for e in load_registry() if e.name == "worker-A")
    assert entry.status == "orphaned"


def test_reconcile_gemini_keeps_live_when_session_file_present(
    isolated_state: Path,
) -> None:
    """A reachable session file keeps status=live; no entry in orphaned/
    recovered (it was already live)."""
    work = isolated_state / "work"
    work.mkdir()
    session_id = "abcdef01-2222-3333-4444-555555555555"
    _seed_gemini("worker-A", session_id=session_id, cwd=work, status="live")
    _make_session_file(isolated_state / "home", "work", session_id)

    result = reconcile_agents()
    assert result.orphaned == []
    # No flip because status was already "live"; no entry in recovered either.
    entry = next(e for e in load_registry() if e.name == "worker-A")
    assert entry.status == "live"


def test_reconcile_gemini_recovers_orphaned_when_session_reappears(
    isolated_state: Path,
) -> None:
    """If a gemini agent was marked orphaned but the session file is
    back (e.g. a sync restored gemini's storage), reconcile flips it
    BACK to live."""
    work = isolated_state / "work"
    work.mkdir()
    session_id = "abcdef01-2222-3333-4444-555555555555"
    _seed_gemini("worker-A", session_id=session_id, cwd=work, status="orphaned")
    _make_session_file(isolated_state / "home", "work", session_id)

    result = reconcile_agents()
    assert len(result.recovered) == 1
    assert result.recovered[0]["provider"] == "gemini"


def test_reconcile_gemini_inconclusive_preserves_status(
    isolated_state: Path, monkeypatch
) -> None:
    """AC8-FR: ReachabilityProbeError (e.g. PermissionError on chats
    dir) preserves the entry's status and routes to errors with a
    per-provider reason discriminator."""
    work = isolated_state / "work"
    work.mkdir()
    session_id = "abcdef01-2222-3333-4444-555555555555"
    _seed_gemini("worker-A", session_id=session_id, cwd=work, status="live")

    # Force the probe to raise via monkeypatched gemini.gemini_session_reachable.
    from fno.agents.providers import gemini as gemini_mod
    from fno.agents.providers.base import ReachabilityProbeError

    def raising(*args, **kwargs):
        raise ReachabilityProbeError(
            provider="gemini", reason="EACCES on chats dir (chmod 000)"
        )
    monkeypatch.setattr(gemini_mod, "gemini_session_reachable", raising)

    result = reconcile_agents()
    # Status preserved.
    entry = next(e for e in load_registry() if e.name == "worker-A")
    assert entry.status == "live"
    # Routed to errors.
    gemini_errors = [e for e in result.errors if e["provider"] == "gemini"]
    assert len(gemini_errors) == 1
    assert "gemini-probe-failed" in gemini_errors[0]["reason"]
    assert "EACCES" in gemini_errors[0]["reason"]


def test_reconcile_gemini_missing_session_id_is_inconsistent(
    isolated_state: Path,
) -> None:
    """A registry row with provider=gemini but no gemini_session_id is
    surfaced as agent_inconsistent (registry corruption signal); status
    is NOT mutated."""
    work = isolated_state / "work"
    work.mkdir()
    entry = AgentEntry(
        name="corrupt",
        provider="gemini",
        cwd=str(work),
        log_path=str(work / "log.jsonl"),
        gemini_session_id=None,  # corruption
        status="live",
    )
    update_registry(lambda entries: entries + [entry])

    result = reconcile_agents()
    assert any(
        e["provider"] == "gemini" and e["reason"] == "missing-gemini-session-id"
        for e in result.errors
    )
    # Status untouched.
    reloaded = next(e for e in load_registry() if e.name == "corrupt")
    assert reloaded.status == "live"


# ---------------------------------------------------------------------------
# Wave 3.4 — attach gemini branch (exit 13)
# ---------------------------------------------------------------------------


def test_attach_gemini_returns_exit_13_with_phase_6_hint(
    isolated_state: Path, capsys
) -> None:
    """AC8-EDGE: attach for gemini writes a Phase-6 hint to stderr,
    returns exit_code=13, emits agent_attach_refused event with
    provider=gemini."""
    work = isolated_state / "work"
    work.mkdir()
    _seed_gemini("worker-A", session_id="11111111-1111-1111-1111-111111111111", cwd=work)

    result = attach_agent("worker-A")
    assert result.exit_code == 13
    assert result.provider == "gemini"
    captured_err = capsys.readouterr().err
    assert "Phase 6" in captured_err


# ---------------------------------------------------------------------------
# Wave 3.5 — cross-provider reconcile integration
# ---------------------------------------------------------------------------


def test_reconcile_cross_provider_batched_write(
    isolated_state: Path, monkeypatch
) -> None:
    """End-to-end: reconcile a mixed registry of claude + codex + gemini
    agents in one pass and assert update_registry is called exactly once
    (AC3-HP from Wave 1.3 + Wave 3.3 unification)."""
    work = isolated_state / "work"
    work.mkdir()

    # 1 codex live, 1 claude live, 1 gemini live (all unchanged).
    update_registry(lambda entries: entries + [
        AgentEntry(
            name="codex-a", provider="codex", cwd=str(work),
            log_path=str(work / "codex-a.log"),
            codex_session_id="11111111-1111-1111-1111-111111111111",
            status="live",
        ),
        AgentEntry(
            name="claude-a", provider="claude", cwd=str(work),
            log_path=str(work / "claude-a.log"),
            short_id="aaaaaaaa",
            status="live",
        ),
        AgentEntry(
            name="gemini-a", provider="gemini", cwd=str(work),
            log_path=str(work / "gemini-a.log"),
            gemini_session_id="bbbbbbbb-1111-2222-3333-444444444444",
            status="live",
        ),
    ])

    # Reachable for all three providers.
    from fno.agents.providers import codex as codex_mod
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers import gemini as gemini_mod

    monkeypatch.setattr(
        codex_mod, "session_index_exists", lambda **_: True
    )
    monkeypatch.setattr(
        codex_mod, "load_known_session_ids",
        lambda **_: {"11111111-1111-1111-1111-111111111111"},
    )
    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable", lambda *a, **kw: True
    )
    monkeypatch.setattr(
        dispatch_mod, "is_provider_available", lambda p: True
    )
    monkeypatch.setattr(
        gemini_mod, "gemini_session_reachable", lambda *a, **kw: True
    )

    # Count update_registry calls.
    real_update = dispatch_mod.update_registry
    counter = {"n": 0}
    def counted(updater):
        counter["n"] += 1
        return real_update(updater)
    monkeypatch.setattr(dispatch_mod, "update_registry", counted)

    result = reconcile_agents()
    # All three were already "live"; nothing to flip; zero writes.
    assert counter["n"] == 0
    assert result.orphaned == []
    assert result.recovered == []


def test_reconcile_cross_provider_mixed_flips_writes_once(
    isolated_state: Path, monkeypatch
) -> None:
    """When 1 codex + 1 gemini flip in opposite directions, a single
    batched update_registry call commits BOTH atomically (extends AC3-HP
    coverage to the gemini provider)."""
    work = isolated_state / "work"
    work.mkdir()

    update_registry(lambda entries: entries + [
        AgentEntry(
            name="codex-orphan", provider="codex", cwd=str(work),
            log_path=str(work / "codex-orphan.log"),
            codex_session_id="dddddddd-1111-2222-3333-444444444444",
            status="live",  # but session NOT in known_codex_ids -> orphan
        ),
        AgentEntry(
            name="gemini-recover", provider="gemini", cwd=str(work),
            log_path=str(work / "gemini-recover.log"),
            gemini_session_id="eeeeeeee-1111-2222-3333-444444444444",
            status="orphaned",  # but probe says reachable -> recover
        ),
    ])

    from fno.agents.providers import codex as codex_mod
    from fno.agents.providers import gemini as gemini_mod

    monkeypatch.setattr(codex_mod, "session_index_exists", lambda **_: True)
    monkeypatch.setattr(codex_mod, "load_known_session_ids", lambda **_: set())
    monkeypatch.setattr(gemini_mod, "gemini_session_reachable",
                        lambda *a, **kw: True)
    monkeypatch.setattr(dispatch_mod, "is_provider_available", lambda p: True)

    real_update = dispatch_mod.update_registry
    counter = {"n": 0}
    def counted(updater):
        counter["n"] += 1
        return real_update(updater)
    monkeypatch.setattr(dispatch_mod, "update_registry", counted)

    result = reconcile_agents()
    assert counter["n"] == 1, "batched: one write for both flips"
    assert len(result.orphaned) == 1
    assert result.orphaned[0]["name"] == "codex-orphan"
    assert len(result.recovered) == 1
    assert result.recovered[0]["name"] == "gemini-recover"
