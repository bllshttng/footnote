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
        assert h.events[0][1]["redispatched"] is True   # honest: worker started

    def test_rotated_no_worker_emits_swapped_then_nudges(self, tmp_path):
        # codex P1: the swap rotated the provider but no replacement worker
        # started (non-claude target / spawn failed). The event must report
        # redispatched=False (no phantom redispatch) AND the session still gets
        # the bounded nudge rather than being left dead-and-unnudged.
        h = _FailoverHarness(output_result="rate limit", outcome="rotated-no-worker")
        self._run(h, tmp_path)
        assert h.event_types() == ["failover_swapped", "recovery_nudge"]
        assert h.events[0][1]["redispatched"] is False
        assert len(h.sends) == 1

    def test_one_swap_per_tick(self, tmp_path):
        # codex P2: a swap mutates the GLOBAL active provider, so only one
        # rotation may fire per tick; the second stale session nudges this tick
        # (reconsidered next tick against the settled provider).
        h = _FailoverHarness(output_result="rate limit", outcome="swapped")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[
                _stale_candidate(tmp_path, short_id="aaaa1111"),
                _stale_candidate(tmp_path, short_id="bbbb2222", sock="/tmp/b.sock"),
            ],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            liveness_fn=h.liveness, send_fn=h.send,
            failover_fn=h.failover,
        )
        assert len(h.failover_calls) == 1            # only the first swaps
        assert h.failover_calls[0][0] == "aaaa1111"
        assert h.event_types() == ["failover_swapped", "recovery_nudge"]
        assert h.sends[0][0] == "/tmp/b.sock"        # the second nudged

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

    def test_notified_emits_swapped_and_does_not_nudge(self, tmp_path):
        # US4/US5 (AC3-FR + AC4-FR "dead one not also nudged"): a revival that
        # degraded to the manual-resume notification rotated the provider but
        # started no worker, so it reports redispatched=False and must NOT also
        # nudge the exhausted session.
        h = _FailoverHarness(output_result="usage limit reached", outcome="notified")
        self._run(h, tmp_path)
        assert h.sends == []                           # NOT nudged
        assert h.event_types() == ["failover_swapped"]
        assert h.events[0][1]["redispatched"] is False

    def test_queue_exhausted_falls_through_to_nudge(self, tmp_path):
        # AC1-EDGE (watchdog reading): no eligible alternate -> nothing to swap
        # to, so fall back to the bounded x-f47c nudge (the rate-limit window may
        # clear). Strictly no worse than the pre-failover watchdog for the common
        # single-provider case; the per-session cap stops it spinning.
        h = _FailoverHarness(output_result="quota exceeded", outcome="queue-exhausted")
        self._run(h, tmp_path)
        assert len(h.sends) == 1
        assert h.sends[0][1] == recovery.CONTINUE_MESSAGE
        assert h.event_types() == ["recovery_nudge"]

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
    """The real failover_fn maps SwapDecision -> the sweep's outcome strings.
    It re-reads the active provider's cli KIND after a swap and only
    bg-redispatches when that kind is claude. Controller + settings are
    monkeypatched so no real provider rotation / subprocess fires."""

    def _patch(self, monkeypatch, decision, new_cli="claude", redispatch_result=None,
               calls=None, auth=None):
        from fno.adapters.providers import failover as fo_mod
        from fno.adapters.providers import loader as loader_mod
        from fno.adapters.providers import dispatch as dispatch_mod

        class _Result:
            def __init__(self):
                self.decision = decision
                self.new_provider_id = "claude-secondary"  # a RECORD id, not a kind

        class _Ctrl:
            def __init__(self, **kw):
                pass

            def attempt_swap(self, *, current_provider_id, error):
                return _Result()

        class _Snap:
            # Read AFTER the swap, so it is the swapped-to record: .id is the new
            # active id, .cli its kind, .auth its auth strategy (US3: "managed"
            # needs a credential materialization into the shared slot pre-redispatch).
            id = "claude-secondary"
            cli = new_cli
        _Snap.auth = auth

        monkeypatch.setattr(fo_mod, "FailoverController", _Ctrl)
        monkeypatch.setattr(loader_mod, "read_active_provider_atomic", lambda **kw: _Snap())
        monkeypatch.setattr(dispatch_mod, "_default_settings_path", lambda: "/tmp/settings.yaml")
        if redispatch_result is not None:
            def _fake_redispatch(cand, *, pre_spawn=None):
                if calls is not None:
                    calls.append(cand.short_id)
                # Honor the real contract: pre_spawn (managed materialize) runs
                # inside _redispatch; a False result aborts the respawn.
                if pre_spawn is not None and not pre_spawn():
                    return False
                return redispatch_result
            monkeypatch.setattr(recovery, "_redispatch", _fake_redispatch)

    def test_swapped_claude_redispatch_ok_returns_swapped(self, monkeypatch, tmp_path):
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls)
        cand = _stale_candidate(tmp_path)
        err = recovery.classify_session_error("rate limit exceeded")
        assert recovery._default_failover(cand, err) == "swapped"
        assert calls == [cand.short_id]              # redispatch was attempted

    def test_swapped_nonclaude_is_rotated_no_worker(self, monkeypatch, tmp_path):
        # codex P1: a swap onto a non-claude provider cannot bg-redispatch a
        # /target, so no worker starts — and _redispatch must not even be called.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="codex",
                    redispatch_result=True, calls=calls)
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "rotated-no-worker"
        assert calls == []                           # never tried to bg-spawn on codex

    def test_swapped_claude_redispatch_fails_is_rotated_no_worker(self, monkeypatch, tmp_path):
        # The swap landed on claude but the spawn failed (returncode != 0).
        from fno.adapters.providers.failover import SwapDecision

        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude", redispatch_result=False)
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "rotated-no-worker"

    def test_blocked_thrash_maps(self, monkeypatch, tmp_path):
        from fno.adapters.providers.failover import SwapDecision

        self._patch(monkeypatch, SwapDecision.BLOCKED_THRASH)
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "blocked-thrash"

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

    # --- US3: managed-account materialization hook (auto-switch) ---------------

    def test_managed_swap_materializes_then_redispatches(self, monkeypatch, tmp_path):
        # AC3-HP: swap lands on an armed managed claude record -> _redispatch runs
        # with a pre_spawn that materializes the account (after the stop, before
        # the spawn). Returns "swapped".
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="managed")
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: True)
        mat: list = []
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: mat.append(rid) or True)
        err = recovery.classify_session_error("usage limit reached")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "swapped"
        assert mat == ["claude-secondary"]   # materialized the swapped-to record
        assert calls == [_stale_candidate(tmp_path).short_id]  # via _redispatch

    def test_managed_materialize_fails_is_rotated_no_worker(self, monkeypatch, tmp_path):
        # Armed, but a live-pin defer / store error makes materialize (the
        # pre_spawn) return False -> _redispatch aborts the respawn -> nudge.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="managed")
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: True)
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: False)
        err = recovery.classify_session_error("usage limit reached")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "rotated-no-worker"
        assert calls == [_stale_candidate(tmp_path).short_id]  # _redispatch ran (stopped worker)

    def test_managed_auto_switch_off_leaves_worker_alive(self, monkeypatch, tmp_path):
        # Disarmed managed swap: never stop the worker (never _redispatch); leave
        # it alive for the bounded nudge. codex P1 ordering guard: the exhausted
        # worker must not be stopped for a switch that will not happen.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="managed")
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: False)
        mat = {"called": False}
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: mat.__setitem__("called", True) or True)
        err = recovery.classify_session_error("usage limit reached")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "rotated-no-worker"
        assert calls == []                    # never stopped/redispatched the worker
        assert mat["called"] is False         # never materialized

    def test_oauth_dir_swap_skips_materialize(self, monkeypatch, tmp_path):
        # An oauth_dir claude record needs no materialization (env-var switch at
        # spawn); _redispatch runs with no pre_spawn hook.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="oauth_dir")
        called = {"mat": False, "gate": False}
        monkeypatch.setattr(recovery, "_auto_switch_enabled",
                            lambda repo_root=None: called.__setitem__("gate", True) or True)
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: called.__setitem__("mat", True) or True)
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "swapped"
        assert called["mat"] is False         # no materialize for oauth_dir
        assert called["gate"] is False        # auto_switch gate not consulted either
        assert calls == [_stale_candidate(tmp_path).short_id]

    # --- US4: node-bound vs node-less routing ---------------------------------

    def test_node_less_thread_routes_to_revival(self, monkeypatch, tmp_path):
        # US4: a claude swap whose candidate has a live cwd but NO target-state
        # node routes to _revive_bg_thread (resume the transcript), NOT _redispatch.
        from fno.adapters.providers.failover import SwapDecision

        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude", auth="oauth_dir")
        monkeypatch.setattr(recovery, "_node_id_from_worktree", lambda cwd: None)
        seen: dict = {}
        monkeypatch.setattr(
            recovery, "_revive_bg_thread",
            lambda cand, snap, repo_root, *, managed: seen.update(
                short=cand.short_id, root=repo_root, managed=managed) or "swapped")
        cand = recovery.Candidate(short_id="cccc3333", sock_path="/tmp/c.sock",
                                  jobs_dir=tmp_path, cwd=str(tmp_path), name="thread-w")
        err = recovery.classify_session_error("usage limit reached")
        assert recovery._default_failover(cand, err) == "swapped"
        assert seen == {"short": "cccc3333", "root": str(tmp_path), "managed": False}

    def test_node_bound_worker_skips_revival(self, monkeypatch, tmp_path):
        # A candidate whose cwd DOES resolve a node stays on the _redispatch path.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="oauth_dir")
        monkeypatch.setattr(recovery, "_node_id_from_worktree", lambda cwd: "x-node")
        monkeypatch.setattr(
            recovery, "_revive_bg_thread",
            lambda *a, **k: pytest.fail("revival must not run for a node-bound worker"))
        cand = recovery.Candidate(short_id="dddd4444", sock_path="/tmp/d.sock",
                                  jobs_dir=tmp_path, cwd=str(tmp_path), name="node-w")
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(cand, err) == "swapped"
        assert calls == ["dddd4444"]


