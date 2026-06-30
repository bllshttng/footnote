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
        assert recovery.classify("needs-input", old, _now(), 300) == recovery.SKIP_NEEDS_INPUT

    @pytest.mark.parametrize("state", ["done", "completed", "failed"])
    def test_terminal_states_skipped(self, state):
        # AC3-EDGE: a clean terminal is never re-nudged.
        old = _iso(_now() - timedelta(hours=1))
        assert recovery.classify(state, old, _now(), 300) == recovery.SKIP_TERMINAL

    def test_past_promise_does_not_force_skip(self):
        # codex P2: a <promise> is only the model's completion claim; loop-check
        # can reject it and the session keeps going. So a stale running session
        # is still a nudge target regardless of any past promise — "done" is the
        # terminal job state, not a transcript promise. (No promise input exists.)
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify("running", stale, _now(), 300) == recovery.NUDGE

    def test_naive_now_does_not_raise(self):
        # gemini medium: a timezone-naive now must not raise on subtraction.
        stale = _iso(_now() - timedelta(seconds=600))
        naive_now = _now().replace(tzinfo=None)
        assert recovery.classify("running", stale, naive_now, 300) == recovery.NUDGE

    def test_running_and_fresh_is_not_stale(self):
        fresh = _iso(_now() - timedelta(seconds=30))
        assert recovery.classify("running", fresh, _now(), 300) == recovery.NOT_STALE

    def test_running_and_stale_nudges(self):
        # AC1-HP: idle past the threshold, work incomplete -> nudge.
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify("running", stale, _now(), 300) == recovery.NUDGE

    def test_empty_or_unknown_state_stale_nudges(self):
        # A clean connection-close leaves state at the last value (often "running"
        # or empty); freshness is what distinguishes wedged from working.
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify("", stale, _now(), 300) == recovery.NUDGE

    def test_missing_updated_at_is_conservative(self):
        # Can't prove idleness -> do not nudge.
        assert recovery.classify("running", None, _now(), 300) == recovery.NOT_STALE

    def test_unparseable_updated_at_is_conservative(self):
        assert recovery.classify("running", "not-a-date", _now(), 300) == recovery.NOT_STALE


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

    def __init__(self, state="running", updated_age_s=600, sock_live=True):
        self.events: list[tuple[str, dict]] = []
        self.sends: list[tuple[str, str]] = []
        self._state = state
        self._updated = _iso(_now() - timedelta(seconds=updated_age_s))
        self._sock_live = sock_live

    def emit(self, etype, data):
        self.events.append((etype, data))

    def read_state(self, jobs_dir):
        return recovery._SnapshotView(self._state, self._updated)

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
            emit=h.emit, read_state_fn=h.read_state,
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
            emit=h.emit, read_state_fn=h.read_state,
            liveness_fn=h.liveness, send_fn=h.send,
        )
        assert h.sends == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "needs-input"

    def test_terminal_state_no_send_no_event(self, tmp_path):
        # A done/completed session is silent (no event), not noise. "Done" is the
        # terminal job state, the system's real completion authority.
        h = _Harness(state="completed")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            liveness_fn=h.liveness, send_fn=h.send,
        )
        assert h.sends == []
        assert h.events == []

    def test_cap_reached_emits_capped_once_no_send(self, tmp_path):
        # AC4-INV: at the cap, no further nudges; recovery_capped emitted.
        h = _Harness()
        counts = {"aaaa1111": 3}
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts=counts,
            emit=h.emit, read_state_fn=h.read_state,
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
                emit=h.emit, read_state_fn=h.read_state,
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
            emit=h.emit, read_state_fn=h.read_state,
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
            emit=h.emit, read_state_fn=h.read_state,
            liveness_fn=h.liveness, send_fn=boom,
        )
        assert "recovery_skipped" in h.event_types()


# ---------------------------------------------------------------------------
# run_recovery_sweep — the high-level entry (registry join -> sweep -> persist)
# ---------------------------------------------------------------------------

class TestRobustness:
    """Review-driven hardening: malformed inputs must degrade, never crash."""

    def test_config_recovery_non_mapping_degrades_to_defaults(self):
        # gemini high: `recovery: true` / null must not crash settings load.
        from fno.config import ConfigBlock

        assert ConfigBlock(recovery=True).recovery.enabled is True
        assert ConfigBlock(recovery=None).recovery.idle_threshold_seconds == 900
        assert ConfigBlock(recovery=["x"]).recovery.max_nudges == 3

    def test_load_counts_corrupt_utf8_returns_empty(self, tmp_path, monkeypatch):
        # gemini high: a non-UTF-8 counter file must not raise UnicodeDecodeError.
        p = tmp_path / "recovery-nudges.json"
        p.write_bytes(b"\xff\xfe not utf8")
        monkeypatch.setattr(recovery, "_counts_path", lambda: p)
        assert recovery.load_counts() == {}

    def test_load_counts_non_dict_json_returns_empty(self, tmp_path, monkeypatch):
        p = tmp_path / "recovery-nudges.json"
        p.write_text("[1, 2, 3]", encoding="utf-8")
        monkeypatch.setattr(recovery, "_counts_path", lambda: p)
        assert recovery.load_counts() == {}


