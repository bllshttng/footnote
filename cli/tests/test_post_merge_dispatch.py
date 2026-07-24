"""Tests for the post-merge ritual dispatch seam (fno.post_merge_route), x-a35a.

The dispatch lives in fno.post_merge_route: one warm/cold/defer decision
(:func:`decide_post_merge_route`), a durable receipt (:func:`emit_receipt`),
marker + claim dedup, and a verb-first cold path that runs
``fno pr ritual <n> --autonomous`` directly (no bg thread, no ``/fno:pr merged``
LL wrapper). The ``run_verb`` and ``warm_inject`` seams are injected so no real
subprocess or inject fires.
"""
from __future__ import annotations

from fno.graph._reconcile import (
    MergeDriftRecord,
    PrMergeState,
    query_pr_merge_state,
    scan_merge_drift,
)
from fno.post_merge_route import (
    ColdRitualResult,
    PostMergeDispatchResult,
    dispatch_post_merge_ritual,
)


class _RunVerb:
    """A cold-path verb-runner seam: records ``(pr, cwd)`` and returns ok/fail."""

    def __init__(self, ok: bool = True, tail: str = "step=reconcile status=ok"):
        self.ok = ok
        self.tail = tail
        self.calls: list[tuple[int, str]] = []

    def __call__(self, pr_number: int, cwd: str) -> ColdRitualResult:
        self.calls.append((pr_number, cwd))
        return ColdRitualResult(ok=self.ok, tail=self.tail)


class _WarmInject:
    def __init__(self, delivered: bool = True, reason: str = "delivered"):
        self.delivered = delivered
        self.reason = reason
        self.calls: list[tuple[str, int]] = []

    def __call__(self, session_id: str, pr_number: int, source_harness=None):
        self.calls.append((session_id, pr_number, source_harness))
        return (self.delivered, self.reason)


class _Receipts:
    """Captures every emit_receipt call; can fail the reserved phase (AC4-ERR)."""

    def __init__(self, fail_reserved: bool = False):
        self.calls: list[tuple[str, dict]] = []
        self.fail_reserved = fail_reserved

    def __call__(self, phase, **kw):
        self.calls.append((phase, kw))
        if self.fail_reserved and phase == "reserved":
            return False
        return True

    def phases(self) -> list[str]:
        return [p for p, _ in self.calls]

    def last(self, phase: str) -> dict:
        rows = [kw for p, kw in self.calls if p == phase]
        return rows[-1] if rows else {}


def _patch_resolver(monkeypatch, result):
    import fno.post_merge_route as pmr

    monkeypatch.setattr(pmr, "resolve_warm_session", lambda sid, harness=None: result)


# --- gating / defer ------------------------------------------------------


def test_disabled_never_runs_verb(tmp_path):
    verb = _RunVerb()
    rcpt = _Receipts()
    res = dispatch_post_merge_ritual(
        7, dedup_key="sha1", auto_run=False, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=rcpt,
    )
    assert res.outcome == "disabled"
    assert verb.calls == []
    # AC9-EDGE: exactly one deferred receipt, no marker, no work.
    assert rcpt.phases() == ["deferred"]
    assert rcpt.last("deferred")["outcome"] == "auto-run-disabled"
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "sha1").exists()


# --- cold verb dispatch --------------------------------------------------


def test_dispatch_runs_verb_and_marks(tmp_path):
    verb = _RunVerb()
    rcpt = _Receipts()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaA", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=rcpt,
    )
    assert res.outcome == "dispatched"
    assert res.short_id == "verb"
    assert verb.calls == [(7, str(tmp_path))]
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaA").exists()
    # AC1-HP: reserved precedes accepted(cold, completed).
    assert rcpt.phases() == ["reserved", "accepted"]
    assert rcpt.last("accepted")["route"] == "cold"
    assert rcpt.last("accepted")["outcome"] == "completed"


# --- at-most-one per merge SHA -------------------------------------------


def test_second_dispatch_same_sha_is_noop(tmp_path):
    verb = _RunVerb()
    rcpt = _Receipts()
    first = dispatch_post_merge_ritual(
        7, dedup_key="shaB", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=rcpt,
    )
    second = dispatch_post_merge_ritual(
        7, dedup_key="shaB", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=rcpt,
    )
    assert first.outcome == "dispatched"
    assert second.outcome == "already-dispatched"
    assert second.detail == "marker-exists"  # genuine completed dedup
    assert len(verb.calls) == 1  # exactly one verb run for the merge SHA
    # a completed no-op writes no receipt (marker-exists short-circuits first)
    assert rcpt.phases() == ["reserved", "accepted"]