class TestMaterializeManagedSwitch:
    """US3: the managed-account materialize gate (config.providers.auto_switch)."""

    def _fake_config(self, auto_switch, record):
        class _Cfg:
            pass
        c = _Cfg()
        c.auto_switch = auto_switch
        c.by_id = {record.id: record} if record is not None else {}
        return c

    def _managed_record(self):
        from fno.adapters.providers.model import ProviderRecord
        return ProviderRecord(id="claude-secondary", name="B", cli="claude", auth="managed")

    def test_auto_switch_off_returns_false_without_touching_slot(self, monkeypatch):
        from fno.adapters.providers import loader as loader_mod
        from fno.adapters.providers import managed as managed_mod

        rec = self._managed_record()
        monkeypatch.setattr(loader_mod, "load_providers",
                            lambda *a, **k: self._fake_config(False, rec))
        switched = {"called": False}
        monkeypatch.setattr(managed_mod, "switch",
                            lambda *a, **k: switched.__setitem__("called", True))
        assert recovery._materialize_managed_switch("claude-secondary") is False
        assert switched["called"] is False   # disarmed: slot never mutated

    def test_auto_switch_on_materializes(self, monkeypatch):
        from fno.adapters.providers import loader as loader_mod
        from fno.adapters.providers import managed as managed_mod

        rec = self._managed_record()
        monkeypatch.setattr(loader_mod, "load_providers",
                            lambda *a, **k: self._fake_config(True, rec))
        seen: dict = {}
        monkeypatch.setattr(managed_mod, "switch",
                            lambda r, **k: seen.update(id=r.id, by_id=k.get("by_id")))
        assert recovery._materialize_managed_switch("claude-secondary") is True
        assert seen["id"] == "claude-secondary"

    def test_switch_deferred_returns_false(self, monkeypatch):
        from fno.adapters.providers import loader as loader_mod
        from fno.adapters.providers import managed as managed_mod

        rec = self._managed_record()
        monkeypatch.setattr(loader_mod, "load_providers",
                            lambda *a, **k: self._fake_config(True, rec))

        def _defer(*a, **k):
            raise managed_mod.SwitchDeferred("slot pinned by pid 42")
        monkeypatch.setattr(managed_mod, "switch", _defer)
        assert recovery._materialize_managed_switch("claude-secondary") is False


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


