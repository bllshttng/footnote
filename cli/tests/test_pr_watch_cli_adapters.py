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

pytestmark = pytest.mark.usefixtures("_no_global_tick_events")


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
    _emit_event(
        "pr_watch_tick",
        {"open_prs": 0, "acted": 0, "swept_count": 0, "swept": {},
         "dropped_count": 0, "dropped": {}},
        events_path=events_path,
    )

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

    # Don't create the parent so mkdir will fail if parent doesn't exist
    # Actually parent needs to fail in a way that can't be mkdir'd
    # Point at a path whose parent is a FILE, not a dir
    blocker = tmp_path / "blocker"
    blocker.write_text("not a directory")
    bad_events_path = blocker / "events.jsonl"  # parent is a file -> mkdir fails

    with caplog.at_level(logging.WARNING, logger="fno.pr_watch.cli"):
        # Must NOT raise
        result = _emit_event(
            "pr_watch_tick",
            {"open_prs": 0, "acted": 0, "swept_count": 0, "swept": {},
             "dropped_count": 0, "dropped": {}},
            events_path=bad_events_path,
        )

    assert result is False
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
# AC4-HP: _reviewers_for returns configured github_apps
# ---------------------------------------------------------------------------


def test_reviewers_for_returns_required_bots(tmp_path: Path) -> None:
    """AC4-HP: _reviewers_for returns config.review.github_apps for a repo dir.

    (github_apps is the canonical field after the x-4baa rename; required_bots
    is a legacy alias resolved into it.)
    """
    from fno.pr_watch.cli import _reviewers_for

    fake_settings = MagicMock()
    fake_settings.review.github_apps = ["codex", "gemini"]

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


# ---------------------------------------------------------------------------
# AC6: the tick command's printed line (the launchd out.log surface)
# ---------------------------------------------------------------------------
#
# These drive fno.pr_watch.cli.tick() -- the Typer command launchd actually
# invokes -- not the _dispatch.tick() function. The whole incident was that
# the PRINTED line could not distinguish a wedged tick from an empty sweep,
# and that line exists only in the command.


def _run_tick_command(monkeypatch, result):
    """Invoke the Typer tick command with a stubbed dispatch result."""
    import typer
    from typer.testing import CliRunner

    from fno.pr_watch import cli as prcli

    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick", lambda **_kw: result, raising=True
    )

    settings = MagicMock()
    settings.pr_watch.max_age_days = 30
    settings.pr_watch.retries = 3
    settings.recovery.enabled = False
    monkeypatch.setattr(prcli, "load_settings", lambda: settings, raising=True)

    app = typer.Typer()
    app.command()(prcli.tick)
    return CliRunner().invoke(app, [])


def test_AC6_lock_held_tick_prints_holder_and_no_counts(monkeypatch) -> None:
    """A wedged tick must name the holder and never print open_prs=."""
    from fno.pr_watch._dispatch import TickResult

    res = _run_tick_command(
        monkeypatch,
        TickResult(
            open_prs=0, acted=0, lock_held=True, lock_holder="lock held by pr-watch:4242"
        ),
    )

    assert res.exit_code == 0, res.output
    assert "lock held by pr-watch:4242" in res.output
    assert "open_prs=" not in res.output, (
        f"a lock-held tick must not read as a sweep; got: {res.output!r}"
    )


def test_AC6_healthy_tick_still_prints_counts(monkeypatch) -> None:
    """The healthy line is unchanged -- lock_held is purely additive."""
    from fno.pr_watch._dispatch import TickResult

    res = _run_tick_command(
        monkeypatch, TickResult(open_prs=7, acted=2, skipped=1)
    )

    assert res.exit_code == 0, res.output
    assert "open_prs=7 acted=2 skipped=1" in res.output
    assert "lock held" not in res.output


def test_config_disabled_tick_prints_disabled_not_a_sweep(monkeypatch) -> None:
    """x-aaaf wave 2: a disabled tick must not read as an empty sweep either."""
    from fno.pr_watch._dispatch import TickResult

    res = _run_tick_command(monkeypatch, TickResult(open_prs=0, acted=0, disabled=True))

    assert res.exit_code == 0, res.output
    assert "config.pr_watch.enabled is false" in res.output
    assert "open_prs=" not in res.output


