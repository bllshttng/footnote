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
        assert recovery.classify(
            "needs-input", old, _now(), 300,
            truth_state="your-move", truth_age_s=3600,
        ) == recovery.SKIP_NEEDS_INPUT

    def test_needs_input_phase_survives_stalled_transcript_truth(self):
        old = _iso(_now() - timedelta(hours=3))
        assert recovery.classify(
            "needs-input", old, _now(), 300,
            truth_state="stalled", truth_age_s=7201,
        ) == recovery.SKIP_NEEDS_INPUT

    @pytest.mark.parametrize("state", ["done", "completed", "failed"])
    def test_terminal_states_skipped(self, state):
        # AC3-EDGE: a clean terminal is never re-nudged.
        old = _iso(_now() - timedelta(hours=1))
        assert recovery.classify(
            state, old, _now(), 300, truth_state="done", truth_age_s=3600,
        ) == recovery.SKIP_TERMINAL

    def test_past_promise_does_not_force_skip(self):
        # codex P2: a <promise> is only the model's completion claim; loop-check
        # can reject it and the session keeps going. So a stale running session
        # is still a nudge target regardless of any past promise — "done" is the
        # terminal job state, not a transcript promise. (No promise input exists.)
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify(
            "running", stale, _now(), 300, truth_state="stalled", truth_age_s=600,
        ) == recovery.NUDGE

    def test_naive_now_does_not_raise(self):
        # gemini medium: a timezone-naive now must not raise on subtraction.
        stale = _iso(_now() - timedelta(seconds=600))
        naive_now = _now().replace(tzinfo=None)
        assert recovery.classify(
            "running", stale, naive_now, 300,
            truth_state="stalled", truth_age_s=600,
        ) == recovery.NUDGE

    def test_running_and_fresh_is_not_stale(self):
        fresh = _iso(_now() - timedelta(seconds=30))
        assert recovery.classify(
            "running", fresh, _now(), 300, truth_state="working", truth_age_s=30,
        ) == recovery.NOT_STALE

    def test_running_and_stale_nudges(self):
        # AC1-HP: idle past the threshold, work incomplete -> nudge.
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify(
            "running", stale, _now(), 300, truth_state="working", truth_age_s=600,
        ) == recovery.NUDGE

    def test_empty_or_unknown_state_stale_nudges(self):
        # A clean connection-close leaves state at the last value (often "running"
        # or empty); freshness is what distinguishes wedged from working.
        stale = _iso(_now() - timedelta(seconds=600))
        assert recovery.classify(
            "", stale, _now(), 300, truth_state="stalled", truth_age_s=600,
        ) == recovery.NUDGE

    def test_missing_updated_at_is_conservative(self):
        # Can't prove idleness -> do not nudge.
        assert recovery.classify(
            "running", None, _now(), 300, truth_state="unknown", truth_age_s=None,
        ) == recovery.NOT_STALE

    def test_unparseable_updated_at_is_conservative(self):
        assert recovery.classify(
            "running", "not-a-date", _now(), 300,
            truth_state="unknown", truth_age_s=None,
        ) == recovery.NOT_STALE

    def test_frozen_terminal_state_cannot_override_family1_working(self):
        assert (
            recovery.classify(
                "done", "2020-01-01T00:00:00Z", _now(), 300,
                truth_state="working", truth_age_s=30,
            )
            == recovery.NOT_STALE
        )

    def test_family1_stalled_drives_recovery_even_when_state_json_is_fresh(self):
        assert (
            recovery.classify(
                "running", _iso(_now()), _now(), 300,
                truth_state="stalled", truth_age_s=600,
            )
            == recovery.NUDGE
        )


# ---------------------------------------------------------------------------
# candidate join — registry provenance ∩ live bg sessions (the AC invariant:
# only ever touch sessions footnote launched)
# ---------------------------------------------------------------------------

class _Entry:
    def __init__(self, harness, short_id, cwd=None):
        self.harness = harness
        self.short_id = short_id
        self.cwd = cwd


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


def _node_bound_cwd(tmp_path):
    """A worktree-like cwd with a ``.fno/target-state.md`` so the candidate reads
    as a node-bound /target worker (not node-less)."""
    cwd = tmp_path / "wt"
    (cwd / ".fno").mkdir(parents=True, exist_ok=True)
    (cwd / ".fno" / "target-state.md").write_text(
        "graph_node_id: x-test\n", encoding="utf-8"
    )
    return str(cwd)


