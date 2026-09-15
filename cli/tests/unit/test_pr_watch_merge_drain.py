"""Unit tests for the pr-watch merge drain (x-c65e).

Covers: _emit keeps the last receipt; the drain skips listed-closed rows,
records the merge core's reason on held/failed rows, stamps NOT_OPEN on an
already-terminal reply, raises the floor after a slow attempt, and counts
every granted queue row exactly once.
"""

import fno.pr._merge as _merge
import fno.pr_watch._dispatch as d
import pytest
from fno.pr_watch._state import WatermarkStore
from fno.pr_watch._discover import PrCandidate


def _store(tmp_path):
    return WatermarkStore(path=tmp_path / "state.json")


class _NoLockClaim:
    """A claim whose acquire_pr_lock never raises."""

    def acquire_pr_lock(self, key, holder):
        return None

    def release_pr_lock(self, key, holder):
        return None


class _LockingClaim(_NoLockClaim):
    """acquire_pr_lock raises for one named key (the locked row)."""

    def __init__(self, locked_key):
        self.locked_key = locked_key

    def acquire_pr_lock(self, key, holder):
        if key == self.locked_key:
            raise RuntimeError("locked")
        return None


def test_emit_keeps_the_last_receipt(capsys):
    _merge.LAST_RECEIPT.clear()
    _merge._emit(7, "held", "merge serialized", "none", err=False)
    capsys.readouterr()
    assert _merge.LAST_RECEIPT == {
        "pr": 7,
        "outcome": "held",
        "reason": "merge serialized",
        "strategy": "none",
    }


def _open_row(store, key):
    store.set(key, {"last_review_ts": None, "last_seen_state": "OPEN",
                    "merge_dispatched": False, "retries": 0, "parked": None})


def _grant_fields():
    return {"source": "config", "recorded_by": "spawner-session",
            "recorded_at": "2026-09-15T12:00:00Z"}


def _cand(tmp_path, pr, node_id):
    return d.PrCandidate(node_id=node_id, pr_number=pr, pr_url=None,
                         repo_dir=tmp_path, repo_slug="owner/repo")


def _drain(queue, events, claim=None, store_path=None):
    return d.run_execute_queue(
        queue, emit=lambda t, d_: events.append({"type": t, "data": d_}),
        notify=lambda *a, **k: None, max_retries=3,
        claim=claim or _NoLockClaim(), store_path=store_path,
    )


def test_a_listed_closed_row_is_skipped_with_a_reason(tmp_path, monkeypatch):
    events = []
    store_path = tmp_path / "state.json"
    store = _store(tmp_path)
    key = "owner/repo#2017"
    store.set(key, {"last_review_ts": None, "last_seen_state": "NOT_OPEN",
                    "merge_dispatched": False, "retries": 0, "parked": None})
    calls = []
    monkeypatch.setattr(
        _merge, "run_merge", lambda argv, cwd=None, **kw: calls.append(argv) or 0)
    counts = _drain([(_cand(tmp_path, 2017, "x-drain1"), key, _grant_fields())],
                    events, store_path=store_path)
    assert calls == [], "no merge call for a listed-closed row"
    assert [e for e in events if e["type"] == "merge_grant_execution"] == [], events
    skips = [e for e in events if e["type"] == "pr_watch_skipped"]
    assert [s["data"]["reason"] for s in skips] == ["not-open"], events
    assert counts == {"executed": 0, "held": 0, "failed": 0, "skipped": 1}


def test_a_held_row_records_the_merge_reason(tmp_path, monkeypatch):
    events = []
    store_path = tmp_path / "state.json"
    store = _store(tmp_path)
    key = "owner/repo#2049"
    _open_row(store, key)

    def _held(argv, cwd=None, **kw):
        _merge.LAST_RECEIPT.update(
            {"pr": int(argv[0]), "outcome": "held",
             "reason": "merge serialized: another merge holds the lock; retry",
             "strategy": "merge"})
        return 2

    monkeypatch.setattr(_merge, "run_merge", _held)
    counts = _drain([(_cand(tmp_path, 2049, "x-drain2"), key, _grant_fields())],
                    events, store_path=store_path)
    held = [e for e in events
            if e["type"] == "merge_grant_execution" and e["data"]["phase"] == "held"]
    assert held, events
    assert held[0]["data"]["reason"] == \
        "merge serialized: another merge holds the lock; retry", events
    assert counts["held"] == 1 and counts["executed"] == 0

    events.clear()
    monkeypatch.setattr(
        _merge, "run_merge",
        lambda argv, cwd=None, **kw: (_merge.LAST_RECEIPT.update(
            {"pr": int(argv[0]), "outcome": "failed", "reason": "gh merge failed",
             "strategy": "merge"}) or 1))
    key_f = "owner/repo#2050"
    _open_row(store, key_f)
    counts = _drain([(_cand(tmp_path, 2050, "x-drain2f"), key_f, _grant_fields())],
                    events, store_path=store_path)
    failed = [e for e in events
              if e["type"] == "merge_grant_execution" and e["data"]["phase"] == "failed"]
    assert failed, events
    assert failed[0]["data"]["reason"] == "gh merge failed", events
    assert failed[0]["data"]["exit_code"] == 1, events
    assert counts["failed"] == 1