def test_lock_contention_distinguished_from_marker(tmp_path, monkeypatch):
    """A concurrent holder (ClaimHeldByOther) is in-flight, NOT done."""
    from fno import claims

    def _held(*a, **kw):
        raise claims.ClaimHeldByOther("other", pid=999, host="h", key="k")

    monkeypatch.setattr(claims, "acquire_claim", _held)
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaLC", auto_run=True, canonical_root=tmp_path, run_verb=verb,
    )
    assert res.outcome == "already-dispatched"
    assert res.detail == "lock-contention"
    assert verb.calls == []


def test_distinct_shas_each_dispatch(tmp_path):
    verb = _RunVerb()
    dispatch_post_merge_ritual(
        7, dedup_key="shaC", auto_run=True, canonical_root=tmp_path, run_verb=verb,
        emit_receipt_fn=_Receipts(),
    )
    dispatch_post_merge_ritual(
        8, dedup_key="shaD", auto_run=True, canonical_root=tmp_path, run_verb=verb,
        emit_receipt_fn=_Receipts(),
    )
    assert len(verb.calls) == 2


def test_verb_failure_drops_marker_emits_failed(tmp_path):
    """AC5-ERR: a non-zero verb exit appends failed, writes no marker, stays retryable."""
    verb = _RunVerb(ok=False, tail="step=reconcile status=failed")
    rcpt = _Receipts()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaE", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=rcpt,
    )
    assert res.outcome == "failed"
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaE").exists()
    assert "failed" in rcpt.phases()
    assert "step=reconcile status=failed" in rcpt.last("failed")["detail"]
    # a retry now succeeds
    ok = _RunVerb()
    res2 = dispatch_post_merge_ritual(
        7, dedup_key="shaE", auto_run=True, canonical_root=tmp_path,
        run_verb=ok, emit_receipt_fn=_Receipts(),
    )
    assert res2.outcome == "dispatched"
    assert len(ok.calls) == 1


def test_missing_sha_falls_back_to_pr_key(tmp_path):
    verb = _RunVerb()
    dispatch_post_merge_ritual(
        42, dedup_key=None, auto_run=True, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=_Receipts(),
    )
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "pr-42").exists()


# --- receipt lifecycle (AC4-ERR, AC5-ERR, AC7-FR) ------------------------


def test_reservation_failure_starts_no_work(tmp_path):
    """AC4-ERR: a failed reserved write is fail-closed -- no verb, no marker."""
    verb = _RunVerb()
    rcpt = _Receipts(fail_reserved=True)
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaR", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=rcpt,
    )
    assert res.outcome == "failed"
    assert res.detail == "receipt-reservation-failed"
    assert verb.calls == []
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaR").exists()


def test_dispatches_get_fresh_attempt_ids(tmp_path):
    """AC7-FR: each acted attempt reserves under a fresh attempt_id (crash retry shape)."""
    rcpt1 = _Receipts()
    dispatch_post_merge_ritual(
        7, dedup_key="shaT1", auto_run=True, canonical_root=tmp_path,
        run_verb=_RunVerb(), emit_receipt_fn=rcpt1,
    )
    rcpt2 = _Receipts()
    dispatch_post_merge_ritual(
        9, dedup_key="shaT2", auto_run=True, canonical_root=tmp_path,
        run_verb=_RunVerb(), emit_receipt_fn=rcpt2,
    )
    a1 = rcpt1.last("reserved")["attempt_id"]
    a2 = rcpt2.last("reserved")["attempt_id"]
    assert a1 and a2 and a1 != a2


def test_reserved_and_accepted_share_dispatch_and_attempt(tmp_path):
    """AC2-HP / AC7-FR: the reserved + accepted pair share dispatch_id + attempt_id."""
    rcpt = _Receipts()
    dispatch_post_merge_ritual(
        7, dedup_key="shaID", auto_run=True, canonical_root=tmp_path,
        run_verb=_RunVerb(), emit_receipt_fn=rcpt, node_id="x-9", repo_slug="o/r",
    )
    reserved = rcpt.last("reserved")
    accepted = rcpt.last("accepted")
    assert reserved["dispatch_id"] == accepted["dispatch_id"] == "shaID"
    assert reserved["attempt_id"] == accepted["attempt_id"]
    assert reserved["node_id"] == "x-9"
    assert reserved["repo_slug"] == "o/r"