def _stale_candidate(tmp_path, short_id="aaaa1111", sock="/tmp/a.sock", *, node_less=False):
    return recovery.Candidate(
        short_id=short_id,
        sock_path=sock,
        jobs_dir=tmp_path,
        cwd=None if node_less else _node_bound_cwd(tmp_path),
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

    def truth(self, _candidate):
        if self._state == "needs-input":
            return {"state": "your-move", "last_activity_age_s": 600}
        if self._state in {"done", "completed", "failed"}:
            return {"state": "done", "last_activity_age_s": 600}
        age = int((_now() - datetime.fromisoformat(self._updated.replace("Z", "+00:00"))).total_seconds())
        return {"state": "stalled" if age >= 300 else "working", "last_activity_age_s": age}

    def send(self, sock, content, from_name):
        self.sends.append((sock, content))

    def event_types(self):
        return [e[0] for e in self.events]


class TestSweep:
    def test_stale_node_bound_worker_surfaced_as_held_no_send(self, tmp_path):
        # x-d93d: a stuck node-bound /target worker is bypass, so the socket
        # nudge is held by Claude Code's crossSessionInbound guard. recovery does
        # NOT send (that would stack an operator dialog); it surfaces the stuck
        # worker once as held-by-design.
        h = _Harness()
        counts: dict = {}
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts=counts,
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
        )
        assert h.sends == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "held-by-design"
        assert counts["aaaa1111"] == 1

    def test_node_less_bg_thread_is_silent_not_surfaced(self, tmp_path):
        # x-a76d scope fix: a node-less bg thread (an ask/relay worker, or a
        # ``--bg`` session a human attached to and drives) is none of recovery's
        # mission. A stuck one stays silent, never a candidate for held-by-design
        # surfacing or a close nudge.
        h = _Harness()
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path, node_less=True)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
        )
        assert h.sends == []
        assert h.events == []

    def test_needs_input_emits_skipped_no_send(self, tmp_path):
        h = _Harness(state="needs-input")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
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
            truth_fn=h.truth, liveness_fn=h.liveness,
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
            truth_fn=h.truth, liveness_fn=h.liveness,
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
                truth_fn=h.truth, liveness_fn=h.liveness,
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
            truth_fn=h.truth, liveness_fn=h.liveness,
        )
        assert h.sends == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "socket-unreachable"

    def test_held_path_does_not_invoke_send_fn(self, tmp_path):
        # x-d93d: the socket nudge was removed; a stuck node-bound worker is
        # surfaced held-by-design and no socket send is attempted (the bypass
        # recipient would hold it). The seam carries no send_fn anymore.
        h = _Harness()
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
        )
        assert h.sends == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "held-by-design"


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
    def test_end_to_end_held_surface_and_counts_persisted(self, tmp_path):
        h = _Harness()  # stale running session, live socket, no promise
        entries = [_Entry("claude", "aaaa1111", cwd=_node_bound_cwd(tmp_path)), _Entry("codex", "z")]
        live = {"aaaa1111": _Locator("aaaa1111", "/tmp/a.sock", tmp_path)}
        saved: dict = {}

        n = recovery.run_recovery_sweep(
            _Cfg(),
            emit=h.emit,
            now=_now(),
            registry_load=lambda: entries,
            locate_fn=lambda sid: live.get(sid),
            read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
            load_counts_fn=lambda: {},
            save_counts_fn=lambda c: saved.update(c),
        )

        assert n == 1
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "held-by-design"
        assert saved["aaaa1111"] == 1

    def test_prunes_counts_for_vanished_sessions(self, tmp_path):
        h = _Harness()
        entries = [_Entry("claude", "aaaa1111", cwd=_node_bound_cwd(tmp_path))]
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
            truth_fn=h.truth, liveness_fn=h.liveness,
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
        # not a swap trigger — it surfaces held-by-design, not failover.
        err = recovery.classify_session_error("API Error: Connection closed mid-response")
        assert err is None or err.triggers_swap is False

    def test_no_output_returns_none(self):
        assert recovery.classify_session_error(None) is None
        assert recovery.classify_session_error("") is None
        assert recovery.classify_session_error(123) is None  # non-str


class TestClassifyWorkerRefusal:
    """AC1: a LIVE worker's refusal is readable from the transcript turn."""

    def test_transcript_refusal_is_found_and_sourced(self):
        # AC1-HP: the capped worker never died, so output_result is absent and
        # only its last turn carries the refusal.
        got = recovery.classify_worker_refusal(
            None, "Claude usage limit reached. Resets 2026-08-18 07:19:38"
        )
        assert got is not None
        err, source = got
        assert err.triggers_swap is True
        assert source == "transcript"

    def test_output_result_wins_over_transcript(self):
        # A dead session behaves exactly as today: the death text is
        # authoritative and the source says so.
        got = recovery.classify_worker_refusal(
            "API Error: rate limit exceeded", "usage limit reached"
        )
        assert got is not None
        assert got[1] == "output_result"

    def test_ordinary_work_text_is_not_a_refusal(self):
        # AC1-NEG: no marker in either source means the existing classify and
        # nudge path is untouched.
        assert recovery.classify_worker_refusal(
            None, "Ran the tests, 41 passed. Committing now."
        ) is None

    def test_non_swap_class_output_falls_through_to_transcript(self):
        # A connection drop is not swap-class, so it must not shadow a real
        # refusal sitting in the transcript.
        got = recovery.classify_worker_refusal(
            "API Error: Connection closed mid-response", "usage limit reached"
        )
        assert got is not None
        assert got[1] == "transcript"

    def test_no_evidence_at_all_returns_none(self):
        assert recovery.classify_worker_refusal(None, None) is None
        assert recovery.classify_worker_refusal("", "") is None


class _RefusalHarness(_Harness):
    """A sweep harness whose worker is LIVE, fresh, and carrying a refusal.

    This is the population the whole plan exists for: state=running, transcript
    mtime seconds old, and the last turn is the provider saying no. Every gate
    in the sweep reads it as healthy.
    """

    def __init__(self, last_message, **kw):
        super().__init__(updated_age_s=5, **kw)
        self._last_message = last_message

    def truth(self, _candidate):
        return {
            "state": "working",
            "last_activity_age_s": 5,
            "last_message": self._last_message,
            "observed_model": {"kind": "observed", "model": "glm-5.2", "samples": 3},
        }


