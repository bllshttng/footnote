"""Wake-ladder rung 2: identity-preserving respawn (x-eea5 1.1).

wake_and_deliver revived an asleep session by forking a NEW incarnation
(dispatch_spawn --resume), even when the session was still rostered and
`claude respawn` would revive the SAME identity. Rung 2 revives an
exited-but-rostered session in place (respawn + re-inject); any miss falls
through to the fork rung so the mail is never dropped.
"""
from types import SimpleNamespace

import fno.agents.dispatch as dispatch
from fno.agents.dispatch import DispatchAskError, wake_and_deliver


def _entry(status, *, short="abc12345", name="wk-abc12345", sid="uuid-full"):
    return SimpleNamespace(
        status=status, short_id=short, name=name, harness_session_id=sid
    )


def test_roster_exited_revives_in_place(monkeypatch):
    # AC1-HP: a rostered-exited session revives via respawn + re-inject, never forks.
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("exited"))
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 0)
    monkeypatch.setattr(dispatch, "_mail_inject_claude", lambda u, t: True)
    spawned = []
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: spawned.append(k) or SimpleNamespace(short_id="FORK"),
    )
    ok, detail = wake_and_deliver("uuid-full", "wake")
    assert ok is True
    assert detail == "abc12345"  # revived short_id, not a fork id
    assert spawned == []  # never forked - one roster row, same uuid


def test_unrostered_falls_through_to_fork(monkeypatch):
    # No roster row -> rung 3 fork (the existing identity-breaking path).
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: None)
    spawned = []
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: spawned.append(k) or SimpleNamespace(short_id="FORK"),
    )
    ok, detail = wake_and_deliver("uuid-full", "wake")
    assert ok is True and detail == "FORK"
    assert spawned and spawned[0]["resume_session_id"] == "uuid-full"


def test_respawn_failure_falls_through_to_fork(monkeypatch):
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("exited"))
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 1)  # non-zero
    spawned = []
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: spawned.append(k) or SimpleNamespace(short_id="FORK"),
    )
    ok, detail = wake_and_deliver("uuid-full", "wake")
    assert ok is True and detail == "FORK"


def test_respawn_ok_inject_miss_falls_through(monkeypatch):
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("exited"))
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 0)
    monkeypatch.setattr(dispatch, "_mail_inject_claude", lambda u, t: False)
    monkeypatch.setattr(dispatch.time, "sleep", lambda s: None)  # no real waits
    spawned = []
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: spawned.append(k) or SimpleNamespace(short_id="FORK"),
    )
    ok, detail = wake_and_deliver("uuid-full", "wake")
    assert ok is True and detail == "FORK"


def test_live_roster_skips_rung2_and_forks(monkeypatch):
    # A LIVE row is not exited -> rung 2 does not apply (a live session is the
    # caller's rung-1 job; reaching here means inject failed, so fork is honest).
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("live"))
    respawned = []
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: respawned.append(s) or 0)
    spawned = []
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: spawned.append(k) or SimpleNamespace(short_id="FORK"),
    )
    ok, detail = wake_and_deliver("uuid-full", "wake")
    assert ok is True and detail == "FORK"
    assert respawned == []  # never respawned a live session


def test_fork_refusal_tokens_unchanged(monkeypatch):
    # Rung 3 fork refusals still return the documented lane-failure tokens.
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: None)

    def raise11(**k):
        raise DispatchAskError("writer held", exit_code=11)

    monkeypatch.setattr(dispatch, "dispatch_spawn", raise11)
    ok, reason = wake_and_deliver("uuid-full", "wake")
    assert ok is False and reason == "writer-possibly-live"
