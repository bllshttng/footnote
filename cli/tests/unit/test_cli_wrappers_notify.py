"""Unit tests for `fno notify` - in-package OS notification helper (US2).

Formerly a wrapper that sourced scripts/lib/notify.sh; the dispatch is now
internalized in fno.notify._impl, so the verb runs from the installed package
with no repo-root path on disk (AC2-HP) and degrades loudly when no OS
notification tool is present (AC2-FR).
"""
from __future__ import annotations

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


def test_notify_impl_darwin_dispatches_osascript(monkeypatch):
    """Success-path parity: on macOS the helper invokes osascript and returns 0
    even if osascript itself fails (best-effort, matching the former bash)."""
    calls = {}

    def _stub_run(cmd, **kwargs):
        calls["cmd"] = list(cmd)

        class _R:
            returncode = 1  # tool failed; helper must still return 0

        return _R()

    monkeypatch.setattr(_impl.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(_impl.subprocess, "run", _stub_run)
    code, err = _impl.send_notification("T", "M")
    assert code == 0
    assert err == ""
    assert calls["cmd"][0] == "osascript"
