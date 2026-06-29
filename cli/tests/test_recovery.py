"""Unit tests for fno.recovery — Layer-2 session auto-recovery watchdog (x-f47c).

The load-bearing part is ``classify`` (the idle-but-incomplete predicate); the
rest of the suite covers the registry∩live-bg-session join, the per-session
nudge cap, and the one-event-per-decision contract. Every I/O dependency in
``recovery_sweep`` is injectable so these run without a live claude / filesystem.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone

import pytest

from fno import recovery


def _now() -> datetime:
    return datetime(2026, 6, 29, 20, 0, 0, tzinfo=timezone.utc)


def _iso(dt: datetime) -> str:
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


# ---------------------------------------------------------------------------
# classify — the predicate (AC1 nudge, AC2 needs-input, AC3 done)
# ---------------------------------------------------------------------------

class TestClassify:
    def test_needs_input_never_nudged_even_when_stale(self):
        # AC2-EDGE: a needs-input session is waiting on a human, not stalled.
        old = _iso(_now() - timedelta(hours=1))
        assert recovery.classify("needs-input", old, _now(), 300, False) == recovery.SKIP_NEEDS_INPUT

    @pytest.mark.parametrize("state", ["done", "completed", "failed"])
    def test_terminal_states_skipped(self, state):
        # AC3-EDGE: a clean terminal is never re-nudged.
        old = _iso(_now() - timedelta(hours=1))
        assert recovery.classify(state, old, _now(), 300, False) == recovery.SKIP_TERMINAL

    def test_promise_present_skipped(self):
        # AC3 / AC5: <promise> emitted -> done, never nudge, even if state stale.
        old = _iso(_now() - timedelta(hours=1))
        assert recovery.classify("running", old, _now(), 300, True) == recovery.SKIP_DONE

    def test_running_and_fresh_is_not_stale(self):
        fresh = _iso(_now() - timedelta(seconds=30))
        assert recovery.classify("running", fresh, _now(), 300, False) == recovery.NOT_STALE

    def test_running_and_stale_nudges(self):
        # AC1-HP: idle past the threshold, work incomplete -> nudge.
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify("running", stale, _now(), 300, False) == recovery.NUDGE

    def test_empty_or_unknown_state_stale_nudges(self):
        # A clean connection-close leaves state at the last value (often "running"
        # or empty); freshness is what distinguishes wedged from working.
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify("", stale, _now(), 300, False) == recovery.NUDGE

    def test_missing_updated_at_is_conservative(self):
        # Can't prove idleness -> do not nudge.
        assert recovery.classify("running", None, _now(), 300, False) == recovery.NOT_STALE

    def test_unparseable_updated_at_is_conservative(self):
        assert recovery.classify("running", "not-a-date", _now(), 300, False) == recovery.NOT_STALE


# ---------------------------------------------------------------------------
# candidate join — registry provenance ∩ live bg sessions (the AC invariant:
# only ever touch sessions footnote launched)
# ---------------------------------------------------------------------------

class _Entry:
    def __init__(self, provider, short_id):
        self.provider = provider
        self.claude_short_id = short_id


class _Locator:
    def __init__(self, short_id, sock, jobs_dir):
        self.short_id = short_id
        self.messaging_socket_path = sock
        self.jobs_dir = jobs_dir


class TestCandidateJoin:
    def test_only_footnote_claude_entries_with_live_sessions(self, tmp_path):
        entries = [
            _Entry("claude", "aaaa1111"),   # live bg session -> candidate
            _Entry("claude", "bbbb2222"),   # no live session -> dropped
            _Entry("codex", "cccc3333"),    # not claude -> dropped
            _Entry("claude", None),         # no short_id -> dropped
        ]
        live = {"aaaa1111": _Locator("aaaa1111", "/tmp/a.sock", tmp_path)}
        cands = recovery.iter_candidates(entries, locate_fn=lambda sid: live.get(sid))
        assert [c.short_id for c in cands] == ["aaaa1111"]

    def test_arbitrary_non_footnote_bg_session_never_a_candidate(self, tmp_path):
        # A live bg session that footnote never launched (not in the registry)
        # must never appear — invariant: only footnote-launched sessions.
        entries: list = []
        live = {"deadbeef": _Locator("deadbeef", "/tmp/x.sock", tmp_path)}
        cands = recovery.iter_candidates(entries, locate_fn=lambda sid: live.get(sid))
        assert cands == []


# ---------------------------------------------------------------------------
# recovery_sweep — nudge, cap (AC4), one-event-per-decision (invariant)
# ---------------------------------------------------------------------------

class _Cfg:
    enabled = True
    idle_threshold_seconds = 300
    max_nudges = 3


def _stale_candidate(tmp_path, short_id="aaaa1111", sock="/tmp/a.sock"):
    return recovery.Candidate(
        short_id=short_id,
        sock_path=sock,
        jobs_dir=tmp_path,
    )


class _Harness:
    """Collects emitted events and socket sends for assertions."""

    def __init__(self, state="running", updated_age_s=600, promise=False, sock_live=True):
        self.events: list[tuple[str, dict]] = []
        self.sends: list[tuple[str, str]] = []
        self._state = state
        self._updated = _iso(_now() - timedelta(seconds=updated_age_s))
        self._promise = promise
        self._sock_live = sock_live

    def emit(self, etype, data):
        self.events.append((etype, data))

    def read_state(self, jobs_dir):
        return recovery._SnapshotView(self._state, self._updated)

    def read_promise(self, jobs_dir):
        return self._promise

    def liveness(self, sock):
        return self._sock_live

    def send(self, sock, content, from_name):
        self.sends.append((sock, content))

    def event_types(self):
        return [e[0] for e in self.events]


class TestSweep:
    def test_stale_session_gets_one_nudge_and_one_event(self, tmp_path):
        h = _Harness()
        counts: dict = {}
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts=counts,
            emit=h.emit, read_state_fn=h.read_state, read_promise_fn=h.read_promise,
            liveness_fn=h.liveness, send_fn=h.send,
        )
        assert len(h.sends) == 1
        assert h.sends[0][1] == recovery.CONTINUE_MESSAGE
        assert h.event_types() == ["recovery_nudge"]
        assert counts["aaaa1111"] == 1

    def test_needs_input_emits_skipped_no_send(self, tmp_path):
        h = _Harness(state="needs-input")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state, read_promise_fn=h.read_promise,
            liveness_fn=h.liveness, send_fn=h.send,
        )
        assert h.sends == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "needs-input"

    def test_done_promise_no_send_no_event(self, tmp_path):
        h = _Harness(promise=True)
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state, read_promise_fn=h.read_promise,
            liveness_fn=h.liveness, send_fn=h.send,
        )
        assert h.sends == []
        assert h.events == []  # a done session is silent, not noise

    def test_cap_reached_emits_capped_once_no_send(self, tmp_path):
        # AC4-INV: at the cap, no further nudges; recovery_capped emitted.
        h = _Harness()
        counts = {"aaaa1111": 3}
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts=counts,
            emit=h.emit, read_state_fn=h.read_state, read_promise_fn=h.read_promise,
            liveness_fn=h.liveness, send_fn=h.send,
        )
        assert h.sends == []
        assert h.event_types() == ["recovery_capped"]
        assert counts["aaaa1111"] == 3  # not incremented past cap

    def test_capped_event_fires_only_once(self, tmp_path):
        # Two consecutive sweeps at the cap: capped event should fire on the
        # transition, not every tick, to avoid event spam.
        h = _Harness()
        counts = {"aaaa1111": 3}
        for _ in range(2):
            recovery.recovery_sweep(
                _now(), _Cfg(),
                candidates=[_stale_candidate(tmp_path)],
                counts=counts,
                emit=h.emit, read_state_fn=h.read_state, read_promise_fn=h.read_promise,
                liveness_fn=h.liveness, send_fn=h.send,
            )
        assert h.event_types().count("recovery_capped") == 1

    def test_dead_socket_skipped_not_nudged(self, tmp_path):
        # A suspended session (dead/null socket) is not reachable via the live
        # socket path; V1 skips it rather than treating it as dead work.
        h = _Harness(sock_live=False)
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state, read_promise_fn=h.read_promise,
            liveness_fn=h.liveness, send_fn=h.send,
        )
        assert h.sends == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "socket-unreachable"

    def test_send_failure_is_non_fatal(self, tmp_path):
        # AC: a re-nudge whose socket write fails must not crash the sweep.
        h = _Harness()

        def boom(sock, content, from_name):
            raise recovery._SendError("socket gone")

        # Should not raise.
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state, read_promise_fn=h.read_promise,
            liveness_fn=h.liveness, send_fn=boom,
        )
        assert "recovery_skipped" in h.event_types()


# ---------------------------------------------------------------------------
# run_recovery_sweep — the high-level entry (registry join -> sweep -> persist)
# ---------------------------------------------------------------------------

class TestRunRecoverySweep:
    def test_end_to_end_nudge_and_counts_persisted(self, tmp_path):
        h = _Harness()  # stale running session, live socket, no promise
        entries = [_Entry("claude", "aaaa1111"), _Entry("codex", "z")]
        live = {"aaaa1111": _Locator("aaaa1111", "/tmp/a.sock", tmp_path)}
        saved: dict = {}

        n = recovery.run_recovery_sweep(
            _Cfg(),
            emit=h.emit,
            now=_now(),
            registry_load=lambda: entries,
            locate_fn=lambda sid: live.get(sid),
            read_state_fn=h.read_state,
            read_promise_fn=h.read_promise,
            liveness_fn=h.liveness,
            send_fn=h.send,
            load_counts_fn=lambda: {},
            save_counts_fn=lambda c: saved.update(c),
        )

        assert n == 1
        assert h.event_types() == ["recovery_nudge"]
        assert saved["aaaa1111"] == 1

    def test_prunes_counts_for_vanished_sessions(self, tmp_path):
        h = _Harness()
        entries = [_Entry("claude", "aaaa1111")]
        live = {"aaaa1111": _Locator("aaaa1111", "/tmp/a.sock", tmp_path)}
        saved: dict = {}
        # "gone9999" is a leftover count for a session no longer live.
        prior = {"aaaa1111": 0, "gone9999": 2, "capped:gone9999": True}

        recovery.run_recovery_sweep(
            _Cfg(),
            emit=h.emit,
            now=_now(),
            registry_load=lambda: entries,
            locate_fn=lambda sid: live.get(sid),
            read_state_fn=h.read_state,
            read_promise_fn=h.read_promise,
            liveness_fn=h.liveness,
            send_fn=h.send,
            load_counts_fn=lambda: dict(prior),
            save_counts_fn=lambda c: saved.update(c),
        )

        assert "gone9999" not in saved
        assert "capped:gone9999" not in saved
        assert saved["aaaa1111"] == 1
