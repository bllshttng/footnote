"""The pr-watch merge arm refuses an off-list PR under the scoped merge freeze.

The crown writes one record (subject + allow-list) through the
authorized-merge verb's freeze ops; the arm's executor reads it for the
per-PR early skip, and the merge owner's Rust gate is the authoritative
reader on every merge path.
"""
from __future__ import annotations

import json

from fno.pr_watch._dispatch import merge_freeze_refusal, run_execute_queue


def _record(tmp_path, record):
    home = tmp_path / "agents"
    home.mkdir(exist_ok=True)
    if record is not None:
        (home / "merge-freeze.json").write_text(json.dumps(record))
    return home


def _patch_home(monkeypatch, home):
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "agents_home_dir", lambda: home)


class TestMergeFreezeVerdict:
    def test_no_record_reads_clear(self, tmp_path, monkeypatch):
        _patch_home(monkeypatch, _record(tmp_path, None))
        assert merge_freeze_refusal(42) is None

    def test_an_off_list_pr_refuses_naming_the_freeze(self, tmp_path, monkeypatch):
        _patch_home(
            monkeypatch,
            _record(
                tmp_path,
                {"version": 1, "subject": "rc freeze", "set_by": "crown", "allow": [2739]},
            ),
        )
        why = merge_freeze_refusal(2500)
        assert why is not None
        assert "rc freeze" in why
        assert "2500" in why
        assert merge_freeze_refusal(2739) is None

    def test_an_unreadable_record_refuses_fail_closed(self, tmp_path, monkeypatch):
        home = _record(tmp_path, None)
        _patch_home(monkeypatch, home)
        (home / "merge-freeze.json").write_text("{")
        why = merge_freeze_refusal(42)
        assert why is not None and "unreadable" in why

    def test_a_wrong_version_refuses_fail_closed(self, tmp_path, monkeypatch):
        _patch_home(monkeypatch, _record(tmp_path, {"version": 99, "subject": "s", "allow": []}))
        assert merge_freeze_refusal(42) is not None


# --- the arm's queue ----------------------------------------------------------


class _Cand:
    def __init__(self, pr):
        self.pr_number = pr
        self.node_id = "x-test"
        self.repo_slug = "o/r"
        self.repo_dir = "/tmp"


class _Store:
    def __init__(self, path=None):
        self._entries = {}


def _run_queue(monkeypatch, entries, pr):
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


class TestQueueSkipsUnderFreeze:
    def test_an_off_list_pr_skips_with_a_receipt_naming_the_freeze(
        self, tmp_path, monkeypatch
    ):
        _patch_home(
            monkeypatch,
            _record(
                tmp_path,
                {"version": 1, "subject": "rc freeze", "set_by": "crown", "allow": [2739]},
            ),
        )
        counts, receipts = _run_queue(
            monkeypatch, {"o/r#2500": {"last_seen_state": "OPEN", "retries": 0}}, 2500
        )
        assert counts["held"] == 1
        assert counts["executed"] == 0
        held = [d for k, d in receipts if k == "merge_grant_execution" and d.get("phase") == "held"]
        assert held and "rc freeze" in str(held[-1].get("reason"))
        skipped = [d for k, d in receipts if k == "pr_watch_skipped"]
        assert skipped and skipped[-1].get("reason") == "merge-freeze"

    def test_no_freeze_runs_the_merge(self, tmp_path, monkeypatch):
        _patch_home(monkeypatch, _record(tmp_path, None))
        counts, receipts = _run_queue(
            monkeypatch, {"o/r#2500": {"last_seen_state": "OPEN", "retries": 0}}, 2500
        )
        assert counts["executed"] == 1
        assert counts["held"] == 0