# --- warm routing: inject XOR cold, one marker --------------------------


def test_warm_delivery_skips_cold_and_marks(tmp_path, monkeypatch):
    """AC1-HP: live originating session -> exactly one inject, no cold verb."""
    _patch_resolver(monkeypatch, "sess-live-1")
    warm = _WarmInject(delivered=True)
    verb = _RunVerb()
    rcpt = _Receipts()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW1", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sess-live-1", warm_inject=warm,
        emit_receipt_fn=rcpt,
    )
    assert res.outcome == "routed-warm"
    assert warm.calls == [("sess-live-1", 7, None)]
    assert verb.calls == []
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaW1").exists()
    assert rcpt.phases() == ["reserved", "accepted"]
    assert rcpt.last("accepted")["route"] == "warm"
    assert rcpt.last("accepted")["outcome"] == "delivered"


def test_warm_inject_failure_falls_back_cold(tmp_path, monkeypatch):
    """A warm miss degrades to the cold verb; the reason is kept."""
    _patch_resolver(monkeypatch, "sess-live-2")
    warm = _WarmInject(delivered=False, reason="not-live")
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW2", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sess-live-2", warm_inject=warm,
        emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "dispatched"
    assert res.detail == "cold: not-live"
    assert len(verb.calls) == 1
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaW2").exists()


def test_warm_queue_timeout_no_cold_verb_marks(tmp_path, monkeypatch):
    """AC7-FR: a queued (unconfirmed) inject already landed -- no cold verb, marker set."""
    _patch_resolver(monkeypatch, "sess-busy")
    warm = _WarmInject(delivered=False, reason="queue-timeout")
    verb = _RunVerb()
    rcpt = _Receipts()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW3", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sess-busy", warm_inject=warm,
        emit_receipt_fn=rcpt,
    )
    assert res.outcome == "routed-warm"
    assert res.detail == "queued"
    assert verb.calls == []
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaW3").exists()
    assert rcpt.last("accepted")["outcome"] == "queued"
    # a later call sees marker-exists, never a redundant cold verb
    second = dispatch_post_merge_ritual(
        7, dedup_key="shaW3", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id=None,
    )
    assert second.outcome == "already-dispatched"
    assert verb.calls == []


def test_no_source_session_takes_cold_path(tmp_path):
    warm = _WarmInject()
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW4", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id=None, warm_inject=warm,
        emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "dispatched"
    assert warm.calls == []
    assert len(verb.calls) == 1


def test_unresolved_session_takes_cold_path(tmp_path, monkeypatch):
    _patch_resolver(monkeypatch, None)
    warm = _WarmInject()
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW5", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sess-dead", warm_inject=warm,
        emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "dispatched"
    assert res.detail.startswith("cold: no-live-source-session")
    assert warm.calls == []
    assert len(verb.calls) == 1


def test_existing_marker_blocks_warm_inject_too(tmp_path, monkeypatch):
    _patch_resolver(monkeypatch, "sess-live-3")
    verb = _RunVerb()
    first = dispatch_post_merge_ritual(
        7, dedup_key="shaW6", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, emit_receipt_fn=_Receipts(),
    )
    assert first.outcome == "dispatched"
    warm = _WarmInject()
    second = dispatch_post_merge_ritual(
        7, dedup_key="shaW6", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sess-live-3", warm_inject=warm,
    )
    assert second.outcome == "already-dispatched"
    assert warm.calls == []
    assert len(verb.calls) == 1


def test_warm_resolver_error_degrades_to_cold(tmp_path, monkeypatch):
    """A resolver crash must never break the dispatch (fallback floor)."""
    import fno.post_merge_route as pmr

    def _boom(sid, harness=None):
        raise RuntimeError("resolver exploded")

    monkeypatch.setattr(pmr, "resolve_warm_session", _boom)
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW7", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sess-x", warm_inject=_WarmInject(),
        emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "dispatched"
    assert res.detail is not None and res.detail.startswith("cold: warm-error")
    assert len(verb.calls) == 1