class TestRefusalHoistedAboveTheStalenessGate:
    """AC1: a refusal is affirmative evidence and does not wait for a timer."""

    _QUOTA = (
        "Claude usage limit reached. Your limit will reset at "
        "2026-08-18T07:19:38+08:00"
    )

    def _sweep(self, h, tmp_path, counts):
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts=counts,
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
        )

    def test_ac1_hp_fires_on_the_first_tick_while_the_worker_reads_fresh(
        self, tmp_path
    ):
        # The worker is 5s old against a 300s idle threshold, so classify()
        # returns NOT_STALE and the old code skipped it entirely. The event must
        # fire anyway, on this tick.
        h = _RefusalHarness(self._QUOTA)
        self._sweep(h, tmp_path, {})
        assert "worker_refused" in h.event_types()
        payload = dict(h.events[h.event_types().index("worker_refused")][1])
        assert payload["short_id"] == "aaaa1111"
        assert payload["node"] == "x-test"
        assert payload["error_class"] == "provider_4xx_quota"
        assert payload["source"] == "transcript"
        assert payload["model"] == "glm-5.2"
        assert payload["resets_at"] is not None

    def test_a_terminal_promise_does_not_suppress_the_refusal(self, tmp_path):
        # A worker that emitted <promise> earlier reads done -> SKIP_TERMINAL
        # forever. The refusal must still be heard.
        h = _RefusalHarness(self._QUOTA, state="done")
        self._sweep(h, tmp_path, {})
        assert "worker_refused" in h.event_types()

    def test_ac1_edge_a_second_tick_with_no_new_turn_stays_silent(self, tmp_path):
        counts: dict = {}
        h = _RefusalHarness(self._QUOTA)
        self._sweep(h, tmp_path, counts)
        h2 = _RefusalHarness(self._QUOTA)
        self._sweep(h2, tmp_path, counts)
        assert "worker_refused" not in h2.event_types()

    def test_ac1_neg_ordinary_work_text_emits_nothing_new(self, tmp_path):
        h = _RefusalHarness("Ran the tests, 41 passed. Committing now.")
        self._sweep(h, tmp_path, {})
        assert "worker_refused" not in h.event_types()

    def test_the_dedup_key_is_pruned_when_the_session_dies(self):
        live = {"aaaa1111"}
        assert recovery._prune_keep("refused:aaaa1111:provider_4xx_quota", live)
        assert not recovery._prune_keep("refused:bbbb2222:provider_4xx_quota", live)

    def test_the_dedup_key_is_scoped_to_the_error_class(self):
        # A quota refusal followed by an auth refusal is two findings; the SAME
        # refusal re-read every tick is one.
        assert recovery._refused_key("a", "provider_4xx_quota") != (
            recovery._refused_key("a", "provider_4xx_auth")
        )


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
            truth_fn=h.truth, liveness_fn=h.liveness,
            failover_fn=h.failover,
        )

    def test_swap_class_routes_to_failover_not_nudge(self, tmp_path):
        # AC1-FR: a quota-died bg session swaps + re-dispatches, never nudges.
        h = _FailoverHarness(output_result="API Error: rate limit exceeded", outcome="swapped")
        self._run(h, tmp_path)
        assert len(h.failover_calls) == 1
        assert h.sends == []                       # NOT nudged
        # worker_refused first: the swap-class error is now announced as a
        # positive finding before anything acts on it.
        assert h.event_types() == ["worker_refused", "failover_swapped"]
        assert h.events[1][1]["redispatched"] is True   # honest: worker started

    def test_rotated_no_worker_emits_swapped_then_held(self, tmp_path):
        # codex P1: the swap rotated the provider but no replacement worker
        # started (non-claude target / spawn failed). The event must report
        # redispatched=False (no phantom redispatch); the stuck session then
        # falls through to the held-by-design surface (not a socket nudge).
        h = _FailoverHarness(output_result="rate limit", outcome="rotated-no-worker")
        self._run(h, tmp_path)
        assert h.event_types() == [
            "worker_refused", "failover_swapped", "recovery_skipped",
        ]
        assert h.events[1][1]["redispatched"] is False
        assert h.events[2][1]["reason"] == "held-by-design"
        assert h.sends == []

    def test_one_swap_per_tick(self, tmp_path):
        # codex P2: a swap mutates the GLOBAL active provider, so only one
        # rotation may fire per tick; the second stale session surfaces
        # held-by-design this tick (reconsidered next tick against the settled
        # provider).
        h = _FailoverHarness(output_result="rate limit", outcome="swapped")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[
                _stale_candidate(tmp_path, short_id="aaaa1111"),
                _stale_candidate(tmp_path, short_id="bbbb2222", sock="/tmp/b.sock"),
            ],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
            failover_fn=h.failover,
        )
        assert len(h.failover_calls) == 1            # only the first swaps
        assert h.failover_calls[0][0] == "aaaa1111"
        # Two candidates, so two refusal notices: the once-only guard is per
        # short_id, and the one-swap-per-tick guard is separate from it.
        assert h.event_types() == [
            "worker_refused", "failover_swapped",
            "worker_refused", "recovery_skipped",
        ]
        assert h.events[3][1]["short_id"] == "bbbb2222"   # the second surfaced

    def test_connection_drop_surfaces_held(self, tmp_path):
        # AC2-FR: a clean connection-drop never triggers failover; the stuck
        # worker is surfaced held-by-design (the socket nudge is held by the
        # bypass recipient, x-d93d).
        h = _FailoverHarness(output_result="API Error: Connection closed mid-response")
        self._run(h, tmp_path)
        assert h.failover_calls == []
        assert h.sends == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "held-by-design"

    def test_no_output_result_surfaces_held(self, tmp_path):
        # No last-error text (the common idle case): held-by-design surface.
        h = _FailoverHarness(output_result=None)
        self._run(h, tmp_path)
        assert h.failover_calls == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "held-by-design"

    def test_blocked_thrash_emits_blocked_no_nudge(self, tmp_path):
        # AC2-EDGE: storm-cap reached -> bounded stop, no nudge churn.
        h = _FailoverHarness(output_result="rate limit", outcome="blocked-thrash")
        self._run(h, tmp_path)
        assert h.sends == []
        assert h.event_types() == ["worker_refused", "failover_blocked"]
        assert h.events[1][1]["reason"] == "blocked-thrash"

    def test_notified_emits_swapped_and_does_not_nudge(self, tmp_path):
        # US4/US5 (AC3-FR + AC4-FR "dead one not also nudged"): a revival that
        # degraded to the manual-resume notification rotated the provider but
        # started no worker, so it reports redispatched=False and must NOT also
        # nudge the exhausted session.
        h = _FailoverHarness(output_result="usage limit reached", outcome="notified")
        self._run(h, tmp_path)
        assert h.sends == []                           # NOT nudged
        assert h.event_types() == ["worker_refused", "failover_swapped"]
        assert h.events[1][1]["redispatched"] is False

    def test_queue_exhausted_falls_through_to_held(self, tmp_path):
        # AC1-EDGE (watchdog reading): no eligible alternate -> nothing to swap
        # to, so fall through to the held-by-design surface. Nothing can be
        # swapped to and the socket nudge cannot reach a bypass recipient, so the
        # honest action is to surface the stuck session once for the operator.
        h = _FailoverHarness(output_result="quota exceeded", outcome="queue-exhausted")
        self._run(h, tmp_path)
        assert h.sends == []
        assert h.event_types() == ["worker_refused", "recovery_skipped"]
        assert h.events[1][1]["reason"] == "held-by-design"

    def test_no_swap_outcome_falls_through_to_held(self, tmp_path):
        # Controller declined (NO_SWAP_NEEDED): defensive fall-through to held.
        h = _FailoverHarness(output_result="rate limit", outcome="no-swap")
        self._run(h, tmp_path)
        assert h.event_types() == ["worker_refused", "recovery_skipped"]
        assert h.events[1][1]["reason"] == "held-by-design"

    def test_failover_disabled_when_fn_absent(self, tmp_path):
        # Backward compat: no failover_fn -> swap-class error surfaces held-by-design.
        h = _FailoverHarness(output_result="rate limit exceeded")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts={},
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
            # failover_fn omitted
        )
        assert h.failover_calls == []
        # The refusal notice is independent of failover_fn: reporting that a
        # worker cannot think must not depend on having somewhere to move it.
        assert h.event_types() == ["worker_refused", "recovery_skipped"]
        assert h.events[1][1]["reason"] == "held-by-design"


