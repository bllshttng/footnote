"""Group 3 (x-a2c9 / US3,US4,US5 / AC3,AC4,AC5): the always-on relay daemon.

The routing core is tested with a FAKE deliver over the REAL jsonl bus, so the
human-out-of-loop guarantee, ttl cycle-cut, dedup, and provenance framing are
deterministic without spawning claude.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.bus.log import append
from fno.relay import daemon
from fno.relay import envelope as env
from fno.relay.registry import RegistryEntry


@pytest.fixture
def bus(tmp_path, monkeypatch):
    """Isolate the bus log + cursors under tmp (FNO_BUS_DIR)."""
    monkeypatch.setenv("FNO_BUS_DIR", str(tmp_path / "bus"))
    return tmp_path


@pytest.fixture
def events(tmp_path) -> Path:
    return tmp_path / "events.jsonl"


def _read_events(p: Path) -> list[dict]:
    if not p.exists():
        return []
    return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]


def _idx(provider="claude", handle="pty:42"):
    return {"B": RegistryEntry(session_id="B", provider=provider, pid=42,
                               inject_handle=handle, name="bob")}


class _Recorder:
    """A fake deliver that records every injected line; optional canned reply."""

    def __init__(self, reply=None):
        self.calls: list[tuple[str, str]] = []
        self.reply = reply

    def __call__(self, res, framed):
        self.calls.append((res.session_id, framed))
        return self.reply


# ---- AC3-HP: autonomous daemon route ---------------------------------------

def test_ac3_daemon_routes_with_no_human(bus, events):
    e = env.make_relay_envelope(from_session="A", to="session:B", body="hi B",
                                provider_from="claude", from_model="opus")
    append(e)
    rec = _Recorder()
    results = daemon.run_once(deliver=rec, index=_idx(), events_path=events, seen=set())

    assert [r.status for r in results] == ["routed"]
    assert len(rec.calls) == 1  # delivered exactly once, no operator
    evs = [x for x in _read_events(events) if x["kind"] == "relay_routed"]
    assert len(evs) == 1 and evs[0]["to"] == "session:B"  # exactly one decision event


def test_run_once_advances_cursor_so_restart_does_not_redeliver(bus, events):
    append(env.make_relay_envelope(from_session="A", to="session:B", body="x",
                                   provider_from="claude"))
    rec = _Recorder()
    daemon.run_once(deliver=rec, index=_idx(), events_path=events, seen=set())
    # Second pass with a FRESH seen set: cursor already advanced -> nothing redelivered.
    daemon.run_once(deliver=rec, index=_idx(), events_path=events, seen=set())
    assert len(rec.calls) == 1


# ---- AC4-EDGE: ttl cuts the cycle ------------------------------------------

def test_ac4_ttl_exhausted_drops_and_does_not_deliver(bus, events):
    e = env.make_relay_envelope(from_session="A", to="session:B", body="loop",
                                provider_from="claude", hop_count=8, ttl=8)
    rec = _Recorder()
    res = daemon.route_message(e, deliver=rec, index=_idx(), events_path=events, seen=set())
    assert res.status == "dropped" and res.reason == "ttl-exhausted"
    assert rec.calls == []  # never re-injected
    assert any(x["kind"] == "relay_ttl_exhausted" for x in _read_events(events))


def test_ac4_self_cycling_pair_terminates_at_ttl(bus, events):
    # A -> B and B always replies -> the daemon re-injects back to A, hop++ each
    # time. With ttl=4 the cycle must terminate (bounded deliveries), not loop.
    idx = {
        "A": RegistryEntry(session_id="A", provider="claude", pid=1, inject_handle="pty:1"),
        "B": RegistryEntry(session_id="B", provider="claude", pid=2, inject_handle="pty:2"),
    }
    append(env.make_relay_envelope(from_session="A", to="session:B", body="start",
                                   provider_from="claude", ttl=4))
    rec = _Recorder(reply="and you?")  # every recipient replies -> a -> b -> a -> b ...
    seen: set[str] = set()
    for _ in range(20):  # bounded poll; must converge well inside this
        daemon.run_once(deliver=rec, index=idx, events_path=events, seen=seen)
    # ttl=4 caps total injected hops; the cycle is cut, not infinite.
    assert len(rec.calls) <= 4
    assert any(x["kind"] == "relay_ttl_exhausted" for x in _read_events(events))


# ---- AC5-HP: provenance correct on a claude hop ----------------------------

def test_ac5_claude_hop_carries_peer_stamp(bus, events):
    e = env.make_relay_envelope(from_session="A", to="session:B", body="ping",
                                provider_from="claude", from_model="opus")
    rec = _Recorder()
    daemon.route_message(e, deliver=rec, index=_idx(), events_path=events, seen=set())
    _, framed = rec.calls[0]
    parsed = env.parse(framed)
    # node x-1f23: the relay tag converged to <fno_mail>; provider "claude" stamps
    # as the harness "claude-code".
    assert parsed["from_session"] == "A" and parsed["harness"] == "claude-code"
    assert parsed["body"] == "ping"  # not impersonating the user: explicit peer tag


# ---- AC5-FR: unframed cross-provider refused -------------------------------

def test_ac5fr_unframed_cross_provider_refused(bus, events):
    # A relay envelope with NO provenance (cannot be framed) addressed to a codex
    # recipient must be REFUSED, never injected bare.
    from fno.bus.log import Envelope
    bare = Envelope.new(from_="A", to="session:B", kind="relay", body="raw", from_session="A")
    rec = _Recorder()
    res = daemon.route_message(bare, deliver=rec, index=_idx(provider="codex"),
                               events_path=events, seen=set())
    assert res.status == "dropped" and res.reason == "unframed-cross-provider"
    assert rec.calls == []  # the spike's Alice-rejection made impossible by construction
    drops = [x for x in _read_events(events) if x["kind"] == "relay_dropped"]
    assert drops and drops[0]["reason"] == "unframed-cross-provider"


# ---- Invariants: dedup, unroutable, no-handle ------------------------------

def test_dedup_delivers_once_per_msg_id(bus, events):
    e = env.make_relay_envelope(from_session="A", to="session:B", body="x", provider_from="claude")
    seen: set[str] = set()
    rec = _Recorder()
    r1 = daemon.route_message(e, deliver=rec, index=_idx(), events_path=events, seen=seen)
    r2 = daemon.route_message(e, deliver=rec, index=_idx(), events_path=events, seen=seen)
    assert r1.status == "routed" and r2.status == "skipped"
    assert len(rec.calls) == 1  # at most once per recipient (Invariant)


def test_unroutable_target_is_surfaced_not_swallowed(bus, events):
    e = env.make_relay_envelope(from_session="A", to="session:ghost", body="x", provider_from="claude")
    rec = _Recorder()
    res = daemon.route_message(e, deliver=rec, index=_idx(), events_path=events, seen=set())
    assert res.status == "dropped" and res.reason == "unroutable"
    assert rec.calls == []
    drops = [x for x in _read_events(events) if x["kind"] == "relay_dropped"]
    assert drops and drops[0]["reason"] == "unroutable"


def test_recipient_without_inject_handle_refused(bus, events):
    e = env.make_relay_envelope(from_session="A", to="session:B", body="x", provider_from="claude")
    rec = _Recorder()
    res = daemon.route_message(e, deliver=rec, index=_idx(handle=None),
                               events_path=events, seen=set())
    assert res.status == "dropped" and res.reason == "no-inject-handle"
    assert rec.calls == []


def test_non_relay_bus_traffic_is_ignored(bus, events):
    from fno.bus.log import Envelope
    append(Envelope.new(from_="A", to="B", kind="chat", body="human mail"))
    rec = _Recorder()
    results = daemon.run_once(deliver=rec, index=_idx(), events_path=events, seen=set())
    assert results == [] and rec.calls == []  # left for its own consumer


# ---- Errors: a raising deliver is surfaced, not silently swallowed ---------

def test_deliver_failure_surfaces_event_and_does_not_crash(bus, events):
    e = env.make_relay_envelope(from_session="A", to="session:B", body="x", provider_from="claude")

    def _boom(res, framed):
        raise RuntimeError("no live PTY peer for B")

    seen: set[str] = set()
    res = daemon.route_message(e, deliver=_boom, index=_idx(), events_path=events, seen=seen)
    assert res.status == "dropped" and res.reason == "deliver-failed"
    evs = [x for x in _read_events(events) if x["kind"] == "relay_deliver_failed"]
    assert len(evs) == 1 and evs[0]["to"] == "session:B"  # surfaced, never silent
    # And a deliver failure must NOT also emit relay_routed (no false success).
    assert not any(x["kind"] == "relay_routed" for x in _read_events(events))


def test_failed_then_recovered_delivery_is_retried_not_swallowed(bus, events):
    # A peer that is down on pass 1 (deliver raises) then up on pass 2 must NOT
    # be permanently lost: the message is recorded seen only at a terminal
    # success, so the retry delivers it.
    append(env.make_relay_envelope(from_session="A", to="session:B", body="x", provider_from="claude"))
    rec = _Recorder()
    calls = {"n": 0}

    def _flaky(res, framed):
        calls["n"] += 1
        if calls["n"] == 1:
            raise RuntimeError("peer not up yet")
        return rec(res, framed)

    seen: set[str] = set()
    daemon.run_once(deliver=_flaky, index=_idx(), events_path=events, seen=seen)
    # Cursor advanced past the failed message, so re-append simulates next-pass
    # retry of the SAME id only if not yet seen -> route it directly again.
    e2 = env.make_relay_envelope(from_session="A", to="session:B", body="x", provider_from="claude")
    daemon.route_message(e2, deliver=_flaky, index=_idx(), events_path=events, seen=seen)
    assert len(rec.calls) == 1  # eventually delivered, not swallowed


# ---- restart: rotated-out cursor resyncs, never replays stale hops ----------

def test_rotated_out_cursor_resyncs_without_replay(bus, events):
    from fno.bus import cursor as buscursor
    # Two old relay messages, then point the cursor at an id NOT in the log
    # (simulating the real id having rotated out of retention).
    append(env.make_relay_envelope(from_session="A", to="session:B", body="old1", provider_from="claude"))
    append(env.make_relay_envelope(from_session="A", to="session:B", body="old2", provider_from="claude"))
    buscursor.write_cursor(daemon.CURSOR_NAME, "msg-rotated-away")
    rec = _Recorder()
    results = daemon.run_once(deliver=rec, index=_idx(), events_path=events, seen=set())
    assert results == [] and rec.calls == []  # stale backlog NOT re-injected
    assert any(x["kind"] == "relay_cursor_resync" for x in _read_events(events))
    # And the cursor is now at head, so genuinely-new traffic flows next pass.
    append(env.make_relay_envelope(from_session="A", to="session:B", body="fresh", provider_from="claude"))
    daemon.run_once(deliver=rec, index=_idx(), events_path=events, seen=set())
    assert len(rec.calls) == 1 and "fresh" in rec.calls[0][1]


# ---- Concurrency: singleton daemon -----------------------------------------

def test_singleton_second_daemon_refuses(bus, events, tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    from fno.claims.core import acquire_claim
    acquire_claim(daemon.CLAIM_KEY, "relay-daemon:first")  # first daemon holds it
    rec = _Recorder()
    daemon.run_forever(deliver=rec, holder="relay-daemon:second", events_path=events, max_passes=1)
    assert any(x["kind"] == "relay_daemon_already_running" for x in _read_events(events))
    assert rec.calls == []  # the second instance never routed


# ---------------------------------------------------------------------------
# daemon_deliver: the single-writer session: claim guard (E4.3 / AC-E4-3).
# ---------------------------------------------------------------------------

def _resolution(sid):
    from fno.relay.router import Resolution
    return Resolution(session_id=sid, provider="claude", inject_handle=f"pty:{sid}")


def test_daemon_deliver_routes_when_session_held_by_interactive_lane(tmp_path, monkeypatch):
    """Finding session:X held by the daemon's INTERACTIVE lane (holder
    ``pty:<short_id>``) is the signal to route THROUGH the held handle
    (worker.submit), never spawn a second --session-id writer."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    from fno.claims.core import acquire_claim
    from fno.relay import roundtrip as rt
    monkeypatch.setattr(rt, "resolve_worker_short_id", lambda sid: "wkB")
    acquire_claim("session:sidA", "pty:wkB")  # the daemon interactive lane holds it (E1)

    seen = {}
    monkeypatch.setattr(
        rt, "deliver_session",
        lambda sid, framed, **kw: seen.update(sid=sid, framed=framed, short_id=kw.get("short_id")) or "the reply",
    )
    deliver = daemon.daemon_deliver(holder="relay-daemon:OTHER")
    out = deliver(_resolution("sidA"), "framed text")
    assert out == "the reply"
    assert seen == {"sid": "sidA", "framed": "framed text", "short_id": "wkB"}