def test_source_harness_threads_to_resolver_and_inject(tmp_path, monkeypatch):
    """The harness selects the live vehicle for both resolver and inject."""
    import fno.post_merge_route as pmr

    seen = {}

    def _resolver(sid, harness=None):
        seen["resolver"] = (sid, harness)
        return sid  # live

    monkeypatch.setattr(pmr, "resolve_warm_session", _resolver)
    warm = _WarmInject(delivered=True)
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        9, dedup_key="shaWH", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="codex-sess", source_harness="codex",
        warm_inject=warm, emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "routed-warm"
    assert seen["resolver"] == ("codex-sess", "codex")
    assert warm.calls == [("codex-sess", 9, "codex")]
    assert verb.calls == []


# --- receipt attribution: shipping vs borrowed (AC2-HP, AC3-HP) ---------


def test_warm_receipt_carries_delivering_and_borrowed(tmp_path, monkeypatch):
    """AC2-HP: a warm route names the shipping session AND the borrowed session
    it injects into -- the durable join PR #575 lacked."""
    _patch_resolver(monkeypatch, "S2")  # ship S1 is not warm-reachable; borrow S2
    warm = _WarmInject(delivered=True)
    rcpt = _Receipts()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaA2", auto_run=True, canonical_root=tmp_path,
        run_verb=_RunVerb(), ship_session_id="S1", ship_harness="codex",
        source_session_id="S2", warm_inject=warm, emit_receipt_fn=rcpt,
        node_id="x-1", repo_slug="o/r",
    )
    assert res.outcome == "routed-warm"
    acc = rcpt.last("accepted")
    assert acc["delivering_session_id"] == "S1"  # who shipped
    assert acc["delivering_harness"] == "codex"
    assert acc["borrowed_session_id"] == "S2"  # who runs the ritual
    assert acc["node_id"] == "x-1"
    assert acc["repo_slug"] == "o/r"
    assert acc["dispatch_id"] == "shaA2"


def test_cold_receipt_omits_borrowed_identity(tmp_path, monkeypatch):
    """A cold route records the delivering identity when known but never invents a borrowed one."""
    _patch_resolver(monkeypatch, None)
    rcpt = _Receipts()
    dispatch_post_merge_ritual(
        7, dedup_key="shaA3", auto_run=True, canonical_root=tmp_path,
        run_verb=_RunVerb(), ship_session_id="S1", ship_harness="claude",
        emit_receipt_fn=rcpt,
    )
    acc = rcpt.last("accepted")
    assert acc["route"] == "cold"
    assert acc["delivering_session_id"] == "S1"
    assert not acc.get("borrowed_session_id")  # omitted, not invented


# --- ritual claim guard (x-616b) -----------------------------------------


_RITUAL_TTL_MS = 15 * 60 * 1000


def _arm_ritual_claim(monkeypatch, tmp_path, pr_number, *, pid):
    import os

    from fno import claims
    from fno.claims.io import claims_root_for

    global_root = tmp_path / "global"
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(global_root))
    key = f"reconcile:pr-{pr_number}"
    claims.acquire_claim(
        key, f"postmerge:pr-{pr_number}:test", ttl_ms=_RITUAL_TTL_MS,
        pid=pid, root=claims_root_for(key),
    )
    return global_root


def test_ritual_claim_live_skips_and_persists_marker(tmp_path, monkeypatch):
    """A LIVE ritual claim means the verb is already running: skip, persist marker."""
    import os

    _arm_ritual_claim(monkeypatch, tmp_path, 401, pid=os.getpid())
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        401, dedup_key="sha401", auto_run=True, canonical_root=tmp_path, run_verb=verb,
    )
    assert res.outcome == "already-dispatched"
    assert res.detail == "ritual-claim-live"
    assert verb.calls == []
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "sha401").exists()
    # a subsequent tick short-circuits on the persisted marker, not the claim
    res2 = dispatch_post_merge_ritual(
        401, dedup_key="sha401", auto_run=True, canonical_root=tmp_path, run_verb=verb,
    )
    assert res2.outcome == "already-dispatched"
    assert res2.detail == "marker-exists"
    assert verb.calls == []


def test_ritual_claim_suspect_is_lock_contention_no_marker(tmp_path, monkeypatch):
    """A SUSPECT claim (dead holder pid) is lock-contention: no marker, retryable."""
    dead_pid = 2**31 - 1  # not a live process -> suspect
    _arm_ritual_claim(monkeypatch, tmp_path, 402, pid=dead_pid)
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        402, dedup_key="sha402", auto_run=True, canonical_root=tmp_path, run_verb=verb,
    )
    assert res.outcome == "already-dispatched"
    assert res.detail == "lock-contention"
    assert verb.calls == []
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "sha402").exists()


