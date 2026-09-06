"""Unit tests for `fno notify` - in-package OS notification helper (US2).

Formerly a wrapper that sourced scripts/lib/notify.sh; the dispatch is now
internalized in fno.notify._impl, so the verb runs from the installed package
with no repo-root path on disk (AC2-HP) and degrades loudly when no OS
notification tool is present (AC2-FR).
"""
from __future__ import annotations

import json
from types import SimpleNamespace

from typer.testing import CliRunner

from fno.cli import app
from fno.notify import _impl

runner = CliRunner()


def test_notify_help_renders():
    """AC2-UI: fno notify --help documents both positional args."""
    result = runner.invoke(app, ["notify", "--help"])
    assert result.exit_code == 0
    assert "TITLE" in result.stdout
    assert "MESSAGE" in result.stdout


def test_notify_runs_in_package_no_repo_path(monkeypatch):
    """AC2-HP: the verb dispatches via in-package Python (no scripts/ path).

    Stub the in-package dispatch so the test does not actually fire a desktop
    notification; assert the verb reached it with both args and exited 0.
    """
    captured = {}

    def _stub(title, message):
        captured["title"] = title
        captured["message"] = message
        return 0, ""

    monkeypatch.setattr("fno.notify.cli.send_notification", _stub)
    result = runner.invoke(app, ["notify", "Test Title", "Test message body"])
    assert result.exit_code == 0, result.output
    assert captured["title"] == "Test Title"
    assert captured["message"] == "Test message body"


def test_notify_degrades_loudly_when_no_tool(monkeypatch):
    """AC2-FR: with neither osascript nor notify-send, exit non-zero with a
    one-line message - never a silent no-op, never a traceback."""
    # Force the no-tool path: non-Darwin platform and notify-send absent.
    monkeypatch.setattr(_impl.platform, "system", lambda: "Linux")
    monkeypatch.setattr(_impl.shutil, "which", lambda _name: None)

    result = runner.invoke(app, ["notify", "title", "message"])
    assert result.exit_code != 0
    assert "no OS notification tool available" in result.output


def test_notify_impl_darwin_dispatches_osascript(monkeypatch, tmp_path):
    """Success-path parity: on macOS the helper invokes osascript and returns 0
    even if osascript itself fails (best-effort, matching the former bash)."""
    calls: list = []

    def _stub_run(cmd, **kwargs):
        calls.append(list(cmd))

        class _R:
            returncode = 1  # tool failed; helper must still return 0

        return _R()

    monkeypatch.setattr(_impl.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(_impl.subprocess, "run", _stub_run)
    # This test asserts that a dispatch HAPPENS, so it opts out of the
    # hermetic suppression the whole suite runs under - `fno.hermetic` stamps
    # FNO_TEST_HERMETIC=1 to keep every other test off the operator's screen.
    # The unsuppressed run also emits the operator_notice journal row (x-5f06),
    # so the journal must be pinned inside the sandbox.
    monkeypatch.delenv("FNO_TEST_HERMETIC", raising=False)
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / "events.jsonl"))
    code, err = _impl.send_notification("T", "M")
    assert code == 0
    assert err == ""
    assert calls[0][0] == "osascript"


def _headless(monkeypatch):
    """A host with neither notifier: Linux, no notify-send, dispatch suppressed."""
    monkeypatch.setattr(_impl.platform, "system", lambda: "Linux")
    monkeypatch.setattr(_impl.shutil, "which", lambda _name: None)
    monkeypatch.delenv("FNO_TEST_HERMETIC", raising=False)


def _sink_settings(monkeypatch, events):
    """load_settings returning one enabled sink routing exactly `events`."""

    class _S:
        status_sinks = [
            SimpleNamespace(name="s", enabled=True, events=list(events)),
        ]

    monkeypatch.setattr("fno.config.load_settings", lambda: _S())


def test_headless_with_sink_routes_operator_notice_delivers(monkeypatch, tmp_path):
    """The no-local-notifier host delivers through the sink lane (x-5f06).

    The journal row lands carrying the pointer, and the return reads 0: the
    channel count is honest in the direction the old contract got wrong.
    """
    _headless(monkeypatch)
    _sink_settings(monkeypatch, ["operator_notice"])
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / "events.jsonl"))

    code, err = _impl.send_notification("T", "3 open questions.", "fno inbox outstanding")

    assert (code, err) == (0, "")
    rows = [json.loads(line) for line in (tmp_path / "events.jsonl").read_text().splitlines()]
    notices = [r for r in rows if r["type"] == "operator_notice"]
    assert len(notices) == 1
    assert notices[0]["data"] == {
        "title": "T",
        "body": "3 open questions.",
        "pointer": "fno inbox outstanding",
    }


def test_headless_without_any_sink_still_degrades_loudly(monkeypatch, tmp_path):
    """No local tool AND no sink routing the type: non-zero, the AC2-FR shape."""
    _headless(monkeypatch)
    _sink_settings(monkeypatch, ["run_summary"])
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / "events.jsonl"))

    code, err = _impl.send_notification("T", "M")
    assert code == 1 and "no OS notification tool available" in err
