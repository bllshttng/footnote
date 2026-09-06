"""The notify_watch tick phase (x-87fb): spawn, parse, emit, degrade.

The arm's sampler logic is Rust and tested there; these tests pin the seam:
the receipt becomes one tick row, and every failure shape - binary absent,
non-zero exit, unparseable stdout - lands as ``notify_failed`` without
raising out of the tick.
"""
import stat

import pytest

from fno.pr_watch import cli as pr_watch_cli


@pytest.fixture()
def rows(monkeypatch):
    """Capture the tick rows and the phase's view of the world."""
    captured = []
    monkeypatch.setattr(pr_watch_cli, "_emit_tick_row", lambda *a, **kw: captured.append((a, kw)))
    monkeypatch.setattr(pr_watch_cli, "_catchup_roots", lambda: [])
    return captured


def _fake_binary(tmp_path, script):
    binary = tmp_path / "fake-fno-agents"
    binary.write_text(script)
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return binary


def test_receipt_becomes_one_tick_row(monkeypatch, tmp_path, rows):
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(
            tmp_path,
            "#!/bin/sh\necho '{\"acted\": 2, \"skip_reason\": null, \"detail\": \"operator_question:sent\"}'\n",
        ),
    )
    pr_watch_cli._run_notify_watch_phase()
    assert len(rows) == 1
    args, kwargs = rows[0]
    assert args == ("notify_watch",)
    assert kwargs["interval_s"] == 300
    assert kwargs["acted"] == 2
    assert kwargs["skip_reason"] is None
    assert kwargs["detail"] == "operator_question:sent"


def test_roots_ride_the_argv(monkeypatch, tmp_path, rows):
    recorded = tmp_path / "argv.txt"

    def fake_roots():
        # Production roots are is_dir()-filtered; mirror that.
        for name in ("repo-a", "repo-b"):
            (tmp_path / name).mkdir()
        return [tmp_path / "repo-a", tmp_path / "repo-b"]

    monkeypatch.setattr(pr_watch_cli, "_catchup_roots", fake_roots)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(
            tmp_path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGV_RECORD\"\necho '{\"acted\": 0, \"skip_reason\": \"notify_off\", \"detail\": \"x\"}'\n",
        ),
    )
    monkeypatch.setenv("ARGV_RECORD", str(recorded))
    pr_watch_cli._run_notify_watch_phase()
    argv = recorded.read_text().splitlines()
    assert argv[0] == "notify-watch"
    assert "--json" in argv
    assert argv[argv.index("--root") + 1] == str(tmp_path / "repo-a")
    assert argv.count("--root") == 2


def test_absent_binary_is_notify_failed(monkeypatch, tmp_path, rows):
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)
    pr_watch_cli._run_notify_watch_phase()
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "notify_failed"
    assert "absent" in kwargs["detail"]


def test_nonzero_and_unparseable_runs_are_notify_failed(monkeypatch, tmp_path, rows):
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(tmp_path, "#!/bin/sh\necho 'not json' >&2\necho ''\nexit 3\n"),
    )
    pr_watch_cli._run_notify_watch_phase()
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "notify_failed"

    rows.clear()
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(tmp_path, "#!/bin/sh\necho 'garbage'\n"),
    )
    pr_watch_cli._run_notify_watch_phase()
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "notify_failed"