def test_ritual_claim_free_falls_through_to_verb(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        403, dedup_key="sha403", auto_run=True, canonical_root=tmp_path, run_verb=verb,
        emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "dispatched"
    assert verb.calls == [(403, str(tmp_path))]


def test_ritual_guard_failopen_when_root_resolution_raises(tmp_path, monkeypatch):
    """claim_status never raises, but claims_root_for can (no HOME). Fail open."""
    import fno.claims.io as cio

    def _boom(_key):
        raise RuntimeError("no home dir")

    monkeypatch.setattr(cio, "claims_root_for", _boom)
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        404, dedup_key="sha404", auto_run=True, canonical_root=tmp_path, run_verb=verb,
        emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "dispatched"
    assert verb.calls == [(404, str(tmp_path))]


# --- location: marker + verb cwd under canonical ------------------------


def test_node_cwd_resolves_target_canonical(tmp_path, monkeypatch):
    """A foreign-repo node resolves THAT repo's canonical from node_cwd."""
    import fno.paths as paths

    target = tmp_path / "repoB"
    target.mkdir()
    monkeypatch.setattr(paths, "resolve_canonical_worktree", lambda cwd=None: target)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    verb = _RunVerb()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaG", auto_run=True, node_cwd=str(tmp_path / "repoB-worktree"),
        run_verb=verb, emit_receipt_fn=_Receipts(),
    )
    assert res.outcome == "dispatched"
    assert (target / ".fno" / "post-merge-dispatched" / "shaG").exists()
    assert verb.calls[0][1] == str(target)


def test_marker_under_provided_canonical_not_cwd(tmp_path):
    canonical = tmp_path / "canonical"
    canonical.mkdir()
    verb = _RunVerb()
    dispatch_post_merge_ritual(
        7, dedup_key="shaF", auto_run=True, canonical_root=canonical,
        run_verb=verb, emit_receipt_fn=_Receipts(),
    )
    assert (canonical / ".fno" / "post-merge-dispatched" / "shaF").exists()
    assert verb.calls[0][1] == str(canonical)


# --- merge SHA threading (unchanged, in _reconcile) ---------------------


def test_query_parses_merge_sha():
    class _Res:
        returncode = 0
        stdout = (
            '{"number": 7, "state": "MERGED", "url": "u", '
            '"mergedAt": "t", "mergeCommit": {"oid": "cafef00d"}}'
        )
        stderr = ""

    def runner(cmd, **kw):
        assert "mergeCommit" in cmd[cmd.index("--json") + 1]
        return _Res()

    state = query_pr_merge_state(7, repo="o/r", runner=runner)
    assert state.merge_sha == "cafef00d"


def test_scan_threads_merge_sha_onto_record():
    entries = [
        {"id": "x-0001", "pr_number": 7, "pr_url": "https://github.com/o/r/pull/7"}
    ]

    def query(number, repo=None, cwd=None):
        return PrMergeState(
            number=number, state="MERGED", url="https://github.com/o/r/pull/7",
            merged_at="t", merge_sha="beefcafe",
        )

    records = scan_merge_drift(entries, query=query, list_merged=lambda **kw: [])
    closeable = [r for r in records if r.closeable]
    assert len(closeable) == 1
    assert closeable[0].merge_sha == "beefcafe"


# --- daemon adapter: _default_dispatch_ritual shells the verb ----------


class _Cand:
    def __init__(self, pr_number, repo_dir, source_session_id=None):
        self.pr_number = pr_number
        self.repo_dir = repo_dir
        self.source_session_id = source_session_id
        self.source_harness = None
        self.source_cwd = None
        self.ship_session_id = None
        self.ship_harness = None
        self.node_id = "x-1"
        self.repo_slug = "o/r"


class _Obs:
    def __init__(self, merge_sha=None):
        self.merge_sha = merge_sha


def _arm_auto_run(repo_dir):
    cfg = repo_dir / ".fno" / "config.toml"
    cfg.parent.mkdir(parents=True, exist_ok=True)
    cfg.write_text("[post_merge]\nauto_run = true\n")


def _patch_default_verb(monkeypatch, captured):
    """Redirect the cold verb runner to a recorder (no real subprocess)."""
    import fno.post_merge_route as pmr

    def _verb(pr, cwd):
        captured.append((pr, cwd))
        return pmr.ColdRitualResult(ok=True, tail="ok")

    monkeypatch.setattr(pmr, "_default_run_ritual_verb", _verb)