class TestDefaultFailover:
    """The real failover_fn maps SwapDecision -> the sweep's outcome strings.
    It re-reads the active provider's cli KIND after a swap and only
    bg-redispatches when that kind is claude. Controller + settings are
    monkeypatched so no real provider rotation / subprocess fires."""

    def _patch(self, monkeypatch, decision, new_cli="claude", redispatch_result=None,
               calls=None, auth=None, seen_materialize=None):
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

            def attempt_swap(self, *, current_provider_id, error, materialize_managed=True):
                if seen_materialize is not None:
                    seen_materialize.append(materialize_managed)
                return _Result()

        class _Snap:
            # Read AFTER the swap, so it is the swapped-to record: .id is the new
            # active id, .harness its kind, .auth its auth strategy (US3: "managed"
            # needs a credential materialization into the shared slot pre-redispatch).
            id = "claude-secondary"
            harness = new_cli
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
        seen_materialize: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="managed",
                    seen_materialize=seen_materialize)
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: True)
        mat: list = []
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: mat.append(rid) or True)
        err = recovery.classify_session_error("usage limit reached")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "swapped"
        assert mat == ["claude-secondary"]   # materialized the swapped-to record
        assert calls == [_stale_candidate(tmp_path).short_id]  # via _redispatch
        # attempt_swap itself must NOT materialize here: the candidate still
        # pins the shared slot until _redispatch stops it below, so an eager
        # switch() would hit that live pin and self-block the swap. This
        # sweep always does its own, correctly post-stop materialize via
        # _redispatch's pre_spawn instead (asserted above via `mat`).
        assert seen_materialize == [False]

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
        seen_materialize: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="managed",
                    seen_materialize=seen_materialize)
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: False)
        mat = {"called": False}
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: mat.__setitem__("called", True) or True)
        err = recovery.classify_session_error("usage limit reached")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "rotated-no-worker"
        assert calls == []                    # never stopped/redispatched the worker
        assert mat["called"] is False         # never materialized
        # attempt_swap is never asked to materialize from this caller,
        # armed or not - the sweep's own auto_switch gate above (line 775)
        # is what actually decides, and it decided nothing happens here.
        assert seen_materialize == [False]

    def test_oauth_dir_swap_skips_materialize(self, monkeypatch, tmp_path):
        # An oauth_dir claude record needs no materialization (env-var switch at
        # spawn); _redispatch runs with no pre_spawn hook. auto_switch being
        # armed has no effect on an oauth_dir candidate: no pre_spawn hook,
        # no _materialize_managed_switch call.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="oauth_dir")
        called = {"mat": False}
        monkeypatch.setattr(recovery, "_auto_switch_enabled", lambda repo_root=None: True)
        monkeypatch.setattr(recovery, "_materialize_managed_switch",
                            lambda rid, repo_root=None: called.__setitem__("mat", True) or True)
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(_stale_candidate(tmp_path), err) == "swapped"
        assert called["mat"] is False         # no materialize for oauth_dir
        assert calls == [_stale_candidate(tmp_path).short_id]

    # --- US4: node-bound vs node-less routing ---------------------------------

    def test_node_less_thread_routes_to_revival(self, monkeypatch, tmp_path):
        # US4: a claude swap whose candidate has a live cwd but NO target-state
        # node routes to _revive_bg_thread (resume the transcript), NOT _redispatch.
        from fno.adapters.providers.failover import SwapDecision

        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude", auth="oauth_dir")
        monkeypatch.setattr(recovery, "_worktree_is_node_less", lambda cwd: True)
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
        # A candidate whose worktree HAS a manifest stays on the _redispatch path.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=True, calls=calls, auth="oauth_dir")
        monkeypatch.setattr(recovery, "_worktree_is_node_less", lambda cwd: False)
        monkeypatch.setattr(
            recovery, "_revive_bg_thread",
            lambda *a, **k: pytest.fail("revival must not run for a node-bound worker"))
        cand = recovery.Candidate(short_id="dddd4444", sock_path="/tmp/d.sock",
                                  jobs_dir=tmp_path, cwd=str(tmp_path), name="node-w")
        err = recovery.classify_session_error("rate limit")
        assert recovery._default_failover(cand, err) == "swapped"
        assert calls == ["dddd4444"]

    def test_unreadable_manifest_stays_node_bound(self, monkeypatch, tmp_path):
        # Finding 1: a real node-bound worker whose manifest read transiently fails
        # (._node_id_from_worktree -> None) must NOT misroute into revival. The
        # gate is confirmed manifest ABSENCE, so an unreadable manifest (is_node_less
        # False) falls through to _redispatch and its bounded nudge, not a revival.
        from fno.adapters.providers.failover import SwapDecision

        calls: list = []
        self._patch(monkeypatch, SwapDecision.SWAPPED, new_cli="claude",
                    redispatch_result=False, calls=calls, auth="oauth_dir")
        monkeypatch.setattr(recovery, "_worktree_is_node_less", lambda cwd: False)
        monkeypatch.setattr(
            recovery, "_revive_bg_thread",
            lambda *a, **k: pytest.fail("a read-miss worker must not revive"))
        cand = recovery.Candidate(short_id="eeee0000", sock_path="/tmp/e0.sock",
                                  jobs_dir=tmp_path, cwd=str(tmp_path), name="node-w")
        err = recovery.classify_session_error("rate limit")
        # redispatch returned False -> rotated-no-worker (nudge), never notified.
        assert recovery._default_failover(cand, err) == "rotated-no-worker"
        assert calls == ["eeee0000"]


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
        return ProviderRecord(id="claude-secondary", name="B", harness="claude", auth="managed")

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
                            lambda r, **k: seen.update(id=r.id, by_id=k.get("by_id"),
                                                       pin_policy=k.get("pin_policy")))
        assert recovery._materialize_managed_switch("claude-secondary") is True
        assert seen["id"] == "claude-secondary"
        assert seen["pin_policy"] == "defer"  # recovery must never swap under live pins

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
        self._patch_graph(monkeypatch, [{"id": "x-370f", "status": "done"}])
        assert recovery._node_is_done("x-370f") is True

    def test_false_when_not_done(self, monkeypatch):
        self._patch_graph(monkeypatch, [{"id": "x-370f", "status": "claimed"}])
        assert recovery._node_is_done("x-370f") is False

    def test_false_when_absent(self, monkeypatch):
        self._patch_graph(monkeypatch, [{"id": "x-other", "status": "done"}])
        assert recovery._node_is_done("x-370f") is False

    def test_load_error_degrades_to_false(self, monkeypatch):
        from fno.graph import load as gl

        def boom(*a, **k):
            raise RuntimeError("corrupt graph")

        monkeypatch.setattr(gl, "load_graph", boom)
        assert recovery._node_is_done("x-370f") is False


