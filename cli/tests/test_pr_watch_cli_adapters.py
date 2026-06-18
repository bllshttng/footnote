"""Tests for the CLI adapter layer in fno.pr_watch.cli.

All four adapters (_emit_event, _notify_parked, _reviewers_for, ClaimAdapter)
are extracted to module-level callables so they can be tested here without
exercising the full Typer CLI plumbing.

TDD: tests written BEFORE the extraction/fix so we watch them fail first,
then make them green.
"""
from __future__ import annotations

import json
import logging
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest


# ---------------------------------------------------------------------------
# AC1-HP: _emit_event writes a valid canonical event to events.jsonl
# ---------------------------------------------------------------------------


def test_emit_event_writes_real_event_to_events_jsonl(tmp_path: Path) -> None:
    """AC1-HP: _emit_event writes a real pr_watch_tick event; validate() accepts it.

    This would have caught bug #1: 'from fno.events.cli import emit_event' –
    emit_event does not exist on that module (only 'emit', a Typer command).
    The ImportError was swallowed by 'except Exception: pass', so events were
    silently dropped.
    """
    from fno.pr_watch.cli import _emit_event
    from fno.events import validate

    events_path = tmp_path / "events.jsonl"
    _emit_event("pr_watch_tick", {"open_prs": 0, "acted": 0}, events_path=events_path)

    assert events_path.exists(), "events.jsonl was not created"
    lines = events_path.read_text().strip().splitlines()
    assert len(lines) == 1, f"expected 1 event line, got {len(lines)}"

    event = json.loads(lines[0])
    # Verify the envelope: source must be 'daemon' (matching the schema)
    assert event["type"] == "pr_watch_tick"
    assert event["source"] == "daemon"
    assert event["data"]["open_prs"] == 0
    assert event["data"]["acted"] == 0
    # Must pass full schema validation (raises on failure)
    validate(event)


def test_emit_event_dispatched_writes_valid_event(tmp_path: Path) -> None:
    """AC1-HP: _emit_event works for pr_watch_dispatched with required fields."""
    from fno.pr_watch.cli import _emit_event
    from fno.events import validate

    events_path = tmp_path / "events.jsonl"
    _emit_event("pr_watch_dispatched", {"kind": "review", "pr": 42}, events_path=events_path)

    lines = events_path.read_text().strip().splitlines()
    event = json.loads(lines[0])
    assert event["type"] == "pr_watch_dispatched"
    assert event["source"] == "daemon"
    validate(event)


# ---------------------------------------------------------------------------
# AC2-ERR: _emit_event on write failure LOGS a warning and does not raise
# ---------------------------------------------------------------------------


def test_emit_event_logs_warning_on_write_failure(tmp_path: Path, caplog: pytest.LogCaptureFixture) -> None:
    """AC2-ERR: unwritable events path triggers a warning log, not a silent pass or raise."""
    from fno.pr_watch.cli import _emit_event

    # Unwritable path: a file where a directory is expected
    bad_path = tmp_path / "not-a-dir" / "events.jsonl"
    # Don't create the parent so mkdir will fail if parent doesn't exist
    # Actually parent needs to fail in a way that can't be mkdir'd
    # Point at a path whose parent is a FILE, not a dir
    blocker = tmp_path / "blocker"
    blocker.write_text("not a directory")
    bad_events_path = blocker / "events.jsonl"  # parent is a file -> mkdir fails

    with caplog.at_level(logging.WARNING, logger="fno.pr_watch.cli"):
        # Must NOT raise
        _emit_event("pr_watch_tick", {"open_prs": 0, "acted": 0}, events_path=bad_events_path)

    # Must have logged a warning
    assert any("pr-watch" in r.message and "emit" in r.message for r in caplog.records), (
        f"expected a warning log mentioning 'pr-watch' and 'emit'; got: {[r.message for r in caplog.records]}"
    )


# ---------------------------------------------------------------------------
# AC3-HP: _notify_parked calls send_notification with TWO positional args
# ---------------------------------------------------------------------------


def test_notify_parked_calls_send_notification_with_two_args() -> None:
    """AC3-HP: _notify_parked(message) calls send_notification('pr-watch', message).

    This catches bug #2: the old code called send_notification(message) with
    ONE arg, but the signature requires (title: str, message: str).
    """
    from fno.pr_watch.cli import _notify_parked

    with patch("fno.pr_watch.cli.send_notification") as mock_notify:
        mock_notify.return_value = (0, "")
        _notify_parked("PR #42 parked after 3 failed dispatch attempts")

    assert mock_notify.call_count == 1, "send_notification was not called"
    call_args = mock_notify.call_args
    positional = call_args[0]
    assert len(positional) == 2, (
        f"expected 2 positional args, got {len(positional)}: {positional}"
    )
    assert positional[0] == "pr-watch", f"expected first arg 'pr-watch', got {positional[0]!r}"
    assert "PR #42" in positional[1], f"message not in second arg: {positional[1]!r}"


