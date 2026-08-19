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


def _entry(
    status,
    *,
    short="abc12345",
    name="wk-abc12345",
    sid="uuid-full",
    provider=None,
    route_settings_path=None,
):
    return SimpleNamespace(
        status=status,
        short_id=short,
        name=name,
        harness_session_id=sid,
        provider=provider,
        route_settings_path=route_settings_path,
    )


def _allow_rung2_claim(monkeypatch):
    """Stub the rung-2 single-writer guard so the revive path runs without touching
    the real claims substrate (F5)."""
    monkeypatch.setattr(dispatch, "_acquire_rung2_guard", lambda u, s: "revive:test")
    monkeypatch.setattr(dispatch, "_release_rung2_guard", lambda u, h: None)


def test_roster_exited_revives_in_place(monkeypatch):
    # AC1-HP: a rostered-exited session revives via respawn + re-inject, never forks.
    _allow_rung2_claim(monkeypatch)
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("exited"))
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 0)
    stamped = []
    monkeypatch.setattr(
        dispatch, "_stamp_revived_live", lambda entry: stamped.append(entry.name)
    )
    monkeypatch.setattr(dispatch, "_mail_inject_claude", lambda u, t, **_k: True)
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
    assert stamped == ["wk-abc12345"]


def test_routed_respawn_acquires_provider_gate_before_side_effect(monkeypatch):
    _allow_rung2_claim(monkeypatch)
    monkeypatch.setattr(
        dispatch,
        "_roster_entry_for_session",
        lambda u: _entry(
            "exited", provider="zai", route_settings_path="/route.json"
        ),
    )
    events = []

    class _Gate:
        def release(self):
            events.append("release")

    monkeypatch.setattr(
        "fno.agents.spawn_gate.run_gate",
        lambda *args, **kwargs: events.append("gate") or _Gate(),
    )
    monkeypatch.setattr(
        dispatch,
        "_respawn_claude_session",
        lambda short: events.append("respawn") or 0,
    )
    monkeypatch.setattr(
        dispatch,
        "_stamp_revived_live",
        lambda entry: events.append("stamp-live"),
    )
    monkeypatch.setattr(
        dispatch,
        "_mail_inject_claude",
        lambda uuid, text, **kwargs: events.append("inject") or True,
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **kwargs: (_ for _ in ()).throw(AssertionError("forked after revive")),
    )

    assert wake_and_deliver("uuid-full", "wake") == (True, "abc12345")
    assert events == ["gate", "respawn", "stamp-live", "inject", "release"]


def test_incomplete_forward_registry_refuses_wake(monkeypatch):
    from fno.agents.registry import LoadedRegistry

    monkeypatch.setattr(
        dispatch, "load_registry", lambda: LoadedRegistry([], complete=False)
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **kwargs: (_ for _ in ()).throw(AssertionError("spawned from partial read")),
    )

    assert wake_and_deliver("uuid-full", "wake") == (
        False,
        "registry-incomplete",
    )


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
    _allow_rung2_claim(monkeypatch)
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


def test_respawn_ok_inject_miss_does_not_create_second_worker(monkeypatch):
    _allow_rung2_claim(monkeypatch)
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("exited"))
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 0)
    monkeypatch.setattr(dispatch, "_stamp_revived_live", lambda entry: None)
    monkeypatch.setattr(dispatch, "_mail_inject_claude", lambda u, t, **_k: False)
    monkeypatch.setattr(dispatch.time, "sleep", lambda s: None)  # no real waits
    spawned = []
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: spawned.append(k) or SimpleNamespace(short_id="FORK"),
    )
    ok, detail = wake_and_deliver("uuid-full", "wake")
    assert ok is False and detail == "respawn-inject-unconfirmed"
    assert spawned == []


def test_respawn_stamp_failure_stops_worker_before_releasing_admission(monkeypatch):
    from fno.agents.registry import RegistryVersionError

    _allow_rung2_claim(monkeypatch)
    monkeypatch.setattr(
        dispatch,
        "_roster_entry_for_session",
        lambda u: _entry(
            "exited", provider="zai", route_settings_path="/route.json"
        ),
    )
    events = []

    class _Gate:
        def retain_revived_worker(self, short, *, worker_pid=None):
            events.append(("retain", short, worker_pid))

        def release_gate_mutex(self):
            events.append("release-mutex")

        def release(self):
            events.append("release")

    monkeypatch.setattr(
        "fno.agents.spawn_gate.run_gate", lambda *args, **kwargs: _Gate()
    )
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 0)
    monkeypatch.setattr(
        dispatch,
        "_stamp_revived_live",
        lambda entry: (_ for _ in ()).throw(RegistryVersionError("changed")),
    )
    monkeypatch.setattr(
        "fno.agents.harnesses.claude.claude_stop",
        lambda short: events.append(("stop", short)) or (0, ""),
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **kwargs: (_ for _ in ()).throw(AssertionError("forked")),
    )

    ok, detail = wake_and_deliver("uuid-full", "wake")

    assert ok is False and detail == "spawn-error-RuntimeError"
    assert events == [("stop", "abc12345"), "release"]


