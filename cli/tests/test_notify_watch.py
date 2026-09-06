"""Tests for the operator-notice lane (x-5f06): signal dedupe, the
notify_watch pass, and the honest multi-channel return of send_notification.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.notify import _signal
from fno.pr_watch import _notify_watch


@pytest.fixture(autouse=True)
def _signal_state(tmp_path, monkeypatch):
    """Point the dedupe map inside the sandbox."""
    monkeypatch.setattr(
        "fno.paths.notify_signals_json", lambda: tmp_path / "notify-signals.json"
    )


@pytest.fixture(autouse=True)
def _fake_send(monkeypatch):
    """Record sends at the chokepoint; the toast and journal never fire."""
    calls: list = []

    def _send(title, message, pointer=""):
        calls.append({"title": title, "body": message, "pointer": pointer})
        return 0, ""

    monkeypatch.setattr("fno.notify._impl.send_notification", _send)
    return calls


def _settings(min_interval_s=300):
    return SimpleNamespace(notify=SimpleNamespace(min_interval_s=min_interval_s))


def test_same_token_dedupes_and_sends_nothing(_fake_send):
    assert _signal.notify_signal("k", "t1", "T", "B", "p") == (0, "sent")
    assert len(_fake_send) == 1
    assert _signal.notify_signal("k", "t1", "T", "B", "p") == (0, "deduped")
    assert len(_fake_send) == 1


def test_changed_token_inside_floor_is_held_not_dropped(monkeypatch, _fake_send):
    monkeypatch.setattr("fno.config.load_settings", lambda: _settings(3600))
    assert _signal.notify_signal("k", "t1", "T", "B", "p") == (0, "sent")
    code, verdict = _signal.notify_signal("k", "t2", "T", "B", "p")
    assert (code, verdict) == (0, "rate-held")
    assert len(_fake_send) == 1
    # The token was NOT written: the next pass after the floor reconsiders t2.
    from fno.paths import notify_signals_json

    stored = json.loads(notify_signals_json().read_text())
    assert stored["k"]["token"] == "t1"


def test_changed_token_after_floor_sends(monkeypatch, _fake_send):
    monkeypatch.setattr("fno.config.load_settings", lambda: _settings(0))
    assert _signal.notify_signal("k", "t1", "T", "B", "p")[1] == "sent"
    assert _signal.notify_signal("k", "t2", "T", "B", "p") == (0, "sent")
    assert len(_fake_send) == 2


def test_failed_send_rolls_back_so_the_next_pass_retries(monkeypatch, _fake_send):
    monkeypatch.setattr(
        "fno.notify._impl.send_notification", lambda *a, **k: (1, "no channel")
    )
    code, verdict = _signal.notify_signal("k", "t1", "T", "B", "p")
    assert code == 1
    from fno.paths import notify_signals_json

    assert notify_signals_json().exists() is False or "k" not in json.loads(
        notify_signals_json().read_text() or "{}"
    )


def test_forget_lets_the_next_change_send(_fake_send):
    _signal.notify_signal("k", "t1", "T", "B", "p")
    _signal.forget("k")
    assert _signal.notify_signal("k", "t1", "T", "B", "p") == (0, "sent")
    assert len(_fake_send) == 2


def _board_payload(counts: dict):
    rows = {
        "operator_question": [{"id": "q-1", "question": "SECRET QUESTION TEXT"}],
        "mergeable_pr": [{"number": 1491, "title": "SECRET PR TITLE"}],
        "undriven_pr": [{"id": "x-1"}],
    }
    return {
        "queues": [
            {
                "name": name,
                "count": count,
                "rows": [] if count == 0 else rows.get(name, []),
            }
            for name, count in counts.items()
        ]
    }


SIGNALS = ["operator_question", "mergeable_pr", "undriven_pr", "crown_set"]


def test_pass_notifies_once_with_count_and_pointer(monkeypatch, _fake_send):
    monkeypatch.setattr(_notify_watch, "_board", lambda: _board_payload(
        {"operator_question": 3, "mergeable_pr": 2, "undriven_pr": 1}
    ))
    monkeypatch.setattr("fno.agents.court.gather_court",
                        lambda: {"crowns": [{"scope": "s", "holder": "h", "status": "live"}]})

    acted, skip, detail = _notify_watch.run_notify_watch(_settings(), SIGNALS)

    assert acted == 4
    assert skip is None
    q = _fake_send[0]
    assert q["body"].startswith("3 open operator question(s)")
    assert q["pointer"] == "fno inbox outstanding"
    assert "SECRET" not in json.dumps(_fake_send)


def test_second_pass_is_all_deduped(monkeypatch, _fake_send):
    monkeypatch.setattr(_notify_watch, "_board", lambda: _board_payload(
        {"operator_question": 3, "mergeable_pr": 2, "undriven_pr": 1}
    ))
    monkeypatch.setattr("fno.agents.court.gather_court",
                        lambda: {"crowns": [{"scope": "s", "holder": "h", "status": "live"}]})
    _notify_watch.run_notify_watch(_settings(), SIGNALS)
    acted, skip, _ = _notify_watch.run_notify_watch(_settings(), SIGNALS)
    assert acted == 0
    assert skip is None


def test_drained_queue_is_forgotten_not_announced(monkeypatch, _fake_send):
    monkeypatch.setattr(_notify_watch, "_board", lambda: _board_payload(
        {"operator_question": 3, "mergeable_pr": 0, "undriven_pr": 0}
    ))
    acted, skip, detail = _notify_watch.run_notify_watch(
        _settings(), ["operator_question", "mergeable_pr"]
    )
    assert acted == 1
    assert skip is None
    assert "mergeable_pr:clear" in detail
    assert len(_fake_send) == 1


def test_drained_queue_detail(monkeypatch, _fake_send):
    monkeypatch.setattr(_notify_watch, "_board", lambda: _board_payload(
        {"operator_question": 0, "mergeable_pr": 0, "undriven_pr": 0}
    ))
    acted, skip, detail = _notify_watch.run_notify_watch(
        _settings(), ["operator_question", "mergeable_pr", "undriven_pr"]
    )
    assert acted == 0
    assert skip is None
    assert detail.count(":clear") == 3


def test_gh_absent_skips_only_main_ci(monkeypatch, _fake_send):
    monkeypatch.setattr(_notify_watch.shutil, "which", lambda _n: None)
    monkeypatch.setattr(_notify_watch, "_board", lambda: _board_payload(
        {"operator_question": 1}
    ))
    acted, skip, detail = _notify_watch.run_notify_watch(
        _settings(), ["operator_question", "main_ci"]
    )
    assert acted == 1
    assert skip == "gh_absent"
    assert "main_ci:gh_absent" in detail


def test_main_ci_flips_token_on_conclusion_change(monkeypatch, _fake_send):
    monkeypatch.setattr(
        _notify_watch,
        "_run",
        lambda cmd, timeout=60, cwd=None: {
            "git": "git@github.com:owner/repo.git",
            "gh": json.dumps({"check_runs": [{"conclusion": "failure"}]}),
        }[cmd[0]],
    )
    acted, skip, _ = _notify_watch.run_notify_watch(
        _settings(), ["main_ci"], roots=[Path("/any/repo")]
    )
    assert acted == 1 and skip is None
    send = _fake_send[0]
    assert send["body"] == "main CI on owner/repo: failure."
    assert send["pointer"] == "https://github.com/owner/repo/actions"

    # Same state: deduped. Then green inside the floor: held, not dropped.
    monkeypatch.setattr(
        _notify_watch,
        "_run",
        lambda cmd, timeout=60, cwd=None: {
            "git": "git@github.com:owner/repo.git",
            "gh": json.dumps({"check_runs": [{"conclusion": "success"}]}),
        }[cmd[0]],
    )
    assert _notify_watch.run_notify_watch(
        _settings(3600), ["main_ci"], roots=[Path("/any/repo")]
    )[0] == 0


def test_main_ci_pending_run_is_not_a_state(monkeypatch, _fake_send):
    monkeypatch.setattr(
        _notify_watch,
        "_run",
        lambda cmd, timeout=60, cwd=None: {
            "git": "git@github.com:owner/repo.git",
            "gh": json.dumps({"check_runs": [{"conclusion": None}]}),
        }[cmd[0]],
    )
    acted, _, detail = _notify_watch.run_notify_watch(
        _settings(), ["main_ci"], roots=[Path("/any/repo")]
    )
    assert acted == 0
    assert "unreadable" in detail
    assert _fake_send == []