class TestNodeIsDone:
    """x-370f AC1-EDGE: the already-done guard reads node status, fail-open."""

    def _patch_graph(self, monkeypatch, entries):
        from fno.graph import load as gl
        monkeypatch.setattr(gl, "load_graph", lambda *a, **k: entries)

    def test_true_when_done(self, monkeypatch):
        self._patch_graph(monkeypatch, [{"id": "x-370f", "_status": "done"}])
        assert recovery._node_is_done("x-370f") is True

    def test_false_when_not_done(self, monkeypatch):
        self._patch_graph(monkeypatch, [{"id": "x-370f", "_status": "claimed"}])
        assert recovery._node_is_done("x-370f") is False

    def test_false_when_absent(self, monkeypatch):
        self._patch_graph(monkeypatch, [{"id": "x-other", "_status": "done"}])
        assert recovery._node_is_done("x-370f") is False

    def test_load_error_degrades_to_false(self, monkeypatch):
        from fno.graph import load as gl

        def boom(*a, **k):
            raise RuntimeError("corrupt graph")

        monkeypatch.setattr(gl, "load_graph", boom)
        assert recovery._node_is_done("x-370f") is False


class TestRedispatch:
    """x-370f residual 1: failover respawn frees the dead session's claim via
    ``fno claim force-release`` before spawning, skips an already-done node, and
    bails to the nudge (False) when the claim cannot be freed."""

    def _cand(self):
        return recovery.Candidate(
            short_id="aaaa1111", sock_path="/tmp/a.sock", jobs_dir=None,
            cwd="/wt/x-370f", name="dead-worker",
        )

    def _patch_resolve(self, monkeypatch, node="x-370f", done=False):
        monkeypatch.setattr(recovery, "_node_id_from_worktree", lambda cwd: node)
        monkeypatch.setattr(recovery, "_node_is_done", lambda n: done)

    def _patch_run(self, monkeypatch, *, stop_rc=0, force_release_rc=0, spawn_rc=0):
        """Stub subprocess.run; record the (markered) calls for assertions."""
        from types import SimpleNamespace
        import subprocess as sp

        calls = []

        def fake_run(cmd, **kw):
            calls.append(cmd)
            if cmd[:3] == ["fno-py", "agents", "stop"]:
                return SimpleNamespace(returncode=stop_rc)
            if cmd[:3] == ["fno-py", "claim", "force-release"]:
                return SimpleNamespace(returncode=force_release_rc)
            if cmd[:3] == ["fno-py", "agents", "spawn"]:
                return SimpleNamespace(returncode=spawn_rc)
            return SimpleNamespace(returncode=0)

        monkeypatch.setattr(sp, "run", fake_run)
        return calls

    @staticmethod
    def _index_of(calls, marker):
        return next((i for i, c in enumerate(calls) if c[:3] == marker), None)

    def test_force_release_before_spawn_happy_path(self, monkeypatch):
        # AC1-HP: stop → force-release node:<id> → canonical claude bg spawn.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(self._cand()) is True

        fr = self._index_of(calls, ["fno-py", "claim", "force-release"])
        spawn = self._index_of(calls, ["fno-py", "agents", "spawn"])
        assert fr is not None and spawn is not None
        assert fr < spawn                      # claim freed strictly before spawn
        assert "node:x-370f" in calls[fr]      # exact claim key
        assert "-R" in calls[fr]               # required audit reason supplied
        spawn_cmd = calls[spawn]
        assert "--provider" in spawn_cmd and "claude" in spawn_cmd
        assert "--substrate" in spawn_cmd and "bg" in spawn_cmd
        assert "--cwd" in spawn_cmd and "/wt/x-370f" in spawn_cmd

    def test_stop_failure_skips_force_release_and_spawn(self, monkeypatch):
        # codex P2: a non-zero `fno agents stop` means the worker may still be
        # live; force-releasing its claim + spawning would create two workers on
        # one node. Bail to the nudge (no force-release, no spawn) → False.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch, stop_rc=1)
        assert recovery._redispatch(self._cand()) is False
        assert self._index_of(calls, ["fno-py", "claim", "force-release"]) is None
        assert self._index_of(calls, ["fno-py", "agents", "spawn"]) is None

    def test_force_release_failure_skips_spawn(self, monkeypatch):
        # AC1-ERR: force-release non-zero → no spawn, False so the caller nudges.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch, force_release_rc=1)
        assert recovery._redispatch(self._cand()) is False
        assert self._index_of(calls, ["fno-py", "agents", "spawn"]) is None

    def test_done_node_not_redispatched(self, monkeypatch):
        # AC1-EDGE: already-done node → no stop/force-release/spawn at all.
        self._patch_resolve(monkeypatch, done=True)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(self._cand()) is False
        assert calls == []

    def test_spawn_failure_returns_false(self, monkeypatch):
        # Spawn exit non-zero (existing contract) → False so the caller nudges.
        self._patch_resolve(monkeypatch)
        self._patch_run(monkeypatch, spawn_rc=1)
        assert recovery._redispatch(self._cand()) is False

    def test_spawn_failure_releases_lane_slot(self, monkeypatch):
        # Parallel G4: no replacement worker → the dead lane's dispatch-time
        # slot is freed so lane-fill can re-select the node before the TTL.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch, spawn_rc=1)
        assert recovery._redispatch(self._cand()) is False
        lr = self._index_of(calls, ["fno-py", "claim", "lane-release"])
        assert lr is not None
        assert "x-370f" in calls[lr]

    def test_successful_respawn_keeps_lane_slot(self, monkeypatch):
        # A respawned worker reconciles the existing slot at target init; the
        # sweep must not release it out from under the new lane.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(self._cand()) is True
        assert self._index_of(calls, ["fno-py", "claim", "lane-release"]) is None

    def test_unresolvable_node_returns_false(self, monkeypatch):
        # No node id in the worktree manifest → nothing to re-dispatch.
        monkeypatch.setattr(recovery, "_node_id_from_worktree", lambda cwd: None)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(self._cand()) is False
        assert calls == []

    def test_pre_spawn_runs_after_stop_release_before_spawn(self, monkeypatch):
        # codex P1 (US3): the managed materialize (pre_spawn) must run AFTER the
        # worker is stopped (so it no longer pins the slot) and its claim freed,
        # and BEFORE the replacement spawns (it must read the new account's creds).
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(
            self._cand(), pre_spawn=lambda: calls.append(["MATERIALIZE"]) or True) is True
        stop = self._index_of(calls, ["fno-py", "agents", "stop"])
        fr = self._index_of(calls, ["fno-py", "claim", "force-release"])
        mat = next((i for i, c in enumerate(calls) if c == ["MATERIALIZE"]), None)
        spawn = self._index_of(calls, ["fno-py", "agents", "spawn"])
        assert None not in (stop, fr, mat, spawn)
        assert stop < mat < spawn      # materialize between the stop and the spawn
        assert fr < mat                # and after the claim is freed

    def test_pre_spawn_false_aborts_spawn_and_frees_lane(self, monkeypatch):
        # A False pre_spawn (materialize deferred/failed) → no spawn, lane slot
        # freed so the node re-dispatches fresh, and False so the caller nudges.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(self._cand(), pre_spawn=lambda: False) is False
        assert self._index_of(calls, ["fno-py", "agents", "spawn"]) is None
        assert self._index_of(calls, ["fno-py", "claim", "lane-release"]) is not None