def test_master_switch_off_names_autonomy_not_pr_watch_in_the_message(monkeypatch) -> None:
    """x-aaaf wave 3: when the panic switch (not pr_watch's own gate) caused
    the skip, the printed line must name IT, not the narrower gate."""
    import typer
    from typer.testing import CliRunner

    from fno.pr_watch import cli as prcli
    from fno.pr_watch._dispatch import TickResult

    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick", lambda **_kw: TickResult(open_prs=0, acted=0, disabled=True),
        raising=True,
    )
    settings = MagicMock()
    settings.pr_watch.max_age_days = 30
    settings.pr_watch.retries = 3
    settings.pr_watch.enabled = True
    settings.autonomy.enabled = False
    settings.recovery.enabled = False
    monkeypatch.setattr(prcli, "load_settings", lambda: settings, raising=True)

    app = typer.Typer()
    app.command()(prcli.tick)
    res = CliRunner().invoke(app, [])

    assert res.exit_code == 0, res.output
    assert "config.autonomy.enabled is false" in res.output


def test_master_switch_off_also_stops_the_recovery_sweep(monkeypatch) -> None:
    """x-aaaf wave 3: config.recovery.enabled=True alone used to be enough to
    fire the respawn sweep even with the master switch off -- a guard checked
    independently of the panic switch it was supposed to obey. Assert the
    sweep function is never called when autonomy.enabled is False, even
    though recovery's own gate is armed."""
    import typer
    from typer.testing import CliRunner

    from fno.pr_watch import cli as prcli
    from fno.pr_watch._dispatch import TickResult

    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick",
        lambda **_kw: TickResult(open_prs=0, acted=0, disabled=True),
        raising=True,
    )
    settings = MagicMock()
    settings.pr_watch.max_age_days = 30
    settings.pr_watch.retries = 3
    settings.pr_watch.enabled = True
    settings.autonomy.enabled = False
    settings.recovery.enabled = True
    monkeypatch.setattr(prcli, "load_settings", lambda: settings, raising=True)

    called = []
    monkeypatch.setattr(
        "fno.recovery.run_recovery_sweep", lambda *a, **kw: called.append(1), raising=True
    )

    app = typer.Typer()
    app.command()(prcli.tick)
    res = CliRunner().invoke(app, [])

    assert res.exit_code == 0, res.output
    assert not called, "recovery sweep must not fire when the master switch is off"


def test_cli_passes_the_resolved_enabled_flag_to_dispatch_tick(monkeypatch) -> None:
    """The CLI must pass config.pr_watch.enabled through, not silently drop it."""
    import typer
    from typer.testing import CliRunner

    from fno.pr_watch import cli as prcli
    from fno.pr_watch._dispatch import TickResult

    captured: dict = {}

    def _fake_tick(**kw):
        captured.update(kw)
        return TickResult(open_prs=0, acted=0)

    monkeypatch.setattr("fno.pr_watch._dispatch.tick", _fake_tick, raising=True)
    settings = MagicMock()
    settings.pr_watch.max_age_days = 30
    settings.pr_watch.retries = 3
    settings.pr_watch.enabled = False
    settings.recovery.enabled = False
    monkeypatch.setattr(prcli, "load_settings", lambda: settings, raising=True)

    app = typer.Typer()
    app.command()(prcli.tick)
    CliRunner().invoke(app, [])

    assert captured["enabled"] is False


def test_AC6_subsystem_failure_is_not_reported_as_a_held_lock(monkeypatch) -> None:
    """A claims failure must not masquerade as routine contention."""
    from fno.pr_watch._dispatch import TickResult

    res = _run_tick_command(
        monkeypatch,
        TickResult(
            open_prs=0,
            acted=0,
            lock_held=True,
            lock_holder="tick lock unavailable: [Errno 28] No space left on device",
        ),
    )

    assert res.exit_code == 0, res.output
    assert "unavailable" in res.output
    assert "No space left on device" in res.output
    assert "open_prs=" not in res.output