class TestMissionAwareTerminalGate:
    """x-5583: a hollow ``<promise>`` no longer suppresses recovery forever."""

    def _classify(self, mission_complete, age_s=3600):
        return recovery.classify(
            "running", None, _now(), 300,
            truth_state="done", truth_age_s=age_s,
            mission_complete=mission_complete,
        )

    def test_incomplete_and_stale_nudges(self):
        # AC1: positive evidence of an unfinished mission relaxes the skip.
        assert self._classify(False) == recovery.NUDGE

    @pytest.mark.parametrize("mc", [None])
    def test_unverifiable_stays_terminal(self, mc):
        # AC7: None (probe failed, node-less thread) keeps the terminal skip
        # (fail closed).
        assert self._classify(mc) == recovery.SKIP_TERMINAL

    def test_complete_and_stale_nudges_close(self):
        # x-a76d: a session whose mission actually shipped but still lingers is
        # surfaced to CLOSE once idle, not left silent or resumed.
        assert self._classify(True) == recovery.NUDGE_CLOSE

    def test_complete_and_fresh_is_left_alone(self):
        # A fresh finish is not yet lingering; leave it alone.
        assert self._classify(True, age_s=120) == recovery.NOT_STALE

    def test_default_keyword_preserves_legacy_behavior(self):
        assert recovery.classify(
            "running", None, _now(), 300,
            truth_state="done", truth_age_s=3600,
        ) == recovery.SKIP_TERMINAL

    @pytest.mark.parametrize("age", [None, 299])
    def test_fresh_incomplete_promise_is_not_nudged(self, age):
        # AC4: finalize may still be in flight; the staleness gate outwaits it.
        assert self._classify(False, age_s=age) == recovery.NOT_STALE

    def test_needs_input_still_wins_over_incomplete_mission(self):
        assert recovery.classify(
            "needs-input", None, _now(), 300,
            truth_state="done", truth_age_s=3600, mission_complete=False,
        ) == recovery.SKIP_NEEDS_INPUT

    # -- sweep wiring -----------------------------------------------------
    def _sweep(self, tmp_path, *, mission_complete_fn, counts=None,
               failover_fn=None, output_result=None):
        h = _Harness(state="done")
        if output_result is not None:
            h.read_state = lambda jobs_dir: recovery._SnapshotView(
                "done", None, output_result)
        counts = {} if counts is None else counts
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)],
            counts=counts,
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
            failover_fn=failover_fn,
            mission_complete_fn=mission_complete_fn,
        )
        return h, counts

    def test_sweep_hollow_promise_surfaces_held(self, tmp_path):
        # A hollow promise (mission_complete=False) is unfinished; once idle it
        # falls to the held-by-design surface (the resume nudge is held by the
        # bypass recipient, x-d93d).
        h, counts = self._sweep(tmp_path, mission_complete_fn=lambda c: False)
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "held-by-design"
        assert counts["aaaa1111"] == 1

    def test_sweep_complete_mission_surfaces_close(self, tmp_path):
        # x-a76d: a finished-but-lingering session is surfaced to CLOSE over a
        # working channel, not resumed over the held socket and not left silent.
        h, _ = self._sweep(tmp_path, mission_complete_fn=lambda c: True)
        assert h.sends == []
        assert h.event_types() == ["recovery_close_notify"]

    def test_close_notify_calls_seam_once_across_ticks(self, tmp_path):
        # The close surface fires ONCE per session (a finished session the
        # operator has not gotten to must not ping every tick).
        notified: list = []
        counts: dict = {}
        for _ in range(3):
            h = _Harness(state="done")
            recovery.recovery_sweep(
                _now(), _Cfg(),
                candidates=[_stale_candidate(tmp_path)], counts=counts,
                emit=h.emit, read_state_fn=h.read_state,
                truth_fn=h.truth, liveness_fn=h.liveness,
                mission_complete_fn=lambda c: True,
                notify_close_fn=lambda c: notified.append(c) or True,
            )
        assert len(notified) == 1
        assert counts[recovery._close_key("aaaa1111")] is True

    def test_close_notify_undelivered_does_not_claim_surface(self, tmp_path):
        # A channel that reports not-delivered (no osascript/notify-send, or a
        # raising seam) must not record a recovery_close_notify it never sent.
        def boom(_c):
            raise RuntimeError("notify down")

        h = _Harness(state="done")
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)], counts={},
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
            mission_complete_fn=lambda c: True,
            notify_close_fn=boom,
        )
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "no-notify-channel"

    def test_sweep_without_seam_is_unchanged(self, tmp_path):
        h, _ = self._sweep(tmp_path, mission_complete_fn=None)
        assert (h.sends, h.events) == ([], [])

    def test_probe_skipped_off_the_terminal_path(self, tmp_path):
        calls: list = []
        h = _Harness(state="running")  # truth -> stalled, not done
        recovery.recovery_sweep(
            _now(), _Cfg(),
            candidates=[_stale_candidate(tmp_path)], counts={},
            emit=h.emit, read_state_fn=h.read_state,
            truth_fn=h.truth, liveness_fn=h.liveness,
            mission_complete_fn=lambda c: calls.append(c) or False,
        )
        assert calls == []
        assert h.event_types() == ["recovery_skipped"]
        assert h.events[0][1]["reason"] == "held-by-design"

    def test_hollow_promise_reaches_failover(self, tmp_path):
        # AC1/US4: a swap-class death behind a hollow promise rotates providers
        # instead of nudging an already-rate-limited account.
        seen: list = []
        h, _ = self._sweep(
            tmp_path, mission_complete_fn=lambda c: False,
            output_result="API Error: 429 rate limit exceeded",
            failover_fn=lambda c, err: seen.append(err) or "swapped",
        )
        assert len(seen) == 1
        assert h.event_types() == ["worker_refused", "failover_swapped"]
        assert h.sends == []

    def test_cap_bounds_the_restored_path(self, tmp_path):
        # AC6: the newly reachable candidates obey max_nudges + capped-once.
        h, counts = self._sweep(tmp_path, mission_complete_fn=lambda c: False,
                                counts={"aaaa1111": 3})
        assert h.sends == []
        assert h.event_types() == ["recovery_capped"]
        h2, _ = self._sweep(tmp_path, mission_complete_fn=lambda c: False,
                            counts=counts)
        assert h2.events == []