class TestReviveBgThread:
    """US4/US5: node-less bg-thread revival - resume under the new account, or
    degrade to the manual-resume notify path (never a resume against a missing
    transcript)."""

    def _cand(self, tmp_path):
        return recovery.Candidate(short_id="eeee5555", sock_path="/tmp/e.sock",
                                  jobs_dir=tmp_path, cwd=str(tmp_path), name="thread-w")

    def _snap(self, auth="oauth_dir"):
        from types import SimpleNamespace
        return SimpleNamespace(id="claude-secondary", cli="claude", auth=auth)

    def test_no_uuid_falls_through_to_nudge(self, monkeypatch, tmp_path):
        # No resolvable session id: can't resume or build a command -> bounded nudge.
        monkeypatch.setattr(recovery, "_resolve_session_uuid", lambda s: None)
        assert recovery._revive_bg_thread(
            self._cand(tmp_path), self._snap(), str(tmp_path), managed=False
        ) == "rotated-no-worker"

    def test_visible_transcript_resumes_returns_swapped(self, monkeypatch, tmp_path):
        # AC4-FR: transcript visible -> respawn resuming the uuid; not notified.
        monkeypatch.setattr(recovery, "_resolve_session_uuid", lambda s: "U-1")
        monkeypatch.setattr(recovery, "_transcript_visible", lambda u, d: True)
        seen: dict = {}
        monkeypatch.setattr(recovery, "_respawn_bg_resume",
                            lambda cand, uuid, *, pre_spawn=None: seen.update(uuid=uuid) or True)
        notified = {"n": False}
        monkeypatch.setattr(recovery, "_notify_manual_resume",
                            lambda *a: notified.__setitem__("n", True))
        assert recovery._revive_bg_thread(
            self._cand(tmp_path), self._snap(), str(tmp_path), managed=False
        ) == "swapped"
        assert seen["uuid"] == "U-1"
        assert notified["n"] is False

    def test_unshared_transcript_notifies(self, monkeypatch, tmp_path):
        # AC3-FR: transcript not visible to the new account -> notify, never resume.
        monkeypatch.setattr(recovery, "_resolve_session_uuid", lambda s: "U-1")
        monkeypatch.setattr(recovery, "_transcript_visible", lambda u, d: False)
        monkeypatch.setattr(recovery, "_respawn_bg_resume",
                            lambda *a, **k: pytest.fail("must not resume a missing transcript"))
        seen: dict = {}
        monkeypatch.setattr(recovery, "_notify_manual_resume",
                            lambda cand, snap, uuid: seen.update(uuid=uuid))
        assert recovery._revive_bg_thread(
            self._cand(tmp_path), self._snap(), str(tmp_path), managed=False
        ) == "notified"
        assert seen["uuid"] == "U-1"

    def test_respawn_failure_notifies(self, monkeypatch, tmp_path):
        # Visible but the respawn missed (stop/spawn failure): notify, don't nudge.
        monkeypatch.setattr(recovery, "_resolve_session_uuid", lambda s: "U-1")
        monkeypatch.setattr(recovery, "_transcript_visible", lambda u, d: True)
        monkeypatch.setattr(recovery, "_respawn_bg_resume", lambda *a, **k: False)
        notified = {"n": False}
        monkeypatch.setattr(recovery, "_notify_manual_resume",
                            lambda *a: notified.__setitem__("n", True))
        assert recovery._revive_bg_thread(
            self._cand(tmp_path), self._snap(), str(tmp_path), managed=False
        ) == "notified"
        assert notified["n"] is True

    def test_managed_disarmed_falls_through_to_nudge(self, monkeypatch, tmp_path):
        # A disarmed managed swap never materializes the slot, so a resume would
        # land on the exhausted account: fall to the nudge, never reach visibility.
        monkeypatch.setattr(recovery, "_resolve_session_uuid", lambda s: "U-1")
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: False)
        monkeypatch.setattr(recovery, "_transcript_visible",
                            lambda u, d: pytest.fail("disarmed managed must not reach visibility"))
        assert recovery._revive_bg_thread(
            self._cand(tmp_path), self._snap(auth="managed"), str(tmp_path), managed=True
        ) == "rotated-no-worker"

    def test_managed_visible_materializes_via_pre_spawn(self, monkeypatch, tmp_path):
        # A managed revival threads the materialize into _respawn_bg_resume's
        # pre_spawn (stop -> materialize -> spawn), mirroring _redispatch.
        monkeypatch.setattr(recovery, "_resolve_session_uuid", lambda s: "U-1")
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: True)
        monkeypatch.setattr(recovery, "_transcript_visible", lambda u, d: True)
        mat: list = []
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: mat.append(rid) or True)
        captured: dict = {}

        def _fake_respawn(cand, uuid, *, pre_spawn=None):
            captured["pre_spawn_result"] = pre_spawn() if pre_spawn else None
            return True
        monkeypatch.setattr(recovery, "_respawn_bg_resume", _fake_respawn)
        assert recovery._revive_bg_thread(
            self._cand(tmp_path), self._snap(auth="managed"), str(tmp_path), managed=True
        ) == "swapped"
        assert captured["pre_spawn_result"] is True   # materialize ran as pre_spawn
        assert mat == ["claude-secondary"]


