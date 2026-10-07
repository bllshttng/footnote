"""The pr-watch merge arm refuses an off-list PR under the scoped merge freeze.

The role writes one record (subject + allow-list) through the
authorized-merge verb's freeze ops; the Rust gate is the authoritative
reader, and the arm's executor asks it through one thin receipt call. The
verdict mapping lives here; the record read is the Rust tests'.
"""
from __future__ import annotations

from fno.pr_watch._dispatch import merge_freeze_refusal, run_execute_queue


def _door(monkeypatch, receipt=None, error=None):
    import fno.rust_binary as rb

    seen = {}

    def _call(verb, args, *, timeout=None):
        seen["verb"] = verb
        seen["args"] = args
        return (error, receipt)

    monkeypatch.setattr(rb, "call_binary_json", _call, raising=True)
    return seen


CLEAR = {"outcome": "clear", "exit_code": 0, "detail": ""}
FROZEN = {"outcome": "frozen", "exit_code": 0, "detail": "rc freeze role"}


def test_the_freeze_verdict_contract(monkeypatch):
    # A clear receipt reads none; the ask is one freeze-check op.
    seen = _door(monkeypatch, receipt=CLEAR)
    assert merge_freeze_refusal(42) is None
    assert seen["verb"] == "authorized-merge"
    assert '{"op": "freeze-check", "pr": 42}' in seen["args"][0]
    # An off-list PR refuses naming the freeze.
    _door(monkeypatch, receipt=FROZEN)
    why = merge_freeze_refusal(2500)
    assert why is not None
    assert "rc freeze" in why and "2500" in why
    # A failed check and an unreadable receipt both refuse fail-closed.
    _door(monkeypatch, error="fno-agents binary not found")
    assert "unavailable" in (merge_freeze_refusal(42) or "")
    _door(monkeypatch, receipt=[1, 2])
    assert "unavailable" in (merge_freeze_refusal(42) or "")
    _queue_contract(monkeypatch)


# --- the arm's queue ----------------------------------------------------------


class _Cand:
    def __init__(self, pr):
        self.pr_number = pr
        self.node_id = "x-test"
        self.repo_slug = "o/r"
        self.repo_dir = "/tmp"


def _run_queue(monkeypatch, entries, pr, receipt=None):
    """Run one queue row through the executor with the merge stubbed."""
    receipts = []

    def _emit(kind, data):
        receipts.append((kind, data))
        return True

    state = {"entries": dict(entries)}

    class _Store:
        def __init__(self, path=None):
            self._entries = state["entries"]

        def get(self, key):
            return self._entries.get(key)

        def set(self, key, entry):
            self._entries[key] = entry
            state["entries"] = self._entries

    class _NoLock:
        def acquire_pr_lock(self, key, holder):
            return None

    monkeypatch.setattr(
        "fno.pr_watch._state.WatermarkStore", _Store, raising=True
    )
    monkeypatch.setattr(
        "fno.pr_watch._dispatch.phase_seconds_left", lambda: 600, raising=True
    )
    monkeypatch.setattr(
        "fno.pr_watch._dispatch._gh_budget_backoff_left", lambda: 0.0, raising=True
    )
    _door(monkeypatch, receipt=receipt if receipt is not None else CLEAR)

    def _fake_merge(argv, cwd=None, *, authority="", timeout_s=0.0):
        from fno.pr import _merge as m

        m.LAST_RECEIPT.clear()
        return 0

    monkeypatch.setattr("fno.pr._merge.run_merge", _fake_merge, raising=True)
    counts = run_execute_queue(
        [(_Cand(pr), f"o/r#{pr}", {})],
        store_path=None,
        emit=_emit,
        notify=lambda message, **_kw: None,
        max_retries=0,
        claim=_NoLock(),
    )
    return counts, receipts


def _queue_contract(monkeypatch):
    counts, receipts = _run_queue(
            monkeypatch,
            {"o/r#2500": {"last_seen_state": "OPEN", "retries": 0}},
            2500,
            receipt=FROZEN,
        )
    assert counts["held"] == 1
    assert counts["executed"] == 0
    held = [d for k, d in receipts if k == "merge_grant_execution" and d.get("phase") == "held"]
    assert held and "rc freeze" in str(held[-1].get("reason"))
    skipped = [d for k, d in receipts if k == "pr_watch_skipped"]
    assert skipped and skipped[-1].get("reason") == "merge-freeze"
    counts, _ = _run_queue(
        monkeypatch, {"o/r#2500": {"last_seen_state": "OPEN", "retries": 0}}, 2500
    )
    assert counts["executed"] == 1
    assert counts["held"] == 0