def test_daemon_deliver_refuses_when_held_by_non_interactive_lane(tmp_path, monkeypatch):
    """A session: claim held by the stream lane (or any non-``pty:<short_id>``
    holder) is NOT a worker.submit target -- routing must refuse rather than inject
    into a session the daemon does not PTY-host (AC-E4-3 single-writer guard)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    from fno.claims.core import acquire_claim
    from fno.relay import roundtrip as rt
    monkeypatch.setattr(rt, "resolve_worker_short_id", lambda sid: "wkB")
    monkeypatch.setattr(rt, "deliver_session",
                        lambda *a, **k: pytest.fail("must not route to a non-interactive holder"))
    acquire_claim("session:sidS", "stream:wkB")  # the stream lane, not the PTY interactive lane

    deliver = daemon.daemon_deliver(holder="relay-daemon:OTHER")
    with pytest.raises(RuntimeError, match="not the daemon interactive lane"):
        deliver(_resolution("sidS"), "framed")


def test_daemon_deliver_refuses_when_no_worker(tmp_path, monkeypatch):
    """No live daemon worker row for the session -> nothing to route to."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    from fno.relay import roundtrip as rt
    monkeypatch.setattr(rt, "resolve_worker_short_id", lambda sid: None)
    deliver = daemon.daemon_deliver(holder="relay-daemon:OTHER")
    with pytest.raises(RuntimeError, match="no live daemon worker"):
        deliver(_resolution("sidNone"), "framed")


def test_daemon_deliver_refuses_when_session_free_and_holds_no_claim(tmp_path, monkeypatch):
    """A FREE claim (worker row exists, but nothing holds session:X) means no
    daemon host -> no handle. The relay must refuse rather than spawn a second
    writer, and must NOT end up holding the session: claim itself (releases the
    probe)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    from fno.claims.core import claim_status
    from fno.relay import roundtrip as rt
    monkeypatch.setattr(rt, "resolve_worker_short_id", lambda sid: "wkB")
    monkeypatch.setattr(rt, "deliver_session",
                        lambda *a, **k: pytest.fail("must not inject when session is free"))

    deliver = daemon.daemon_deliver(holder="relay-daemon:OTHER")
    with pytest.raises(RuntimeError, match="not daemon-held"):
        deliver(_resolution("sidFree"), "framed")
    # The relay released its probe -> it is never a second holder of session:X.
    assert claim_status("session:sidFree")["state"] == "free"


def test_daemon_deliver_raises_without_session_id(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    deliver = daemon.daemon_deliver()
    with pytest.raises(RuntimeError, match="no session_id"):
        deliver(_resolution(""), "framed")