class TestRespawnBgResume:
    """The node-less resume respawn: stop -> pre_spawn -> ``claude --bg --resume``."""

    def _cand(self):
        return recovery.Candidate(short_id="ffff6666", sock_path="/tmp/f.sock",
                                  jobs_dir=None, cwd="/wt/thread", name="thread-w")

    def _patch_run(self, monkeypatch, *, stop_rc=0, spawn_rc=0):
        from types import SimpleNamespace
        import subprocess as sp
        calls = []

        def fake_run(cmd, **kw):
            calls.append(cmd)
            if cmd[:3] == ["fno-py", "agents", "stop"]:
                return SimpleNamespace(returncode=stop_rc)
            if cmd[:3] == ["fno-py", "agents", "spawn"]:
                return SimpleNamespace(returncode=spawn_rc)
            return SimpleNamespace(returncode=0)
        monkeypatch.setattr(sp, "run", fake_run)
        return calls

    def test_happy_path_builds_resume_spawn(self, monkeypatch):
        calls = self._patch_run(monkeypatch)
        assert recovery._respawn_bg_resume(self._cand(), "U-abc") is True
        spawn = next(c for c in calls if c[:3] == ["fno-py", "agents", "spawn"])
        assert "--substrate" in spawn and "bg" in spawn
        assert "--resume" in spawn and "U-abc" in spawn
        assert "--cwd" in spawn and "/wt/thread" in spawn
        assert spawn[-1] == recovery.CONTINUE_MESSAGE   # seeds the continue turn

    def test_stop_failure_skips_spawn(self, monkeypatch):
        # A non-zero stop means the thread may be live; a second --resume would
        # double it. Bail (the caller notifies).
        calls = self._patch_run(monkeypatch, stop_rc=1)
        assert recovery._respawn_bg_resume(self._cand(), "U-abc") is False
        assert not any(c[:3] == ["fno-py", "agents", "spawn"] for c in calls)

    def test_pre_spawn_false_skips_spawn(self, monkeypatch):
        # A False pre_spawn (managed materialize deferred/failed) aborts the spawn.
        calls = self._patch_run(monkeypatch)
        assert recovery._respawn_bg_resume(self._cand(), "U-abc",
                                           pre_spawn=lambda: False) is False
        assert not any(c[:3] == ["fno-py", "agents", "spawn"] for c in calls)

    def test_spawn_failure_returns_false(self, monkeypatch):
        self._patch_run(monkeypatch, spawn_rc=1)
        assert recovery._respawn_bg_resume(self._cand(), "U-abc") is False