def test_respawn_stop_failure_retains_provider_reservation(monkeypatch):
    from fno.agents.registry import RegistryVersionError

    _allow_rung2_claim(monkeypatch)
    monkeypatch.setattr(
        dispatch,
        "_roster_entry_for_session",
        lambda u: _entry(
            "exited", provider="zai", route_settings_path="/route.json"
        ),
    )
    events = []

    class _Gate:
        def retain_revived_worker(self, short, *, worker_pid=None):
            events.append(("retain", short, worker_pid))

        def release_gate_mutex(self):
            events.append("release-mutex")

        def release(self):
            events.append("release")

    monkeypatch.setattr(
        "fno.agents.spawn_gate.run_gate", lambda *args, **kwargs: _Gate()
    )
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 0)
    monkeypatch.setattr(
        dispatch,
        "_stamp_revived_live",
        lambda entry: (_ for _ in ()).throw(RegistryVersionError("changed")),
    )
    monkeypatch.setattr(
        "fno.agents.harnesses.claude.claude_stop",
        lambda short: (_ for _ in ()).throw(OSError("stop unavailable")),
    )
    monkeypatch.setattr(
        "fno.agents.harnesses._claude_session_registry.roster_sessions",
        lambda: [{"short_id": "abc12345", "pid": 4242}],
    )

    ok, detail = wake_and_deliver("uuid-full", "wake")

    assert ok is False and detail == "spawn-error-RuntimeError"
    assert events == [("retain", "abc12345", 4242), "release-mutex"]


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


def test_routed_fork_holds_provider_gate_across_dispatch(monkeypatch):
    monkeypatch.setattr(
        dispatch,
        "_roster_entry_for_session",
        lambda u: _entry("live", provider="zai", route_settings_path="/route.json"),
    )
    events = []

    class _Gate:
        def release(self):
            events.append("release")

    monkeypatch.setattr(
        "fno.agents.spawn_gate.run_gate",
        lambda name, substrate, **kwargs: events.append(
            ("gate", name, substrate, kwargs)
        )
        or _Gate(),
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **kwargs: events.append(("dispatch", kwargs))
        or SimpleNamespace(short_id="FORK"),
    )

    ok, detail = wake_and_deliver("uuid-full", "wake")

    assert ok is True and detail == "FORK"
    assert events[0][0] == "gate"
    assert events[0][2:] == ("bg", {"route_provider": "zai"})
    assert events[1][0] == "dispatch"
    assert events[1][1]["route_provider"] == "zai"
    assert events[2] == "release"


def test_routed_fork_provider_refusal_launches_nothing(monkeypatch):
    from fno.agents.spawn_gate import GateRefused

    monkeypatch.setattr(
        dispatch,
        "_roster_entry_for_session",
        lambda u: _entry("live", provider="zai", route_settings_path="/route.json"),
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate.run_gate",
        lambda *args, **kwargs: (_ for _ in ()).throw(GateRefused(78)),
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **kwargs: (_ for _ in ()).throw(AssertionError("spawned past cap")),
    )

    assert wake_and_deliver("uuid-full", "wake") == (False, "spawn-exit-78")


def test_fork_refusal_tokens_unchanged(monkeypatch):
    # Rung 3 fork refusals still return the documented lane-failure tokens.
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: None)

    def raise11(**k):
        raise DispatchAskError("writer held", exit_code=11)

    monkeypatch.setattr(dispatch, "dispatch_spawn", raise11)
    ok, reason = wake_and_deliver("uuid-full", "wake")
    assert ok is False and reason == "writer-possibly-live"


# fork rung gate (x-eea5 1.2): lineage prefix + loud receipt ----------------- #
def test_fork_seed_carries_lineage_prefix(monkeypatch):
    # AC2-HP: a fork's seed prompt carries the lineage prefix naming the root.
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: None)
    seeded = {}
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: seeded.update(k) or SimpleNamespace(short_id="new12345"),
    )
    wake_and_deliver("abcdef0123456789", "do the thing")
    msg = seeded["message"]
    assert msg.startswith("[lineage: forked from abcdef01 ")
    assert "do the thing" in msg  # original prompt preserved after the prefix


def test_fork_receipt_is_loud(monkeypatch, capsys):
    # AC2-HP: the fork receipt names both the new handle and the old lineage.
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: None)
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: SimpleNamespace(short_id="new12345"),
    )
    wake_and_deliver("abcdef0123456789", "do the thing")
    err = capsys.readouterr().err
    assert "forked new incarnation new12345 from lineage abcdef01" in err


def test_revive_does_not_prefix_or_fork(monkeypatch):
    # Rung 2 revives in place: the inject gets the plain prompt (no lineage
    # prefix - identity is preserved, there is no fork), and dispatch_spawn
    # is never called.
    _allow_rung2_claim(monkeypatch)
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("exited"))
    monkeypatch.setattr(dispatch, "_respawn_claude_session", lambda s: 0)
    monkeypatch.setattr(dispatch, "_stamp_revived_live", lambda entry: None)
    injected = {}
    monkeypatch.setattr(
        dispatch,
        "_mail_inject_claude",
        lambda u, t: injected.update(text=t) or True,
    )
    spawned = []
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **k: spawned.append(k) or SimpleNamespace(short_id="FORK"),
    )
    ok, detail = wake_and_deliver("abcdef0123456789", "do the thing")
    assert ok is True and spawned == []
    assert injected["text"] == "do the thing"  # no lineage prefix on a revive


def test_rung2_claim_held_falls_through_to_fork(monkeypatch):
    # F5: a concurrent wake holds session:<uuid>; this caller must NOT respawn+
    # inject (double delivery) but fall through to the fork rung, which claims/pins.
    monkeypatch.setattr(dispatch, "_roster_entry_for_session", lambda u: _entry("exited"))
    monkeypatch.setattr(dispatch, "_acquire_rung2_guard", lambda u, s: None)  # held by other
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
    assert respawned == []  # never respawned: the guard was held
    assert spawned and spawned[0]["resume_session_id"] == "uuid-full"
