import json
import multiprocessing
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

from fno.wake.signal import WakeSignal, drop_signal, drain_signals


def _drop_one(args):
    repo_root, idx = args
    sig = WakeSignal(
        source="inbox-drain",
        kind="question",
        msg_id=f"msg-{idx:08x}",
        from_project="foo",
        summary=f"signal {idx}",
        ts=datetime.now(tz=timezone.utc),
    )
    drop_signal(repo_root, sig)


def test_drop_signal_writes_json(tmp_path):
    sig = WakeSignal(
        source="inbox-drain",
        kind="question",
        msg_id="msg-deadbeef",
        from_project="foo",
        summary="hi",
        ts=datetime(2026, 5, 5, 17, 14, tzinfo=timezone.utc),
    )
    drop_signal(tmp_path, sig)
    files = list((tmp_path / ".fno" / "wake-signals").glob("wake-*.json"))
    assert len(files) == 1
    payload = json.loads(files[0].read_text())
    assert payload["source"] == "inbox-drain"
    assert payload["kind"] == "question"
    assert payload["msg_id"] == "msg-deadbeef"


def test_drop_signal_creates_parent_dir(tmp_path):
    sig = WakeSignal(source="x", kind="question", msg_id="m", from_project="f",
                     summary="s", ts=datetime.now(tz=timezone.utc))
    drop_signal(tmp_path, sig)
    assert (tmp_path / ".fno" / "wake-signals").is_dir()


def test_concurrent_drops_no_collision(tmp_path):
    with multiprocessing.Pool(3) as pool:
        pool.map(_drop_one, [(tmp_path, i) for i in range(3)])
    files = list((tmp_path / ".fno" / "wake-signals").glob("wake-*.json"))
    assert len(files) == 3
    tmps = list((tmp_path / ".fno" / "wake-signals").glob(".tmp.*"))
    assert tmps == []


def test_drain_signals_logs_unlink_failure_to_stderr(tmp_path, capsys):
    """AC2-ERR: drain_signals emits a stderr line when unlink fails.

    The signal file is NOT returned in the drained list (since it wasn't deleted),
    but a warning must appear on stderr so operators can detect stuck signals.

    Caught by sigma-review HIGH (silent-failure-hunter).
    """
    sig = WakeSignal(
        source="inbox-drain",
        kind="question",
        msg_id="msg-stuck001",
        from_project="foo",
        summary="stuck signal",
        ts=datetime(2026, 5, 5, 17, 14, tzinfo=timezone.utc),
    )
    drop_signal(tmp_path, sig)

    # Verify the file exists before we inject the failure
    files = list((tmp_path / ".fno" / "wake-signals").glob("wake-*.json"))
    assert len(files) == 1

    # Inject an unlink failure
    original_unlink = Path.unlink

    def failing_unlink(self, missing_ok=False):
        raise OSError("Permission denied: simulated unlink failure")

    with patch.object(Path, "unlink", failing_unlink):
        drained = drain_signals(tmp_path)

    # The signal was not successfully deleted, so it should NOT appear in drained
    assert drained == [], f"Expected empty drained list on unlink failure, got: {drained}"

    # A warning must appear on stderr
    captured = capsys.readouterr()
    assert "unlink failed" in captured.err, (
        f"Expected 'unlink failed' in stderr, got: {captured.err!r}"
    )
    assert "msg-stuck001" in captured.err or "wake-" in captured.err, (
        f"Expected signal path or msg_id in stderr warning, got: {captured.err!r}"
    )