def test_default_dispatch_ritual_cold_verb_ok_marks(tmp_path, monkeypatch):
    """The daemon adapter runs the direct ritual verb; success marks the merge."""
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    _arm_auto_run(tmp_path)
    ran: list = []
    _patch_default_verb(monkeypatch, ran)
    res = _default_dispatch_ritual(_Cand(7, tmp_path), _Obs(merge_sha="shaD1"), None)
    assert res.outcome == "dispatched"
    assert ran == [(7, str(tmp_path))]
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaD1").exists()


def test_default_dispatch_ritual_cold_verb_notok_no_marker(tmp_path, monkeypatch):
    """A not-ok verb -> failed, no marker (the load-bearing retry invariant)."""
    import fno.post_merge_route as pmr
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    _arm_auto_run(tmp_path)
    monkeypatch.setattr(
        pmr, "_default_run_ritual_verb",
        lambda pr, cwd: pmr.ColdRitualResult(ok=False, tail="fail"),
    )
    res = _default_dispatch_ritual(_Cand(7, tmp_path), _Obs(merge_sha="shaD2"), None)
    assert res.outcome == "failed"
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaD2").exists()


def test_default_dispatch_ritual_respects_auto_run_off(tmp_path, monkeypatch):
    """No config -> auto_run off -> disabled, no verb, one deferred receipt (AC9)."""
    import fno.post_merge_route as pmr
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    ran: list = []
    _patch_default_verb(monkeypatch, ran)
    rcpt = _Receipts()
    monkeypatch.setattr(pmr, "emit_receipt", rcpt)
    res = _default_dispatch_ritual(_Cand(7, tmp_path), _Obs(merge_sha="shaOFF"), None)
    assert res.outcome == "disabled"
    assert ran == []
    assert rcpt.phases() == ["deferred"]
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaOFF").exists()


def test_cross_detector_one_handoff_per_sha(tmp_path, monkeypatch):
    """US3: a direct call and the daemon adapter converge on one per-SHA marker."""
    canonical = tmp_path / "canon"
    canonical.mkdir()
    _arm_auto_run(canonical)
    first = dispatch_post_merge_ritual(
        7, dedup_key="shaXD", auto_run=True, canonical_root=canonical,
        run_verb=_RunVerb(), emit_receipt_fn=_Receipts(),
    )
    assert first.outcome == "dispatched"
    ran: list = []
    _patch_default_verb(monkeypatch, ran)
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    second = _default_dispatch_ritual(_Cand(7, canonical), _Obs(merge_sha="shaXD"), None)
    assert second.outcome == "already-dispatched"
    assert ran == []  # the second detector ran nothing


# --- static regression: no bg / no merged LLM wrapper (AC10-EDGE) --------


def test_no_bg_or_merged_wrapper_in_production_post_merge():
    """AC10-EDGE: production post-merge code contains zero ``--substrate bg``,
    ``pr-merged-<n>`` worker names, or ``/fno:pr merged`` LLM wrappers, and the
    mechanical verb ``fno pr ritual`` is the cold path."""
    import inspect

    import fno.post_merge_route as pmr

    body = inspect.getsource(pmr)
    assert "--substrate bg" not in body
    assert "pr-merged-" not in body
    assert "fno pr ritual" in body  # the verb-first cold + warm command
    assert "/fno:pr merged" not in body  # no whole-ritual LLM wrapper


def test_sole_production_dispatch_entrypoint():
    """AC10-EDGE: exactly one production MERGED-to-ritual entrypoint. The seam is
    defined once in the leaf dispatch module, pr-watch is its sole production
    caller, and reconcile (both faces) cannot invoke it."""
    import inspect

    import fno.graph._reconcile as rec
    import fno.graph.cli as gcli
    import fno.post_merge_route as pmr
    import fno.pr_watch._dispatch as pwd

    assert hasattr(pmr, "dispatch_post_merge_ritual")  # defined once, in the leaf
    assert "dispatch_post_merge_ritual" in inspect.getsource(pwd)  # pr-watch calls it
    # Reconcile keeps its node-closure job but must not reach the dispatch seam.
    assert "dispatch_post_merge_ritual" not in inspect.getsource(rec)
    assert "dispatch_post_merge_ritual" not in inspect.getsource(gcli)