def test_failed_tick_exits_nonzero_without_killing_composed_legs(monkeypatch) -> None:
    """A raised tick fails the command but never the recovery and sync legs."""
    import typer
    from typer.testing import CliRunner

    from fno.pr_watch import cli as prcli

    def _boom(**_kw):
        raise RuntimeError("pr-watch tick receipt emission failed")

    monkeypatch.setattr("fno.pr_watch._dispatch.tick", _boom, raising=True)

    settings = MagicMock()
    settings.pr_watch.max_age_days = 30
    settings.pr_watch.retries = 3
    settings.recovery.enabled = False
    monkeypatch.setattr(prcli, "load_settings", lambda: settings, raising=True)

    app = typer.Typer()
    app.command()(prcli.tick)
    res = CliRunner().invoke(app, [])

    assert "pr-watch tick: failed:" in res.output
    assert res.exit_code == 1
    # A controlled failure exit, not the RuntimeError escaping the command:
    # reaching SystemExit proves the composed legs ran to the end.
    assert isinstance(res.exception, SystemExit), repr(res.exception)


def test_provider_supervisor_runs_before_github_leg_and_exception_is_nonfatal(
    monkeypatch
) -> None:
    import typer
    from types import SimpleNamespace
    from typer.testing import CliRunner

    from fno.agents import watchdog
    from fno.agents.watchdog import LEAVE, Row, Verdict
    from fno.pr_watch import cli as prcli
    from fno.pr_watch._dispatch import TickResult

    order = []
    settings = SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(
            enabled=True, watchdog="handoff", watchdog_mail_to="",
        ),
        pr_watch=SimpleNamespace(
            enabled=True, interval_seconds=600, tick_timeout_seconds=500,
            max_age_days=30, retries=3, graphql_min_remaining=0,
        ),
    )
    monkeypatch.setattr(prcli, "load_settings", lambda: settings)
    recovery_calls = []
    monkeypatch.setattr(
        "fno.recovery.run_recovery_sweep",
        lambda *_a, **kwargs: recovery_calls.append(kwargs) or 0,
    )
    monkeypatch.setattr("fno.agents.sweep.run_sweep", lambda **_k: ([], 0))
    verdict = Verdict("row-1", "worker", "working", LEAVE, "ok", "none")
    payload = {
        "generated_at": "x", "verdicts": [verdict._asdict()],
        "counts": {LEAVE: 1}, "warnings": [],
        "provider_outages": {
            "instrument": "measured", "breakers": [], "counts": {}, "refusals": [],
        },
    }
    monkeypatch.setattr(watchdog, "run_sweep", lambda **_k: (payload, [
        Row("row-1", "worker", "working", None, "/tmp")
    ]))
    monkeypatch.setattr(watchdog, "mail_gate", lambda *_a, **_k: (True, "", ""))
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *_a, **_k: None)
    monkeypatch.setattr(watchdog, "supervise_provider_handoffs", lambda *_a, **_k: (
        order.append("supervisor") or (_ for _ in ()).throw(RuntimeError("boom"))
    ))
    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick",
        lambda **_k: order.append("github") or TickResult(open_prs=0, acted=0),
    )
    monkeypatch.setattr(prcli, "_catchup_roots", lambda: [])

    app = typer.Typer()
    app.command()(prcli.tick)
    result = CliRunner().invoke(app, [])

    assert result.exit_code == 0, result.output
    assert order == ["supervisor", "github"]
    assert recovery_calls[0]["provider_failover"] is False
    assert order.count("github") == 1


def test_derived_deadline_stays_below_the_interval(monkeypatch):
    """The 60s floor must not push the derived deadline to or above
    StartInterval on small intervals: launchd does not run a StartInterval
    job concurrently, so an at-or-above deadline suppresses the successor."""
    from types import SimpleNamespace

    from fno.pr_watch.cli import _resolve_tick_deadline

    monkeypatch.delenv("FNO_PR_WATCH_TICK_TIMEOUT", raising=False)

    def cfg(interval):
        return SimpleNamespace(tick_timeout_seconds=None, interval_seconds=interval)

    assert _resolve_tick_deadline(cfg(600)) == 480
    assert _resolve_tick_deadline(cfg(60)) == 55
    assert _resolve_tick_deadline(cfg(30)) == 25
    # An explicit config value is clamped too: 3600 over a 600s interval
    # would suppress up to five successor ticks.
    explicit = SimpleNamespace(tick_timeout_seconds=3600, interval_seconds=600)
    assert _resolve_tick_deadline(explicit) == 595
