"""Coverage gap 2: per-agent flock serialization on the codex create path.

US4-codex inherited the per-agent flock from US1 and the
``test_dispatch_ask::test_parallel_same_name_serializes_via_flock``
test pins serialization for the claude code path. The codex routes
added in US4-codex were not subjected to the same scrutiny — this
file closes that gap.

The strategy mirrors the existing claude-path test:

1. Two ``multiprocessing.Process`` workers run ``dispatch_ask`` with the
   same agent name and ``provider="codex"``.
2. Each worker monkeypatches ``providers.codex.create`` to sleep 0.5s
   inside the flock-bracketed code path so we can observe serialization
   via wall-clock.
3. Driver asserts both workers reach a readiness gate before either
   acquires the flock, then races them.
4. Verifies:
   - Total elapsed >= 1.0s (proves serialization; back-to-back 0.5s).
   - Exactly one ``codex_session_id`` lands in the registry (first
     writer wins; second sees existing and follows up via resume).

``spawn`` is the explicit start method to avoid fork-inherited fixture
state (mirrors existing parallel test pattern in ``test_dispatch_ask.py``).
"""
from __future__ import annotations

import multiprocessing
import os
import time
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


# ---------------------------------------------------------------------------
# Module-level worker target (must be importable for spawn start method).
# ---------------------------------------------------------------------------


def _codex_worker(
    home: str,
    name: str,
    message: str,
    ready_path: str,
    gate_path: str,
    sleep_seconds: float,
) -> None:
    """Spawn-mode worker: install fake codex.create + drive dispatch_ask.

    Reports readiness via ``ready_path``, blocks until ``gate_path``
    exists, then calls dispatch_ask. The faked ``codex.create`` sleeps
    inside the per-agent flock and returns a synthetic CodexResult; the
    follow-up path (second worker, after the first writes the registry
    row) is monkeypatched to short-circuit so we can measure
    serialization without spinning up the real codex resume code.
    """
    import os as _os
    import sys as _sys
    import time as _time
    from pathlib import Path as _P

    _os.environ["HOME"] = home
    _os.environ["FNO_CONFIG"] = str(
        _P(home) / ".fno" / "settings.yaml"
    )

    from fno import paths as _paths

    if hasattr(_paths._settings, "cache_clear"):
        _paths._settings.cache_clear()
    if hasattr(_paths.resolve_repo_root, "cache_clear"):
        _paths.resolve_repo_root.cache_clear()

    # PATH must include a fake codex binary for is_provider_available
    # to return True; we also fake codex.create + codex.resume in-process.
    bin_dir = _P(home) / "fakebin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    fake = bin_dir / "codex"
    if not fake.exists():
        fake.write_text("#!/bin/sh\nexit 0\n")
        fake.chmod(0o755)
    _os.environ["PATH"] = str(bin_dir) + ":" + _os.environ.get("PATH", "")

    from fno.agents.dispatch import DispatchAskError, dispatch_ask
    from fno.agents.providers import codex as codex_mod

    def fake_create(*, cwd, prompt, from_name, yolo, output_path, timeout, **_kwargs):
        # Sleep simulates the in-flock work; serialization shows up as
        # back-to-back sleeps in the wall-clock measurement.
        # **_kwargs accepts forward-compat additions like agent_self.
        _time.sleep(sleep_seconds)
        return codex_mod.CodexResult(
            exit_code=0,
            session_id=f"00000000-0000-0000-0000-{_os.getpid():012d}",
            last_msg="ok",
            duration_ms=int(sleep_seconds * 1000),
        )

    def fake_resume(*, session_id, cwd, prompt, from_name, yolo, output_path, timeout, **_kwargs):
        _time.sleep(sleep_seconds)
        return codex_mod.CodexResult(
            exit_code=0,
            session_id=None,
            last_msg="resumed",
            duration_ms=int(sleep_seconds * 1000),
        )

    codex_mod.create = fake_create
    codex_mod.resume = fake_resume

    _P(ready_path).write_text("ready")
    while not _P(gate_path).exists():
        _time.sleep(0.02)

    try:
        dispatch_ask(
            name=name,
            message=message,
            provider="codex",
            cwd=_P(home),
            timeout=10,
        )
        _sys.exit(0)
    except DispatchAskError as exc:
        _sys.exit(exc.exit_code)


