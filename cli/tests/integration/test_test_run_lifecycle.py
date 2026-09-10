"""`run_suite_bounded`'s two real paths: routed to the native `fno-agents
test-run` owner, and the degraded Python fallback when that binary is
absent. Real short-lived processes throughout (never a fake `Popen`) - the
native-path test is the same x-b275 repro as the Rust lifecycle test, proven
again through the actual Python entry point every `fno doctor test` call
goes through.

Binary resolution mirrors `test_claims_cross_impl.py`: `$FNO_AGENTS_BIN`,
else the repo debug build. Without one, the native-path test SKIPS (the
fallback-path test needs no binary and always runs).
"""
from __future__ import annotations

import os
import time
from pathlib import Path

import pytest

from fno import test_runner


def _find_repo_root(start: Path) -> Path | None:
    for parent in [start, *start.parents]:
        if (parent / "crates" / "fno-agents").is_dir():
            return parent
    return None


def _rust_bin() -> Path | None:
    env = os.environ.get("FNO_AGENTS_BIN", "")
    if env:
        p = Path(env)
        return p if p.exists() else None
    root = _find_repo_root(Path(__file__).resolve().parent)
    if root is None:
        return None
    for profile in ("debug", "release"):
        p = root / "crates" / "fno-agents" / "target" / profile / "fno-agents"
        if p.exists():
            return p
    return None


RUST_BIN = _rust_bin()


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


@pytest.mark.skipif(RUST_BIN is None, reason="fno-agents binary not built (cargo build -p fno-agents)")
def test_native_path_reaps_a_backgrounded_group_mate(tmp_path, monkeypatch):
    monkeypatch.setattr(test_runner, "_native_owner_binary", lambda: str(RUST_BIN))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    pid_file = tmp_path / "leftover.pid"
    cmd = ["/bin/sh", "-c", f"sleep 20 & echo $! > {pid_file}; exit 0"]

    rc = test_runner.run_suite_bounded(cmd, dict(os.environ), timeout=30)

    assert rc == 0, "the leader's own clean exit must still read as success"
    leftover_pid = int(pid_file.read_text().strip())
    assert not _pid_alive(leftover_pid), (
        f"backgrounded sleep {leftover_pid} must be dead: the native owner must reap every "
        "group-mate a normal leader exit leaves running"
    )


@pytest.mark.skipif(RUST_BIN is None, reason="fno-agents binary not built (cargo build -p fno-agents)")
def test_native_path_timeout_kills_a_hung_leader(tmp_path, monkeypatch):
    monkeypatch.setattr(test_runner, "_native_owner_binary", lambda: str(RUST_BIN))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))

    start = time.monotonic()
    rc = test_runner.run_suite_bounded(["sleep", "30"], dict(os.environ), timeout=1)
    elapsed = time.monotonic() - start

    assert rc == 124
    assert elapsed < 10, f"cleanup after a 1s timeout must not need the leader's 30s sleep (took {elapsed}s)"


def test_fallback_path_still_kills_group_on_timeout_when_native_absent(monkeypatch):
    """No native binary on PATH: the pre-native Python group-kill still fires
    - degraded, never silently unbounded."""
    monkeypatch.setattr(test_runner, "_native_owner_binary", lambda: None)

    start = time.monotonic()
    rc = test_runner.run_suite_bounded(["sleep", "30"], dict(os.environ), timeout=1)
    elapsed = time.monotonic() - start

    assert rc == 124
    assert elapsed < 10, f"the fallback's own timeout+kill must not need the leader's 30s sleep (took {elapsed}s)"