class TestSafeReadState:
    def test_non_object_state_json_degrades_not_raises(self, tmp_path):
        # A2: a valid-JSON-but-non-object state.json (bare string) must not raise
        # AttributeError out of the sweep; it degrades to an empty (not-stale) view.
        (tmp_path / "state.json").write_text('"running"', encoding="utf-8")
        view = recovery._safe_read_state(tmp_path)
        assert view.state == ""
        assert view.updated_at is None

    def test_missing_state_json_degrades(self, tmp_path):
        view = recovery._safe_read_state(tmp_path)
        assert view.state == ""
        assert view.updated_at is None


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
            liveness_fn=h.liveness,
            send_fn=h.send,
            load_counts_fn=lambda: dict(prior),
            save_counts_fn=lambda c: saved.update(c),
        )

        assert "gone9999" not in saved
        assert "capped:gone9999" not in saved
        assert saved["aaaa1111"] == 1


# ---------------------------------------------------------------------------
# out-of-usage provider failover (x-7abe) — wire attempt_swap into the watchdog
# ---------------------------------------------------------------------------

class TestClassifySessionError:
    """classify_session_error reuses the shipped normalize() text rules."""

    def test_rate_limit_text_is_swap_class(self):
        err = recovery.classify_session_error("API Error: rate limit exceeded, retry later")
        assert err is not None
        assert err.triggers_swap is True

    def test_quota_text_is_swap_class(self):
        err = recovery.classify_session_error("Error: quota exceeded for this model")
        assert err is not None
        assert err.triggers_swap is True

    def test_connection_drop_is_not_swap_class(self):
        # AC2-FR: a clean connection-drop carries no quota/5xx marker, so it is
        # not a swap trigger — the watchdog nudges it exactly as x-f47c does.
        err = recovery.classify_session_error("API Error: Connection closed mid-response")
        assert err is None or err.triggers_swap is False

    def test_no_output_returns_none(self):
        assert recovery.classify_session_error(None) is None
        assert recovery.classify_session_error("") is None
        assert recovery.classify_session_error(123) is None  # non-str


class _FailoverHarness(_Harness):
    """A sweep harness with a controllable last-error and a fake failover_fn."""

    def __init__(self, output_result=None, outcome="swapped", **kw):
        super().__init__(**kw)
        self._output = output_result
        self._outcome = outcome
        self.failover_calls: list = []

    def read_state(self, jobs_dir):
        return recovery._SnapshotView(self._state, self._updated, self._output)

    def failover(self, candidate, err):
        self.failover_calls.append((candidate.short_id, err.error_class))
        return self._outcome


class TestFailoverSweep:
    def _run(self, h, tmp_path):
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            liveness_fn=h.liveness, send_fn=h.send,
            failover_fn=h.failover,
        )

    def test_swap_class_routes_to_failover_not_nudge(self, tmp_path):
        # AC1-FR: a quota-died bg session swaps + re-dispatches, never nudges.
        h = _FailoverHarness(output_result="API Error: rate limit exceeded", outcome="swapped")
        self._run(h, tmp_path)
        assert len(h.failover_calls) == 1
        assert h.sends == []                       # NOT nudged
        assert h.event_types() == ["failover_swapped"]

    def test_connection_drop_still_nudges(self, tmp_path):
        # AC2-FR: a clean connection-drop is unchanged — failover never called.
        h = _FailoverHarness(output_result="API Error: Connection closed mid-response")
        self._run(h, tmp_path)
        assert h.failover_calls == []
        assert h.sends and h.sends[0][1] == recovery.CONTINUE_MESSAGE
        assert h.event_types() == ["recovery_nudge"]

    def test_no_output_result_nudges(self, tmp_path):
        # No last-error text (the common idle case): unchanged nudge.
        h = _FailoverHarness(output_result=None)
        self._run(h, tmp_path)
        assert h.failover_calls == []
        assert h.event_types() == ["recovery_nudge"]

    def test_blocked_thrash_emits_blocked_no_nudge(self, tmp_path):
        # AC2-EDGE: storm-cap reached -> bounded stop, no nudge churn.
        h = _FailoverHarness(output_result="rate limit", outcome="blocked-thrash")
        self._run(h, tmp_path)
        assert h.sends == []
        assert h.event_types() == ["failover_blocked"]
        assert h.events[0][1]["reason"] == "blocked-thrash"

    def test_queue_exhausted_emits_blocked_no_nudge(self, tmp_path):
        # AC1-EDGE: no eligible alternate -> bounded stop, no nudge.
        h = _FailoverHarness(output_result="quota exceeded", outcome="queue-exhausted")
        self._run(h, tmp_path)
        assert h.sends == []
        assert h.event_types() == ["failover_blocked"]
        assert h.events[0][1]["reason"] == "queue-exhausted"

    def test_no_swap_outcome_falls_through_to_nudge(self, tmp_path):
        # Controller declined (NO_SWAP_NEEDED): defensive fall-through to nudge.
        h = _FailoverHarness(output_result="rate limit", outcome="no-swap")
        self._run(h, tmp_path)
        assert h.event_types() == ["recovery_nudge"]

    def test_failover_disabled_when_fn_absent(self, tmp_path):
        # Backward compat: no failover_fn -> swap-class error still nudges (today).
        h = _FailoverHarness(output_result="rate limit exceeded")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            liveness_fn=h.liveness, send_fn=h.send,
            # failover_fn omitted
        )
        assert h.failover_calls == []
        assert h.event_types() == ["recovery_nudge"]