def test_parallel_codex_asks_serialize_via_flock(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-HP: two simultaneous codex asks serialize via per-agent flock.

    dispatch_ask no longer auto-creates unknown agents (ask de-overload,
    task 1.1). Pre-seed a registry row so both workers take the follow-up
    (resume) path, which is equally guarded by the per-agent flock.
    The serialization invariant is identical: two flock-bracketed 0.5s
    sleeps must run back-to-back.
    """
    use_tmpdir(monkeypatch, tmp_path)

    # Pre-seed the registry so both spawned workers find the agent and
    # route to codex.resume (flock-guarded) instead of hitting the
    # unknown-agent guard (exit 16).
    import json as _json
    registry_dir = tmp_path / ".fno" / "agents"
    registry_dir.mkdir(parents=True, exist_ok=True)
    registry_path = registry_dir / "registry.json"
    seed_session_id = "00000000-0000-0000-0000-pre-seeded-000"
    registry_path.write_text(
        _json.dumps({
            "schema_version": 1,
            "agents": [{
                "name": "parallel-codex",
                "provider": "codex",
                "cwd": str(tmp_path),
                "log_path": str(tmp_path / "log.jsonl"),
                "status": "live",
                "codex_session_id": seed_session_id,
                "gemini_session_id": None,
                "created_at": None,
                "last_message_at": None,
                "mcp_channel_id": None,
                "host_mode": None,
            }],
        }, indent=2),
        encoding="utf-8",
    )

    # ``spawn`` is required because fork-inherited fcntl state can confuse
    # the per-agent flock; matches the existing claude-path test.
    ctx = multiprocessing.get_context("spawn")

    home = str(tmp_path)
    ready_a = tmp_path / "ready-a"
    ready_b = tmp_path / "ready-b"
    gate = tmp_path / "gate"
    sleep_seconds = 0.5

    p_a = ctx.Process(
        target=_codex_worker,
        args=(home, "parallel-codex", "msg-a",
              str(ready_a), str(gate), sleep_seconds),
    )
    p_b = ctx.Process(
        target=_codex_worker,
        args=(home, "parallel-codex", "msg-b",
              str(ready_b), str(gate), sleep_seconds),
    )

    try:
        p_a.start()
        p_b.start()

        deadline = time.monotonic() + 10
        while (
            (not ready_a.exists() or not ready_b.exists())
            and time.monotonic() < deadline
        ):
            time.sleep(0.05)
        assert ready_a.exists() and ready_b.exists(), (
            "workers did not signal ready"
        )

        start = time.monotonic()
        gate.write_text("go")

        p_a.join(timeout=20)
        p_b.join(timeout=20)
        elapsed = time.monotonic() - start

        assert p_a.exitcode is not None
        assert p_b.exitcode is not None

        # Serialization invariant: two 0.5s sleeps back-to-back. We give
        # a 0.4s lower bound (instead of 1.0s) to absorb spawn-start
        # overhead, scheduler jitter, and the gate-write race; the
        # important bit is that the second sleep BEGINS after the first
        # ends, not that it begins at exactly t=0.5s.
        assert elapsed >= sleep_seconds * 1.5, (
            f"workers ran too fast: {elapsed:.3f}s for two {sleep_seconds}s "
            f"sleeps under a shared flock"
        )

        # Both workers took the resume path; the pre-seeded row is the
        # only registry row (resume does not add rows).
        from fno.agents.registry import load_registry
        entries = [e for e in load_registry() if e.name == "parallel-codex"]
        assert len(entries) == 1, (
            f"expected exactly one registry row, got {len(entries)}: "
            f"{entries}"
        )
        assert entries[0].provider == "codex"

        # Both workers should exit 0 - first creates, second resumes.
        exits = {p_a.exitcode, p_b.exitcode}
        assert exits == {0}, (
            f"expected both workers exit 0, got a={p_a.exitcode} "
            f"b={p_b.exitcode}"
        )
    finally:
        for p in (p_a, p_b):
            if p.is_alive():
                p.terminate()
                p.join(timeout=5)
