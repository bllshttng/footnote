"""Tests for the single-writer session claim guard (Task 1.2).

Before respawning an idle claude session into the stream-json host lane, the
daemon must hold an atomic `fno claim session:<uuid>` so two concurrent adopts
cannot both respawn the same transcript (double-writer = corruption), AND it
must refuse to adopt a session id currently held LIVE by another process
(a human interactive TUI / another writer) - `claude --resume` does not
self-guard against a live duplicate.

Covers AC1-EDGE (single-writer / concurrency) for US1.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


def _use_global_claims_root(monkeypatch, tmp_path: Path) -> None:
    """Redirect global session claims into tmp (global_claims_root honors this)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))


def _write_session_file(tmp_path: Path, monkeypatch, *, pid: int, job_id: str,
                        socket_path) -> None:
    """Write a ~/.claude/sessions/<pid>.json so locate_session can find it."""
    monkeypatch.setenv("HOME", str(tmp_path))
    sessions = tmp_path / ".claude" / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    (sessions / f"{pid}.json").write_text(
        json.dumps({
            "jobId": job_id,
            "kind": "bg",
            "sessionId": "019e7157-4236-7bb1-b274-ebbac6040ace",
            "messagingSocketPath": socket_path,
            "cwd": "/tmp",
        }),
        encoding="utf-8",
    )


UUID = "019e7157-4236-7bb1-b274-ebbac6040ace"


def test_acquire_session_writer_claim_happy_path(tmp_path, monkeypatch):
    """A free session:<uuid> is claimed; the claim file exists."""
    _use_global_claims_root(monkeypatch, tmp_path)
    from fno.agents.providers.claude import acquire_session_writer_claim
    from fno.claims import claim_status
    from fno.claims.io import global_claims_root

    claim = acquire_session_writer_claim(session_uuid=UUID, holder="daemon:1")
    assert claim is not None
    st = claim_status(f"session:{UUID}", root=global_claims_root())
    assert st["state"] == "live"


def test_acquire_skips_liveness_when_no_short_id(tmp_path, monkeypatch):
    """An idle session with no short-id given (never-live) acquires cleanly:
    the liveness guard only fires when a short-id is supplied."""
    _use_global_claims_root(monkeypatch, tmp_path)
    from fno.agents.providers.claude import acquire_session_writer_claim

    claim = acquire_session_writer_claim(session_uuid=UUID, holder="daemon:1")
    assert claim is not None


def test_acquire_refuses_when_session_held_live(tmp_path, monkeypatch):
    """A bg session whose supervisor is LIVE (a human TUI / another writer) is
    refused: respawning a live transcript = double-writer."""
    _use_global_claims_root(monkeypatch, tmp_path)
    _write_session_file(tmp_path, monkeypatch, pid=4242, job_id="7c5dcf5d",
                        socket_path="/tmp/sock.msg")
    import fno.agents.providers.claude as claude_mod
    from fno.agents.providers.claude import (
        SessionWriterClaimError,
        acquire_session_writer_claim,
    )

    # Force the liveness probe to report the supervisor as reachable.
    monkeypatch.setattr(claude_mod, "liveness_probe", lambda sock: True)

    with pytest.raises(SessionWriterClaimError) as exc:
        acquire_session_writer_claim(
            session_uuid=UUID, holder="daemon:1", claude_short_id="7c5dcf5d",
        )
    assert "live" in str(exc.value).lower()


def test_acquire_allows_when_session_socket_dead(tmp_path, monkeypatch):
    """A bg session whose supervisor socket is dead (idle) is adoptable: the
    liveness guard passes and the claim is taken."""
    _use_global_claims_root(monkeypatch, tmp_path)
    _write_session_file(tmp_path, monkeypatch, pid=4242, job_id="7c5dcf5d",
                        socket_path="/tmp/sock.msg")
    import fno.agents.providers.claude as claude_mod
    from fno.agents.providers.claude import acquire_session_writer_claim

    monkeypatch.setattr(claude_mod, "liveness_probe", lambda sock: False)

    claim = acquire_session_writer_claim(
        session_uuid=UUID, holder="daemon:1", claude_short_id="7c5dcf5d",
    )
    assert claim is not None


def test_concurrent_adopt_only_one_wins(tmp_path, monkeypatch):
    """AC1-EDGE: two adopts of the same session id - the first holds the claim;
    a second holder is refused (atomic single-writer)."""
    _use_global_claims_root(monkeypatch, tmp_path)
    from fno.agents.providers.claude import (
        SessionWriterClaimError,
        acquire_session_writer_claim,
    )

    first = acquire_session_writer_claim(session_uuid=UUID, holder="daemon:A")
    assert first is not None

    with pytest.raises(SessionWriterClaimError):
        acquire_session_writer_claim(session_uuid=UUID, holder="daemon:B")


def test_reacquire_same_holder_is_idempotent(tmp_path, monkeypatch):
    """Re-acquiring with the SAME holder (e.g. a daemon restart reconnecting) is
    idempotent, not a refusal."""
    _use_global_claims_root(monkeypatch, tmp_path)
    from fno.agents.providers.claude import acquire_session_writer_claim

    acquire_session_writer_claim(session_uuid=UUID, holder="daemon:A")
    again = acquire_session_writer_claim(session_uuid=UUID, holder="daemon:A")
    assert again is not None


def test_release_session_writer_claim_frees_it(tmp_path, monkeypatch):
    """Release frees the claim so a later adopt (after the child orphaned) can
    re-take it."""
    _use_global_claims_root(monkeypatch, tmp_path)
    from fno.agents.providers.claude import (
        acquire_session_writer_claim,
        release_session_writer_claim,
    )
    from fno.claims import claim_status
    from fno.claims.io import global_claims_root

    acquire_session_writer_claim(session_uuid=UUID, holder="daemon:A")
    release_session_writer_claim(session_uuid=UUID, holder="daemon:A")

    st = claim_status(f"session:{UUID}", root=global_claims_root())
    assert st["state"] == "free"
    # A different holder can now acquire.
    again = acquire_session_writer_claim(session_uuid=UUID, holder="daemon:B")
    assert again is not None


def test_release_is_silent_when_not_held(tmp_path, monkeypatch):
    """Releasing a claim that was never held is a silent no-op (idempotent
    cleanup on a child that died before the claim was recorded)."""
    _use_global_claims_root(monkeypatch, tmp_path)
    from fno.agents.providers.claude import release_session_writer_claim

    release_session_writer_claim(session_uuid=UUID, holder="daemon:A")  # no raise
