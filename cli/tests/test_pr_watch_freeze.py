"""The pr-watch merge arm skips under a merge freeze (x-b553 breach 3).

PR 2731 merged at 15:07:45Z while its worker held: the arm merged any green
covered PR with no freeze check. The arm now reads the fleet breaker's merges
verdict before its queue and skips the whole phase when a stop holds merges
(or the breaker is unreadable, fail closed); per-PR crown holds still refuse
inside the merge primitive.
"""
from __future__ import annotations

import json
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from fno.pr_watch.cli import _merges_freeze_frozen


def _check_proc(returncode: int, stdout: str = ""):
    proc = SimpleNamespace(returncode=returncode, stdout=stdout, stderr="")
    return proc


class TestMergesFreezeVerdict:
    def test_a_merges_stop_freezes_naming_the_reason(self, monkeypatch):
        def fake_run(cmd, **kw):
            assert "fleet-incident" in cmd and "check" in cmd
            return _check_proc(90, "fleet incident: stopped (generation 21)\n")

        monkeypatch.setattr("subprocess.run", fake_run)
        frozen, why = _merges_freeze_frozen()
        assert frozen is True
        assert "generation 21" in why

    def test_an_unreadable_breaker_freezes_fail_closed(self, monkeypatch):
        monkeypatch.setattr("subprocess.run", lambda cmd, **kw: _check_proc(91))
        frozen, why = _merges_freeze_frozen()
        assert frozen is True
        assert "unreadable" in why

    def test_a_clear_breaker_reads_clear(self, monkeypatch):
        monkeypatch.setattr("subprocess.run", lambda cmd, **kw: _check_proc(0))
        frozen, why = _merges_freeze_frozen()
        assert frozen is False
        assert why == ""

    def test_a_dead_check_freezes_fail_closed(self, monkeypatch):
        def boom(cmd, **kw):
            raise OSError("binary vanished")

        monkeypatch.setattr("subprocess.run", boom)
        frozen, why = _merges_freeze_frozen()
        assert frozen is True
        assert "refusing to merge" in why

    def test_no_binary_reads_clear_and_defers_to_the_per_pr_gate(self, monkeypatch):
        import fno.rust_binary as rb

        monkeypatch.setattr(rb, "find_dev_binary", lambda: None)
        monkeypatch.setattr(rb, "resolve_binary", lambda: None)
        frozen, why = _merges_freeze_frozen()
        assert frozen is False
        assert "per-PR gate" in why


# --- the arm row -------------------------------------------------------------

def _arm_settings():
    return SimpleNamespace(
        pr_watch=SimpleNamespace(
            enabled=True,
            interval_seconds=600,
            tick_timeout_seconds=None,
            max_age_days=14,
            retries=2,
            graphql_min_remaining=200,
            model="m",
        ),
        review=SimpleNamespace(github_apps=[], required_bots=[]),
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(enabled=False, watchdog=SimpleNamespace(enabled=False, mode="report")),
        king=SimpleNamespace(wake_enabled=False, wake_debounce_seconds=900),
        auto_heal=SimpleNamespace(enabled=False),
    )


def test_merge_arm_skips_frozen_and_never_built_a_queue(tmp_path, monkeypatch):
    """A frozen arm emits one tick row naming the freeze and never asks the
    grant-queue verb for work."""
    fake_home = tmp_path / "home"
    (fake_home / ".fno").mkdir(parents=True)
    monkeypatch.setenv("HOME", str(fake_home))
    monkeypatch.setenv("PR_WATCH_FIRE_CMD", "true")
    monkeypatch.setattr("time.time", lambda: 1201.0)

    def refuse_verb_call(verb, payload, timeout=None):
        raise AssertionError(f"grant-queue must not run under a freeze: {verb} {payload}")

    with (
        patch("fno.pr_watch.cli.load_settings", return_value=_arm_settings()),
        patch("fno.pr_watch.cli.claim_status", return_value={"state": "free"}),
        patch("fno.claims.acquire_claim"),
        patch("fno.claims.release_claim"),
        patch("fno.rust_binary.verb_call", side_effect=refuse_verb_call),
        patch(
            "fno.pr_watch.cli._merges_freeze_frozen",
            return_value=(True, "fleet incident: stopped (generation 21)"),
        ),
    ):
        from fno.pr_watch.cli import tick

        tick()

    from tests._event_rows import event_rows as _event_rows

    rows = [
        r
        for r in _event_rows(fake_home / ".fno" / "events.jsonl")
        if r.get("type") == "control_plane_tick" and r["data"].get("arm") == "pr_watch_merge"
    ]
    assert rows, "expected a merge arm row"
    data = rows[-1]["data"]
    assert data["skip_reason"] == "frozen"
    assert "generation 21" in data["detail"]
    assert data["acted"] == 0


def test_merge_arm_clear_breaker_runs_the_queue(tmp_path, monkeypatch):
    """A clear breaker changes nothing: the phase proceeds past the gate (and
    with no granted queue, lands its ordinary no-work row)."""
    fake_home = tmp_path / "home"
    (fake_home / ".fno").mkdir(parents=True)
    monkeypatch.setenv("HOME", str(fake_home))
    monkeypatch.setenv("PR_WATCH_FIRE_CMD", "true")
    monkeypatch.setattr("time.time", lambda: 1201.0)

    queue_calls = []

    def fake_verb_call(verb, payload, timeout=None):
        queue_calls.append(verb)
        return {"error": None, "queue": [], "verdicts": {}, "candidates": 0, "elapsed_ms": 1}

    with (
        patch("fno.pr_watch.cli.load_settings", return_value=_arm_settings()),
        patch("fno.pr_watch.cli.claim_status", return_value={"state": "free"}),
        patch("fno.claims.acquire_claim"),
        patch("fno.claims.release_claim"),
        patch("fno.rust_binary.verb_call", side_effect=fake_verb_call),
        patch("fno.pr_watch.cli._merges_freeze_frozen", return_value=(False, "")),
    ):
        from fno.pr_watch.cli import tick

        tick()

    assert "authorized-merge" in queue_calls
    from tests._event_rows import event_rows as _event_rows

    rows = [
        r
        for r in _event_rows(fake_home / ".fno" / "events.jsonl")
        if r.get("type") == "control_plane_tick" and r["data"].get("arm") == "pr_watch_merge"
    ]
    assert rows
    # A normal run row omits skip_reason entirely; the point is it is not frozen.
    assert rows[-1]["data"].get("skip_reason") != "frozen"