def test_an_already_closed_reply_stamps_not_open_and_the_next_drain_skips(
        tmp_path, monkeypatch):
    events = []
    store_path = tmp_path / "state.json"
    store = _store(tmp_path)
    key = "owner/repo#2017"
    _open_row(store, key)
    calls = []

    def _already_closed(argv, cwd=None, **kw):
        calls.append(int(argv[0]))
        _merge.LAST_RECEIPT.update(
            {"pr": int(argv[0]), "outcome": "skipped",
             "reason": _merge.ALREADY_TERMINAL + "closed; nothing to merge",
             "strategy": "none"})
        return 2

    monkeypatch.setattr(_merge, "run_merge", _already_closed)
    cand = _cand(tmp_path, 2017, "x-drain3")
    counts = _drain([(cand, key, _grant_fields())], events, store_path=store_path)
    assert calls == [2017]
    assert store.get(key)["last_seen_state"] == "NOT_OPEN"
    assert counts["held"] == 1

    events.clear()
    counts = _drain([(cand, key, _grant_fields())], events, store_path=store_path)
    assert calls == [2017], "second drain calls no merge"
    skips = [e for e in events if e["type"] == "pr_watch_skipped"]
    assert [s["data"]["reason"] for s in skips] == ["not-open"], events
    assert counts["skipped"] == 1


def test_a_slow_attempt_raises_the_floor_for_the_next_row(tmp_path, monkeypatch):
    events = []
    merge_calls = []
    store_path = tmp_path / "state.json"
    store = _store(tmp_path)
    key1, key2 = "owner/repo#1", "owner/repo#2"
    _open_row(store, key1)
    _open_row(store, key2)

    now = [100.0]

    def _clock():
        return now[0]

    def _slow_then_fast(argv, cwd=None, **kw):
        merge_calls.append(int(argv[0]))
        _merge.LAST_RECEIPT.clear()
        took = 80.0 if merge_calls == [1] else 0.0
        now[0] += took
        return 0

    monkeypatch.setattr(d.time, "monotonic", _clock)
    monkeypatch.setattr(d, "_phase_deadline", _clock() + 150.0)
    monkeypatch.setattr(_merge, "run_merge", _slow_then_fast)
    c1, c2 = _cand(tmp_path, 1, "x-slow"), _cand(tmp_path, 2, "x-next")
    counts = _drain([(c1, key1, _grant_fields()), (c2, key2, _grant_fields())],
                    events, store_path=store_path)
    assert merge_calls == [1], "second row never starts an attempt"
    skips = [e for e in events if e["type"] == "pr_watch_skipped"]
    assert [s["data"]["reason"] for s in skips] == ["execute-budget"], events
    assert counts == {"executed": 1, "held": 0, "failed": 0, "skipped": 1}


def test_every_granted_row_is_counted_once(tmp_path, monkeypatch):
    events = []
    store_path = tmp_path / "state.json"
    store = _store(tmp_path)
    key_e, key_h, key_p, key_l = ("owner/repo#10", "owner/repo#11",
                                  "owner/repo#12", "owner/repo#13")
    _open_row(store, key_e)
    _open_row(store, key_h)
    store.set(key_p, {"last_review_ts": None, "last_seen_state": "OPEN",
                      "merge_dispatched": False, "retries": 0,
                      "parked": "waiting on ci"})
    store.set(key_l, {"last_review_ts": None, "last_seen_state": "OPEN",
                      "merge_dispatched": False, "retries": 0, "parked": None})
    replies = {"owner/repo#11": (2, "held reason", "held"),
               "owner/repo#10": (0, "", "")}

    def _merge_call(argv, cwd=None, **kw):
        rc, reason, outcome = replies["owner/repo#" + argv[0]]
        _merge.LAST_RECEIPT.update(
            {"pr": int(argv[0]), "outcome": outcome, "reason": reason,
             "strategy": "merge"})
        return rc

    monkeypatch.setattr(_merge, "run_merge", _merge_call)
    claim = _LockingClaim("pr-watch:owner/repo:13")
    queue = [
        (_cand(tmp_path, 13, "x-l"), key_l, _grant_fields()),
        (_cand(tmp_path, 12, "x-p"), key_p, _grant_fields()),
        (_cand(tmp_path, 11, "x-h"), key_h, _grant_fields()),
        (_cand(tmp_path, 10, "x-e"), key_e, _grant_fields()),
    ]
    counts = _drain(queue, events, claim=claim, store_path=store_path)
    total = counts["executed"] + counts["held"] + counts["failed"] + counts["skipped"]
    assert total == 4, counts
    locked = [e for e in events if e["type"] == "pr_watch_skipped"
              and e["data"].get("reason") == "locked"]
    assert locked and locked[0]["data"]["pr"] == 13, events