def test_notify_parked_logs_warning_on_exception(caplog: pytest.LogCaptureFixture) -> None:
    """AC3-ERR: _notify_parked logs a warning instead of swallowing exceptions."""
    from fno.pr_watch.cli import _notify_parked

    with patch("fno.pr_watch.cli.send_notification", side_effect=RuntimeError("notify broken")):
        with caplog.at_level(logging.WARNING, logger="fno.pr_watch.cli"):
            _notify_parked("test message")  # must NOT raise

    assert any("pr-watch" in r.message for r in caplog.records), (
        f"expected warning log; got: {[r.message for r in caplog.records]}"
    )


# ---------------------------------------------------------------------------
# AC4-HP: _reviewers_for returns configured required_bots
# ---------------------------------------------------------------------------


def test_reviewers_for_returns_required_bots(tmp_path: Path) -> None:
    """AC4-HP: _reviewers_for returns config.review.required_bots for a repo dir."""
    from fno.pr_watch.cli import _reviewers_for

    fake_settings = MagicMock()
    fake_settings.config.review.required_bots = ["codex", "gemini"]

    with patch("fno.pr_watch.cli.load_settings_for_repo", return_value=fake_settings):
        result = _reviewers_for(tmp_path)

    assert result == ["codex", "gemini"]


def test_reviewers_for_returns_empty_list_and_logs_on_error(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    """AC4-ERR: _reviewers_for falls back to [] AND logs a warning on load_settings_for_repo failure.

    This catches bug #3: the old 'except Exception: return []' silently masked
    a broken settings.yaml, making review-dispatch invisibly disabled.
    """
    from fno.pr_watch.cli import _reviewers_for

    with patch("fno.pr_watch.cli.load_settings_for_repo", side_effect=ValueError("settings broken")):
        with caplog.at_level(logging.WARNING, logger="fno.pr_watch.cli"):
            result = _reviewers_for(tmp_path)

    assert result == [], f"expected [] fallback, got {result!r}"
    assert any("pr-watch" in r.message and "reviewer" in r.message for r in caplog.records), (
        f"expected warning log mentioning 'pr-watch' and 'reviewer'; got: {[r.message for r in caplog.records]}"
    )


# ---------------------------------------------------------------------------
# AC5-HP/EDGE: ClaimAdapter.is_node_live fails SAFE (returns True) on exception
# ---------------------------------------------------------------------------


def test_claim_adapter_is_node_live_returns_true_on_exception(
    caplog: pytest.LogCaptureFixture,
) -> None:
    """AC5-EDGE: ClaimAdapter.is_node_live returns True (fail-safe) on claim_status error.

    This catches bug #4: the old 'except Exception: return False' treated a
    claim-system error as 'node not live', risking double-dispatch onto a
    live-claimed node. A daemon must fail SAFE: treat errors as 'yes, it's live'.
    """
    from fno.pr_watch.cli import ClaimAdapter

    adapter = ClaimAdapter()

    with patch("fno.pr_watch.cli.claim_status", side_effect=OSError("claims broken")):
        with caplog.at_level(logging.WARNING, logger="fno.pr_watch.cli"):
            result = adapter.is_node_live("x-abc12345")

    assert result is True, (
        f"expected True (fail-safe) when claim_status raises, got {result!r}"
    )
    assert any("pr-watch" in r.message for r in caplog.records), (
        f"expected warning log; got: {[r.message for r in caplog.records]}"
    )


def test_claim_adapter_is_node_live_returns_true_when_live() -> None:
    """AC5-HP: ClaimAdapter.is_node_live returns True for a live node."""
    from fno.pr_watch.cli import ClaimAdapter

    adapter = ClaimAdapter()

    with patch("fno.pr_watch.cli.claim_status", return_value={"state": "live"}):
        assert adapter.is_node_live("x-abc12345") is True


def test_claim_adapter_is_node_live_returns_false_when_free() -> None:
    """AC5-HP: ClaimAdapter.is_node_live returns False when node is free/stale."""
    from fno.pr_watch.cli import ClaimAdapter

    adapter = ClaimAdapter()

    with patch("fno.pr_watch.cli.claim_status", return_value={"state": "free"}):
        assert adapter.is_node_live("x-abc12345") is False