class TestNotifyManualResume:
    """US5: the manual-resume OS notification carries the exact resume command."""

    def _cand(self):
        return recovery.Candidate(short_id="9999aaaa", sock_path="/tmp/g.sock",
                                  jobs_dir=None, cwd="/wt/thread", name="thread-w")

    def test_managed_command_has_no_env_prefix(self, monkeypatch):
        # Managed shares the default slot, so the resume command needs no env.
        from fno.adapters.providers import dispatch as dispatch_mod
        monkeypatch.setattr(dispatch_mod, "dispatch_env", lambda pid, **k: {})
        assert recovery._resume_command("claude-secondary", "/wt/thread", "U-1") == \
            "claude --resume U-1"

    def test_oauth_dir_command_prefixes_config_dir(self, monkeypatch):
        # A two-dir account resumes under its own CLAUDE_CONFIG_DIR.
        from fno.adapters.providers import dispatch as dispatch_mod
        monkeypatch.setattr(dispatch_mod, "dispatch_env",
                            lambda pid, **k: {"CLAUDE_CONFIG_DIR": "/home/u/.claude-b"})
        assert recovery._resume_command("claude-b", "/wt/thread", "U-1") == \
            "CLAUDE_CONFIG_DIR=/home/u/.claude-b claude --resume U-1"

    def test_notify_sends_os_notification_with_command(self, monkeypatch):
        from types import SimpleNamespace
        from fno.notify import _impl as notify_impl
        sent: dict = {}
        monkeypatch.setattr(notify_impl, "send_notification",
                            lambda title, body: sent.update(title=title, body=body) or (0, ""))
        monkeypatch.setattr(recovery, "_resume_command", lambda *a: "claude --resume U-1")
        recovery._notify_manual_resume(self._cand(),
                                       SimpleNamespace(id="claude-secondary"), "U-1")
        assert "claude --resume U-1" in sent["body"]
        assert "claude-secondary" in sent["title"]