class TestDefaultFailover:
    """The real failover_fn maps SwapDecision -> the sweep's outcome strings and
    only re-dispatches on SWAPPED. Controller + settings are monkeypatched so no
    real provider rotation / subprocess fires."""

    def _patch(self, monkeypatch, decision, new_provider="codex", redispatched=None):
        from fno.adapters.providers import failover as fo_mod
        from fno.adapters.providers import loader as loader_mod
        from fno.adapters.providers import dispatch as dispatch_mod

        class _Result:
            def __init__(self):
                self.decision = decision
                self.new_provider_id = new_provider

        class _Ctrl:
            def __init__(self, **kw):
                pass

            def attempt_swap(self, *, current_provider_id, error):
                return _Result()

        class _Snap:
            id = "claude"

        monkeypatch.setattr(fo_mod, "FailoverController", _Ctrl)
        monkeypatch.setattr(loader_mod, "read_active_provider_atomic", lambda **kw: _Snap())
        monkeypatch.setattr(dispatch_mod, "_default_settings_path", lambda: "/tmp/settings.yaml")
        if redispatched is not None:
            monkeypatch.setattr(
                recovery, "_redispatch",
                lambda cand, prov: redispatched.append((cand.short_id, prov)),
            )

    def test_swapped_redispatches_and_returns_swapped(self, monkeypatch, tmp_path):
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_provider="codex", redispatched=calls)
        cand = _stale_candidate(tmp_path)
        err = recovery.classify_session_error("rate limit exceeded")
        assert recovery._default_failover(cand, err) == "swapped"
        assert calls == [(cand.short_id, "codex")]   # re-dispatch fired with new provider

    def test_blocked_thrash_maps_and_no_redispatch(self, monkeypatch, tmp_path):
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.BLOCKED_THRASH, redispatched=calls)
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "blocked-thrash"
        assert calls == []

    def test_queue_exhausted_maps(self, monkeypatch, tmp_path):
        from fno.adapters.providers.failover import SwapDecision

        self._patch(monkeypatch, SwapDecision.QUEUE_EXHAUSTED)
        err = recovery.classify_session_error("quota exceeded")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "queue-exhausted"

    def test_controller_error_degrades_to_no_swap(self, monkeypatch, tmp_path):
        from fno.adapters.providers import dispatch as dispatch_mod

        def boom():
            raise RuntimeError("settings unreadable")

        monkeypatch.setattr(dispatch_mod, "_default_settings_path", boom)
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "no-swap"


class TestNodeIdFromWorktree:
    def test_reads_graph_node_id(self, tmp_path):
        fno_dir = tmp_path / ".fno"
        fno_dir.mkdir()
        (fno_dir / "target-state.md").write_text(
            'session_id: abc\ngraph_node_id: x-7abe\nprovider: claude\n', encoding="utf-8")
        assert recovery._node_id_from_worktree(str(tmp_path)) == "x-7abe"

    def test_quoted_value_is_unquoted(self, tmp_path):
        fno_dir = tmp_path / ".fno"
        fno_dir.mkdir()
        (fno_dir / "target-state.md").write_text(
            'graph_node_id: "x-1234"\n', encoding="utf-8")
        assert recovery._node_id_from_worktree(str(tmp_path)) == "x-1234"

    def test_missing_file_returns_none(self, tmp_path):
        assert recovery._node_id_from_worktree(str(tmp_path)) is None