class TestMissionComplete:
    """x-5583: the family-2 artifact probe behind the terminal-suppression gate."""

    def _patch_graph(self, monkeypatch, entries):
        from fno.graph import load as gl
        monkeypatch.setattr(gl, "load_graph", lambda *a, **k: entries)

    def _cand(self, name=None, cwd=None):
        return recovery.Candidate(short_id="s1", sock_path="/s", jobs_dir=None,
                                  cwd=cwd, name=name)

    def _worktree(self, tmp_path, node):
        fno_dir = tmp_path / ".fno"
        fno_dir.mkdir()
        (fno_dir / "target-state.md").write_text(
            f"graph_node_id: {node}\n", encoding="utf-8")
        return str(tmp_path)

    # -- resolution order -------------------------------------------------
    def test_manifest_wins_over_name(self, monkeypatch, tmp_path):
        # A non-standard name that WOULD parse to nothing still resolves via the
        # manifest, and a manifest hit is always a target mission.
        self._patch_graph(monkeypatch, [{"id": "x-1111", "plan_path": "/p.md"}])
        cand = self._cand(name="tgt-x-9999-liveness",
                          cwd=self._worktree(tmp_path, "x-1111"))
        # plan_path alone never completes a target mission (AC5).
        assert recovery.mission_complete(cand) is False

    def test_name_fallback_when_no_manifest(self, monkeypatch, tmp_path):
        self._patch_graph(monkeypatch, [{"id": "x-2222", "plan_path": "/p.md"}])
        assert recovery.mission_complete(
            self._cand(name="think-x-2222-bar", cwd=str(tmp_path))) is True

    def test_think_name_ignores_a_foreign_manifest(self, monkeypatch, tmp_path):
        # spawn_think dispatches with --cwd on the node's CANONICAL root, where
        # an unrelated /target session's manifest can sit; a think worker writes
        # none of its own. Reading that manifest would answer about x-1111 and
        # nudge a design pass that actually finished.
        self._patch_graph(monkeypatch, [
            {"id": "x-1111", "status": "ready"},           # foreign, incomplete
            {"id": "x-2222", "plan_path": "/plans/d.md"},  # ours, complete
        ])
        cand = self._cand(name="think-x-2222-bar",
                          cwd=self._worktree(tmp_path, "x-1111"))
        assert recovery.mission_complete(cand) is True

    def test_unresolvable_mission_is_none(self, monkeypatch, tmp_path):
        # AC7: no node id in the name and no manifest -> unverifiable.
        self._patch_graph(monkeypatch, [{"id": "x-1111", "status": "ready"}])
        assert recovery.mission_complete(
            self._cand(name="relay-worker", cwd=str(tmp_path))) is None
        assert recovery.mission_complete(self._cand()) is None

    # -- target missions --------------------------------------------------
    @pytest.mark.parametrize("entry,expected", [
        ({"id": "x-1111", "status": "done"}, True),
        ({"id": "x-1111", "status": "ready", "pr_number": 42}, True),
        ({"id": "x-1111", "status": "ready", "pr_url": "https://gh/pr/42"}, True),
        ({"id": "x-1111", "status": "ready"}, False),
        # AC5: a blueprinted-but-unshipped target node is NOT complete.
        ({"id": "x-1111", "status": "ready", "plan_path": "/p.md"}, False),
    ])
    def test_target_artifacts(self, monkeypatch, entry, expected):
        self._patch_graph(monkeypatch, [entry])
        assert recovery.mission_complete(
            self._cand(name="target-x-1111-foo")) is expected

    # -- think missions ---------------------------------------------------
    @pytest.mark.parametrize("entry,expected", [
        ({"id": "x-2222", "plan_path": "/plans/d.md"}, True),
        ({"id": "x-2222", "status": "done"}, True),
        ({"id": "x-2222", "plan_path": ""}, False),
        ({"id": "x-2222", "plan_path": "   "}, False),  # whitespace is no artifact
        ({"id": "x-2222", "plan_path": None}, False),
        ({"id": "x-2222"}, False),
    ])
    def test_think_artifacts(self, monkeypatch, entry, expected):
        self._patch_graph(monkeypatch, [entry])
        assert recovery.mission_complete(
            self._cand(name="think-x-2222-bar")) is expected

    # -- non-birth think passes (codex P2 on PR #581) ---------------------
    @pytest.mark.parametrize("name", [
        "think-x-2222-retro-bar",          # dispatched only AFTER status done
        "think-x-2222-work-start-bar",     # starts on an already-linked node
        "think-x-2222-conversational-bar",
        "think-x-2222-retro",              # no slug tail
    ])
    @pytest.mark.parametrize("entry", [
        {"id": "x-2222", "status": "done"},
        {"id": "x-2222", "plan_path": "/plans/d.md"},
    ])
    def test_non_birth_think_cannot_inherit_the_nodes_artifacts(
        self, monkeypatch, name, entry,
    ):
        # These artifacts predate the worker, so reading them as completion
        # would re-open the exact suppression this change closes. Unverifiable.
        self._patch_graph(monkeypatch, [entry])
        assert recovery.mission_complete(self._cand(name=name)) is None

    def test_non_birth_think_is_unverifiable_in_both_directions(self, monkeypatch):
        # A retro produces a retro doc, not a plan link, so a MISSING plan_path
        # is no more evidence of failure than a present one is of success.
        # Nudging on it would be the fail-toward-nudge spam this design rejected.
        self._patch_graph(monkeypatch, [{"id": "x-2222", "status": "ready"}])
        assert recovery.mission_complete(
            self._cand(name="think-x-2222-retro-bar")) is None

    def test_birth_pass_whose_slug_shadows_a_reason_fails_closed(self, monkeypatch):
        # `think-x-2222-retrospective-ui` is a BIRTH pass whose slug merely starts
        # with a reason word. Reading it as lifecycle costs a suppression we would
        # have had anyway; the other direction would be a false completion.
        self._patch_graph(monkeypatch, [{"id": "x-2222", "plan_path": "/p.md"}])
        assert recovery.mission_complete(
            self._cand(name="think-x-2222-retro-spective-ui")) is None

    # -- failure paths (AC3) ----------------------------------------------
    def test_absent_node_is_unverifiable_not_incomplete(self, monkeypatch):
        self._patch_graph(monkeypatch, [{"id": "x-other", "status": "ready"}])
        assert recovery.mission_complete(
            self._cand(name="target-x-1111-foo")) is None

    def test_graph_error_degrades_to_none(self, monkeypatch):
        from fno.graph import load as gl

        def boom(*a, **k):
            raise RuntimeError("corrupt graph")

        monkeypatch.setattr(gl, "load_graph", boom)
        assert recovery.mission_complete(
            self._cand(name="target-x-1111-foo")) is None


class TestRedispatch:
    """x-370f residual 1: failover respawn frees the dead session's claim via
    ``fno claim release --force`` before spawning, skips an already-done node, and
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
            # Token containment, not a 3-slice: `claim force-release` became
            # `claim release --force`, and a prefix compare against a 4-token
            # marker is never true - it made this stub return the default 0 and
            # the force_release_rc=1 case silently pass a spawn it should block.
            if all(tok in cmd for tok in ("claim", "release", "--force")):
                return SimpleNamespace(returncode=force_release_rc)
            if cmd[:3] == ["fno-py", "agents", "spawn"]:
                return SimpleNamespace(returncode=spawn_rc)
            return SimpleNamespace(returncode=0)

        monkeypatch.setattr(sp, "run", fake_run)
        return calls

    @staticmethod
    def _index_of(calls, marker):
        """Index of the first call matching every token in ``marker``.

        Was a `c[:3]` prefix compare. `claim force-release` and `claim
        lane-release` collapsed into `claim release --force` / `--lane`, so a
        three-token prefix now matches BOTH and the two assertions below would
        silently pass on the wrong call. Matching on the full token set keeps
        each assertion pinned to the call it names.
        """
        return next(
            (i for i, c in enumerate(calls) if all(tok in c for tok in marker)),
            None,
        )

    def test_force_release_before_spawn_happy_path(self, monkeypatch):
        # AC1-HP: stop -> release --force node:<id> -> canonical claude bg spawn.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(self._cand()) is True

        fr = self._index_of(calls, ["fno-py", "claim", "release", "--force"])
        spawn = self._index_of(calls, ["fno-py", "agents", "spawn"])
        assert fr is not None and spawn is not None
        assert fr < spawn                      # claim freed strictly before spawn
        assert "node:x-370f" in calls[fr]      # exact claim key
        assert "-R" in calls[fr]               # required audit reason supplied
        spawn_cmd = calls[spawn]
        assert "--harness" in spawn_cmd and "claude" in spawn_cmd
        assert "--substrate" in spawn_cmd and "bg" in spawn_cmd
        assert "--cwd" in spawn_cmd and "/wt/x-370f" in spawn_cmd

    def test_stop_failure_skips_force_release_and_spawn(self, monkeypatch):
        # codex P2: a non-zero `fno agents stop` means the worker may still be
        # live; force-releasing its claim + spawning would create two workers on
        # one node. Bail to the nudge (no force-release, no spawn) → False.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch, stop_rc=1)
        assert recovery._redispatch(self._cand()) is False
        assert self._index_of(calls, ["fno-py", "claim", "release", "--force"]) is None
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
        lr = self._index_of(calls, ["fno-py", "claim", "release", "--lane"])
        assert lr is not None
        assert "x-370f" in calls[lr]

    def test_successful_respawn_keeps_lane_slot(self, monkeypatch):
        # A respawned worker reconciles the existing slot at target init; the
        # sweep must not release it out from under the new lane.
        self._patch_resolve(monkeypatch)
        calls = self._patch_run(monkeypatch)
        assert recovery._redispatch(self._cand()) is True
        assert self._index_of(calls, ["fno-py", "claim", "release", "--lane"]) is None

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
        fr = self._index_of(calls, ["fno-py", "claim", "release", "--force"])
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
        assert self._index_of(calls, ["fno-py", "claim", "release", "--lane"]) is not None


class TestReviveBgThread:
    """US4/US5: node-less bg-thread revival - resume under the new account, or
    degrade to the manual-resume notify path (never a resume against a missing
    transcript)."""

    def _cand(self, tmp_path):
        return recovery.Candidate(short_id="eeee5555", sock_path="/tmp/e.sock",
                                  jobs_dir=tmp_path, cwd=str(tmp_path), name="thread-w")

    def _snap(self, auth="oauth_dir"):
        from types import SimpleNamespace
        return SimpleNamespace(id="claude-secondary", harness="claude", auth=auth)

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

    def test_no_name_bails_without_spawn(self, monkeypatch):
        # Finding 2: with no name we cannot stop the dead thread, and the node-less
        # path has no claim+init backstop against a double, so a blind --resume
        # spawn could put two supervisors on one transcript. Bail (False), spawn
        # nothing (the caller notifies).
        calls = self._patch_run(monkeypatch)
        cand = recovery.Candidate(short_id="ffff6666", sock_path="/tmp/f.sock",
                                  jobs_dir=None, cwd="/wt/thread", name=None)
        assert recovery._respawn_bg_resume(cand, "U-abc") is False
        assert calls == []   # neither stop nor spawn ran


class TestWorktreeIsNodeLess:
    """Finding 1: revival gates on confirmed manifest ABSENCE, so an unreadable /
    transiently-unreachable manifest never misroutes a node-bound worker."""

    def test_absent_manifest_is_node_less(self, tmp_path):
        (tmp_path / ".fno").mkdir()
        assert recovery._worktree_is_node_less(str(tmp_path)) is True

    def test_present_manifest_is_not_node_less(self, tmp_path):
        fno = tmp_path / ".fno"
        fno.mkdir()
        (fno / "target-state.md").write_text("graph_node_id: x-1\n", encoding="utf-8")
        assert recovery._worktree_is_node_less(str(tmp_path)) is False

    def test_existing_manifest_is_not_node_less_regardless_of_content(self, tmp_path):
        # A present-but-garbage manifest still means a /target worker (a read miss
        # must not read as node-less): it exists, so treat it as node-bound.
        fno = tmp_path / ".fno"
        fno.mkdir()
        (fno / "target-state.md").write_text("\x00 not utf parseable", encoding="latin-1")
        assert recovery._worktree_is_node_less(str(tmp_path)) is False

    def test_missing_fno_dir_is_not_node_less(self, tmp_path):
        # No .fno at all, or a broken .fno symlink: cannot confirm node-less-ness.
        assert recovery._worktree_is_node_less(str(tmp_path)) is False


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

    def test_spacey_config_dir_is_shell_escaped(self, monkeypatch):
        # A config dir with spaces must stay one shell token when pasted (gemini
        # review): shlex.quote wraps it so the CLAUDE_CONFIG_DIR assignment holds.
        from fno.adapters.providers import dispatch as dispatch_mod
        monkeypatch.setattr(dispatch_mod, "dispatch_env",
                            lambda pid, **k: {"CLAUDE_CONFIG_DIR": "/Users/u/Application Support/.claude"})
        assert recovery._resume_command("claude-b", "/wt/thread", "U-1") == \
            "CLAUDE_CONFIG_DIR='/Users/u/Application Support/.claude' claude --resume U-1"

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
