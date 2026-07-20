"""Tests for fno.agents.registry - TDD Red phase.

Covers the five acceptance criteria for Task 1.1:
  AC1-HP: round-trip a single agent entry with all required fields
  AC2-ERR: atomic write - kill-9 simulation leaves prior file intact
  AC3-HP: per-agent flock serializes concurrent writes for the same agent
  AC4-ERR: schema_version mismatch raises RegistryVersionError with clear message
  AC5-HP: registry path resolved via fno.paths
"""
from __future__ import annotations

import fcntl
import json
import multiprocessing as mp
import os
import time
from datetime import datetime, timezone
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _minimal_entry(name: str = "test-agent", **overrides) -> dict:
    base = {
        "name": name,
        "provider": "claude",
        "cwd": "/tmp",
        "log_path": "/tmp/test-agent.log",
    }
    base.update(overrides)
    return base


# ---------------------------------------------------------------------------
# AC1-HP: round-trip a single agent entry
# ---------------------------------------------------------------------------


def test_ac1_hp_round_trip_entry(tmp_path: Path, monkeypatch) -> None:
    """AC1-HP: write + read back a single agent entry preserving all fields."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="my-agent",
        harness="claude",
        cwd="/home/user/project",
        short_id="abc123",
        harness_session_id=None,
        log_path="/tmp/my-agent.log",
    )

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert len(loaded) == 1
    e = loaded[0]
    assert e.name == "my-agent"
    assert e.harness == "claude"
    assert e.cwd == "/home/user/project"
    assert e.short_id == "abc123"
    assert e.harness_session_id is None
    assert e.log_path == "/tmp/my-agent.log"
    # AC1-HP: the removed identity keys never round-trip to disk.
    raw_row = json.loads(registry_path.read_text())["agents"][0]
    for dead in ("provider", "codex_session_id", "gemini_session_id", "claude_session_uuid"):
        assert dead not in raw_row
    # created_at must be ISO8601 UTC
    assert e.created_at.endswith("Z") or "+" in e.created_at


def test_ac1_hp_optional_session_ids(tmp_path: Path, monkeypatch) -> None:
    """AC1-HP: codex_session_id and gemini_session_id are optional."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="codex-agent",
        harness="codex",
        cwd="/tmp",
        harness_session_id="sess-xyz",
        log_path="/tmp/codex-agent.log",
    )

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)
    loaded = load_registry(path=registry_path)

    assert loaded[0].harness_session_id == "sess-xyz"
    assert loaded[0].short_id == ""
    assert loaded[0].session_id == "sess-xyz"


def test_ac1_hp_schema_version_in_file(tmp_path: Path, monkeypatch) -> None:
    """AC1-HP: on-disk format includes the current schema_version."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import SCHEMA_VERSION, AgentEntry, write_registry

    entry = AgentEntry(
        name="v-agent",
        harness="gemini",
        cwd="/tmp",
        log_path="/tmp/v-agent.log",
    )

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    raw = json.loads(registry_path.read_text())
    assert raw.get("schema_version") == SCHEMA_VERSION


# ---------------------------------------------------------------------------
# AC2-ERR: atomic write - exception mid-write leaves prior file intact
# ---------------------------------------------------------------------------


def test_ac2_err_atomic_write_on_exception(tmp_path: Path, monkeypatch) -> None:
    """AC2-ERR: exception mid-write leaves prior file intact (no corruption)."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    # Write an initial valid registry
    initial_entry = AgentEntry(
        name="safe-agent",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/safe.log",
    )
    write_registry([initial_entry], path=registry_path)
    original_content = registry_path.read_text()

    # Now simulate a write that raises mid-way by patching json.dumps
    import fno.agents.registry as reg_module

    original_dumps = json.dumps

    def _exploding_dumps(*args, **kwargs):
        raise RuntimeError("simulated kill -9 mid-write")

    monkeypatch.setattr(reg_module, "_json_dumps", _exploding_dumps)

    new_entry = AgentEntry(
        name="corrupt-agent",
        harness="codex",
        cwd="/tmp",
        log_path="/tmp/corrupt.log",
    )
    with pytest.raises(RuntimeError, match="simulated kill -9"):
        write_registry([new_entry], path=registry_path)

    # Original file must be intact
    assert registry_path.read_text() == original_content
    loaded = load_registry(path=registry_path)
    assert loaded[0].name == "safe-agent"


# ---------------------------------------------------------------------------
# AC3-HP: per-agent flock serializes concurrent writes
# ---------------------------------------------------------------------------


def _write_agent_with_held_lock(
    registry_path_str: str,
    agent_name: str,
    result_queue: "mp.Queue[str]",
    hold_seconds: float,
) -> None:
    """Child-process: hold the per-agent flock for hold_seconds, then write."""
    from pathlib import Path as P
    from fno.agents.registry import AgentEntry, write_registry, _agent_lock_path

    registry_path = P(registry_path_str)
    lock_path = _agent_lock_path(agent_name, registry_path)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    with open(lock_path, "w") as lf:
        fcntl.flock(lf, fcntl.LOCK_EX)
        result_queue.put("locked")
        time.sleep(hold_seconds)
        entry = AgentEntry(
            name=agent_name,
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/holder.log",
        )
        write_registry([entry], path=registry_path)
        result_queue.put("done")
        fcntl.flock(lf, fcntl.LOCK_UN)


def test_ac3_hp_flock_blocks_concurrent_write(tmp_path: Path, monkeypatch) -> None:
    """AC3-HP: concurrent writes to the same agent name are serialized by flock."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, write_registry, _agent_lock_path

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    agent_name = "race-agent"
    result_q: mp.Queue = mp.Queue()

    # Spawn a child that holds the flock for 1 second
    child = mp.Process(
        target=_write_agent_with_held_lock,
        args=(str(registry_path), agent_name, result_q, 1.0),
    )
    child.start()

    # Wait until child confirms it holds the lock
    msg = result_q.get(timeout=5)
    assert msg == "locked"

    # Now try to acquire in the foreground with a LOCK_NB attempt - must fail
    lock_path = _agent_lock_path(agent_name, registry_path)
    lock_path.parent.mkdir(parents=True, exist_ok=True)

    with open(lock_path, "w") as lf:
        try:
            fcntl.flock(lf, fcntl.LOCK_EX | fcntl.LOCK_NB)
            locked = True
            fcntl.flock(lf, fcntl.LOCK_UN)
        except BlockingIOError:
            locked = False

    assert not locked, "Expected flock to block while child holds the lock"

    child.join(timeout=5)
    assert child.exitcode == 0


# ---------------------------------------------------------------------------
# AC4-ERR: schema_version mismatch raises RegistryVersionError
# ---------------------------------------------------------------------------


def test_ac4_err_future_schema_version_raises(tmp_path: Path, monkeypatch) -> None:
    """AC4-ERR: loading a file with a future schema_version raises RegistryVersionError.

    SCHEMA_VERSION is now 10 (provider removal); v11 is the future-drift case.
    """
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": 11, "agents": []}), encoding="utf-8"
    )

    with pytest.raises(RegistryVersionError) as exc_info:
        load_registry(path=registry_path)

    msg = str(exc_info.value)
    assert "11" in msg  # read version present
    assert "10" in msg  # expected version present


def test_ac4_err_version_error_message_names_versions(tmp_path: Path, monkeypatch) -> None:
    """AC4-ERR: error message explicitly names both the read and expected schema_version."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": 99, "agents": []}), encoding="utf-8"
    )

    with pytest.raises(RegistryVersionError, match=r"99") as exc_info:
        load_registry(path=registry_path)

    from fno.agents.registry import SCHEMA_VERSION

    assert f"schema_version={SCHEMA_VERSION}" in str(exc_info.value)


def test_x8dfc_unknown_provider_loads_undispatchable(tmp_path: Path, monkeypatch) -> None:
    """x-8dfc: a provider outside the dispatch roster no longer bricks the read.

    Pre-x-8dfc a typo'd provider raised RegistryVersionError, bricking the
    whole shared read. Now identity is a shape check: the row loads as an
    undispatchable identity row (mail-routable), and capability is refused
    later at the spawn/ask seam, not at load.
    """
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.dispatch import _check_known_provider
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "agents": [
                    {
                        "name": "rogue",
                        "provider": "calude",  # typo — masquerades as claude
                        "cwd": "/tmp",
                        "log_path": "/tmp/r.log",
                        "claude_short_id": None,
                        "codex_session_id": None,
                        "gemini_session_id": None,
                        "created_at": "2026-05-19T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    entries = load_registry(path=registry_path)
    assert len(entries) == 1
    assert entries[0].harness == "calude"
    # ...but it is NOT dispatchable: the spawn/ask seam still refuses it.
    with pytest.raises(ValueError, match="calude"):
        _check_known_provider("calude")


def test_load_registry_tolerates_agy_provider(tmp_path: Path, monkeypatch) -> None:
    """A provider='agy' row loads without raising.

    Rust writes agy rows; before the READABLE_PROVIDERS split, load_registry
    rc=12'd on the first agy row, bricking every Python consumer (spawn
    collision check, mail send, discuss dispatch).
    """
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "agents": [
                    {
                        "name": "relay-agy-live",
                        "provider": "agy",
                        "cwd": "/tmp",
                        "log_path": "/tmp/agy.log",
                        "created_at": "2026-06-30T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    entries = load_registry(path=registry_path)
    assert len(entries) == 1
    assert entries[0].harness == "agy"


def test_dispatch_still_refuses_agy_provider() -> None:
    """agy is read-tolerant but NOT Python-dispatchable (no adapter).

    The dispatch vocabulary (KNOWN_PROVIDERS) must stay narrower than the
    read vocabulary (READABLE_PROVIDERS), so a Python spawn/ask of agy is
    rejected early rather than crashing late with a missing adapter.
    """
    from fno.agents.dispatch import _check_known_provider
    from fno.agents.providers import KNOWN_PROVIDERS, READABLE_PROVIDERS

    assert "agy" in READABLE_PROVIDERS
    assert "agy" not in KNOWN_PROVIDERS
    with pytest.raises(ValueError, match="agy"):
        _check_known_provider("agy")


def test_ac4_err_malformed_row_shape_rejected(tmp_path: Path, monkeypatch) -> None:
    """A row with unknown fields (future-schema drift) is rejected loudly, not silently."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "agents": [
                    {
                        "name": "ahead-of-time",
                        "provider": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/x.log",
                        # Field that doesn't exist on AgentEntry yet —
                        # mimics a future fno adding metadata without
                        # bumping schema_version.
                        "supervisor_pgid": 1234,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    with pytest.raises(RegistryVersionError, match="malformed shape"):
        load_registry(path=registry_path)


def test_load_registry_rejects_invalid_json(tmp_path: Path, monkeypatch) -> None:
    """Invalid JSON surfaces as RegistryVersionError, not raw JSONDecodeError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text("not even {valid", encoding="utf-8")

    with pytest.raises(RegistryVersionError, match="malformed JSON"):
        load_registry(path=registry_path)


def test_load_registry_rejects_non_dict_top_level(tmp_path: Path, monkeypatch) -> None:
    """A JSON array at the top level is rejected via RegistryVersionError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text("[]", encoding="utf-8")

    with pytest.raises(RegistryVersionError, match="not a JSON object"):
        load_registry(path=registry_path)


def test_load_registry_rejects_non_list_agents_field(tmp_path: Path, monkeypatch) -> None:
    """agents must be a list — string or object is RegistryVersionError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": 1, "agents": "oops"}), encoding="utf-8"
    )

    with pytest.raises(RegistryVersionError, match="'agents' field is not a list"):
        load_registry(path=registry_path)


def test_load_registry_rejects_non_dict_row(tmp_path: Path, monkeypatch) -> None:
    """A non-dict element inside agents (e.g. string, null) is RegistryVersionError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": 1, "agents": ["oops", None]}),
        encoding="utf-8",
    )

    with pytest.raises(RegistryVersionError, match="row 0 is not a JSON object"):
        load_registry(path=registry_path)


def _concurrent_update_worker(worker_id: int, registry_path_str: str) -> None:
    """Module-scope worker for the concurrent-update test.

    Lives at module scope so multiprocessing can pickle it under both
    fork (Linux default ≤3.13) and spawn (macOS / Linux 3.14+) start
    methods. A test-local closure cannot be pickled under spawn.
    """
    from pathlib import Path as P

    from fno.agents.registry import AgentEntry, update_registry

    def add_entry(entries):
        entries.append(
            AgentEntry(
                name=f"worker-{worker_id}",
                harness="claude",
                cwd="/tmp",
                log_path=f"/tmp/w{worker_id}.log",
            )
        )
        return entries

    update_registry(add_entry, path=P(registry_path_str))


def test_update_registry_serializes_different_name_writes(
    tmp_path: Path, monkeypatch
) -> None:
    """Two concurrent update_registry calls for DIFFERENT agents don't lose updates.

    Codex review on PR #288 (P1): without a registry-wide lock, two ask
    calls for different names can both ``load_registry`` -> mutate ->
    ``write_registry`` and the loser's update is silently dropped. This
    test spawns N parallel workers; if the global lock holds, all N
    entries survive in the final registry.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"

    n_workers = 5
    procs = [
        mp.Process(
            target=_concurrent_update_worker,
            args=(i, str(registry_path)),
        )
        for i in range(n_workers)
    ]
    for p in procs:
        p.start()
    for p in procs:
        p.join(timeout=15)
        assert p.exitcode == 0, f"worker exited {p.exitcode}"

    loaded = load_registry(path=registry_path)
    names = {e.name for e in loaded}
    assert names == {f"worker-{i}" for i in range(n_workers)}, (
        f"expected {n_workers} distinct worker entries; got {names}"
    )


def test_write_registry_cleans_orphan_tmp_on_failure(tmp_path: Path, monkeypatch) -> None:
    """An ``OSError`` during the temp-write/rename window does not leave a stray .tmp."""
    use_tmpdir(monkeypatch, tmp_path)

    import fno.agents.registry as reg_module
    from fno.agents.registry import AgentEntry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    original_replace = os.replace

    def _explode_replace(*args, **kwargs):
        raise OSError("simulated disk full during rename")

    monkeypatch.setattr(reg_module.os, "replace", _explode_replace)

    entry = AgentEntry(name="t", harness="claude", cwd="/tmp", log_path="/tmp/t.log")
    with pytest.raises(OSError, match="simulated disk full"):
        write_registry([entry], path=registry_path)

    # The .tmp must be cleaned up so future writes don't accumulate orphans.
    tmp_sibling = registry_path.with_suffix(registry_path.suffix + ".tmp")
    assert not tmp_sibling.exists()


# ---------------------------------------------------------------------------
# AC5-HP: registry path resolved via fno.paths
# ---------------------------------------------------------------------------


def test_ac5_hp_default_path_under_state_dir(tmp_path: Path, monkeypatch) -> None:
    """AC5-HP: agents_registry_path() returns a path under state_dir by default."""
    use_tmpdir(monkeypatch, tmp_path)

    import fno.paths as paths

    reg_path = paths.agents_registry_path()
    state = paths.state_dir()

    assert str(reg_path).startswith(str(state)), (
        f"registry path {reg_path} should be under state_dir {state}"
    )
    assert reg_path.name == "registry.json"


def test_ac5_hp_write_registry_uses_paths_default(tmp_path: Path, monkeypatch) -> None:
    """AC5-HP: write_registry with no path argument writes to paths.agents_registry_path()."""
    use_tmpdir(monkeypatch, tmp_path)

    import fno.paths as paths
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="default-path-agent",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/default.log",
    )
    write_registry([entry])

    expected = paths.agents_registry_path()
    assert expected.exists(), f"Expected registry at {expected}"

    loaded = load_registry()
    assert loaded[0].name == "default-path-agent"


# ---------------------------------------------------------------------------
# US2 Task 2.1: schema_version 2 — status + last_message_at + v1->v2 synthesis
# ---------------------------------------------------------------------------


def test_us2_schema_version_is_three() -> None:
    """The on-disk schema version is 7 after the screen-manifest bump.

    (Test name retained for greppability of the original US2 commit;
    the value tracks the latest bump.)
    """
    from fno.agents.registry import SCHEMA_VERSION

    assert SCHEMA_VERSION == 10


def test_us2_agent_entry_has_status_and_last_message_at() -> None:
    """AgentEntry gains status (default "live") and last_message_at (default None)."""
    from fno.agents.registry import AgentEntry

    entry = AgentEntry(
        name="x",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/x.log",
    )
    assert entry.status == "live"
    assert entry.last_message_at is None


def test_us2_v2_round_trip_preserves_new_fields(tmp_path: Path, monkeypatch) -> None:
    """v2 write -> read round-trip preserves status and last_message_at."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="busy",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/busy.log",
        status="orphaned",
        last_message_at="2026-05-20T22:00:00Z",
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert len(loaded) == 1
    assert loaded[0].status == "orphaned"
    assert loaded[0].last_message_at == "2026-05-20T22:00:00Z"


def test_load_registry_accepts_all_projected_statuses(tmp_path: Path, monkeypatch) -> None:
    """registry.status is a projection of state.status, so every AgentStatus
    value the daemon can write must read back cleanly. In particular `exited`:
    the daemon writes it on child exit and retains the row until rm. The old
    {live, orphaned} guard hard-errored on `exited`, bricking every Python
    `fno agents` command until the row was rm'd (ab-3c063856 grid testing)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    for status in (
        "spawning",
        "ready",
        "idle",
        "busy",
        "live",
        "restarting",
        "orphaned",
        "failed",
        "exited",
        "permanent_dead",
    ):
        registry_path.write_text(
            json.dumps(
                {
                    "schema_version": 3,
                    "agents": [
                        {
                            "name": "a",
                            "provider": "codex",
                            "cwd": "/tmp",
                            "log_path": "/tmp/a.log",
                            "status": status,
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        loaded = load_registry(path=registry_path)
        assert len(loaded) == 1, f"status {status!r} should load"
        assert loaded[0].status == status


def test_load_registry_still_rejects_garbage_status(tmp_path: Path, monkeypatch) -> None:
    """Widening KNOWN_STATUSES to the full AgentStatus set must not weaken the
    guard against genuinely-invalid values."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 3,
                "agents": [
                    {
                        "name": "a",
                        "provider": "codex",
                        "cwd": "/tmp",
                        "log_path": "/tmp/a.log",
                        "status": "zombie",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    with pytest.raises(RegistryVersionError, match="zombie"):
        load_registry(path=registry_path)


def test_ab_a171ceb2_v4_reads_host_mode_and_keeps_back_compat(
    tmp_path: Path, monkeypatch
) -> None:
    """The v4 host_mode forward-compat bump reads cleanly with host_mode
    preserved, and the widened accepted range still reads v1..=v4 (the bump
    must not drop back-compat reads; ab-a171ceb2). The current SCHEMA_VERSION
    is 7 after the screen-manifest bump."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import SCHEMA_VERSION, load_registry

    assert SCHEMA_VERSION == 10
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    def row(host_mode=None):
        r = {
            "name": "a",
            "provider": "codex",
            "cwd": "/tmp",
            "log_path": "/tmp/a.log",
            "status": "live",
        }
        if host_mode is not None:
            r["host_mode"] = host_mode
        return r

    # v4 round-trips an explicit interactive host_mode.
    registry_path.write_text(
        json.dumps({"schema_version": 4, "agents": [row("interactive")]}),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert len(loaded) == 1
    assert loaded[0].host_mode == "interactive"

    # Every version in the widened accepted range still loads (no v1 drop),
    # and an absent host_mode coerces to exec regardless of version.
    for v in (1, 2, 3, 4):
        registry_path.write_text(
            json.dumps({"schema_version": v, "agents": [row()]}), encoding="utf-8"
        )
        loaded = load_registry(path=registry_path)
        assert len(loaded) == 1, f"v{v} must still read after the bump"
        assert loaded[0].host_mode == "exec", f"v{v} absent host_mode => exec"


def test_inside_leg_round_trips_across_registry_boundary(
    tmp_path: Path, monkeypatch
) -> None:
    """inside-out E3.1: the additive `inside_leg` field round-trips losslessly.

    Python is a pure passthrough custodian of the Rust-authored report blob, so a
    row carrying inside_leg must (a) read into AgentEntry without bricking the
    typed `AgentEntry(**row)` path, (b) survive a write/load cycle unchanged, and
    (c) default to None when absent.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        write_registry,
    )

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    report = {
        "state": "working",
        "seq": 7,
        "reason": "running tests",
        "received_at": "2026-06-27T00:00:00Z",
        "ttl_ms": 5000,
    }

    # (a) A Rust-written row carrying inside_leg loads as an opaque dict.
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 5,
                "agents": [
                    {
                        "name": "pane",
                        "provider": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/pane.log",
                        "created_at": "2026-06-27T00:00:00Z",
                        "status": "live",
                        "inside_leg": report,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert len(loaded) == 1
    assert loaded[0].inside_leg == report

    # (b) write -> load preserves the blob byte-for-value.
    write_registry(loaded, path=registry_path)
    reloaded = load_registry(path=registry_path)
    assert reloaded[0].inside_leg == report

    # (c) A row without inside_leg defaults to None, and a fresh AgentEntry too.
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 5,
                "agents": [
                    {
                        "name": "bare",
                        "provider": "codex",
                        "cwd": "/tmp",
                        "log_path": "/tmp/bare.log",
                        "created_at": "2026-06-27T00:00:00Z",
                        "status": "live",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    assert load_registry(path=registry_path)[0].inside_leg is None
    assert AgentEntry(name="x", harness="claude", cwd="/t", log_path="/t/x.log").inside_leg is None


def test_screen_state_round_trips_across_registry_boundary(
    tmp_path: Path, monkeypatch
) -> None:
    """v7: the additive `screen_state` verdict round-trips losslessly.

    Same X3 passthrough contract as inside_leg: the Rust daemon's scrape sweep
    is the sole writer; Python custodies the opaque blob so a mixed-language
    registry never drops it.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        write_registry,
    )

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    verdict = {
        "state": "idle",
        "rule": "idle_prompt",
        "seq": 3,
        "at": "2026-07-02T00:00:00Z",
        "ttl_ms": 30000,
    }

    # (a) A Rust-written row carrying screen_state loads as an opaque dict.
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 7,
                "agents": [
                    {
                        "name": "pane",
                        "provider": "codex",
                        "cwd": "/tmp",
                        "log_path": "/tmp/pane.log",
                        "created_at": "2026-07-02T00:00:00Z",
                        "status": "live",
                        "screen_state": verdict,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert len(loaded) == 1
    assert loaded[0].screen_state == verdict

    # (b) write -> load preserves the blob byte-for-value.
    write_registry(loaded, path=registry_path)
    reloaded = load_registry(path=registry_path)
    assert reloaded[0].screen_state == verdict

    # (c) Absent defaults to None (pre-bump rows need no migration).
    assert AgentEntry(name="x", harness="claude", cwd="/t", log_path="/t/x.log").screen_state is None


def test_us2_v1_entries_synthesized_at_read(tmp_path: Path, monkeypatch) -> None:
    """A v1 on-disk registry reads back with status='live' and last_message_at=None
    without mutating the file on disk."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    v1_payload = {
        "schema_version": 1,
        "agents": [
            {
                "name": "legacy",
                "provider": "claude",
                "cwd": "/tmp",
                "log_path": "/tmp/legacy.log",
                "claude_short_id": "abc12345",
                "codex_session_id": None,
                "gemini_session_id": None,
                "created_at": "2026-05-19T00:00:00Z",
            }
        ],
    }
    on_disk_text = json.dumps(v1_payload)
    registry_path.write_text(on_disk_text, encoding="utf-8")
    pre_mtime = registry_path.stat().st_mtime_ns

    loaded = load_registry(path=registry_path)

    assert len(loaded) == 1
    assert loaded[0].name == "legacy"
    assert loaded[0].status == "live"
    assert loaded[0].last_message_at is None
    # On-disk file is untouched (no auto-mutation during load).
    assert registry_path.read_text(encoding="utf-8") == on_disk_text
    assert registry_path.stat().st_mtime_ns == pre_mtime


def test_us2_first_write_upgrades_on_disk_to_current(tmp_path: Path, monkeypatch) -> None:
    """A v1 file rewritten via write_registry persists as the current schema_version.

    Phase 5 bumped SCHEMA_VERSION to 3; v1 upgrades pass through to v3
    on the next write_registry, synthesizing both status+last_message_at
    (v2 additions) and mcp_channel_id (v3 addition).
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import SCHEMA_VERSION, load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "agents": [
                    {
                        "name": "upgraded",
                        "provider": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/u.log",
                        "claude_short_id": None,
                        "codex_session_id": None,
                        "gemini_session_id": None,
                        "created_at": "2026-05-19T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    loaded = load_registry(path=registry_path)
    write_registry(loaded, path=registry_path)

    raw = json.loads(registry_path.read_text(encoding="utf-8"))
    assert raw["schema_version"] == SCHEMA_VERSION  # 4 today
    assert raw["agents"][0]["status"] == "live"
    assert raw["agents"][0]["last_message_at"] is None
    assert raw["agents"][0]["mcp_channel_id"] is None


def test_us2_v1_corrupt_identity_still_rejected(tmp_path: Path, monkeypatch) -> None:
    """v1 synthesis MUST NOT swallow content validation: a structurally corrupt
    row still raises. Post-x-8dfc the identity check is a shape check (an alien
    provider now loads, tested in test_load_gate_x8dfc), so the surviving guard
    is corruption -- an empty identity with no harness -- which must still raise
    even under v1 synthesis."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "agents": [
                    {
                        "name": "bad",
                        "provider": "",  # corrupt: empty identity, no harness
                        "cwd": "/tmp",
                        "log_path": "/tmp/b.log",
                        "claude_short_id": None,
                        "codex_session_id": None,
                        "gemini_session_id": None,
                        "created_at": "2026-05-19T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    with pytest.raises(RegistryVersionError, match="no valid identity"):
        load_registry(path=registry_path)


# ---------------------------------------------------------------------------
# Phase 5 US6 schema v3 — mcp_channel_id field on AgentEntry
# ---------------------------------------------------------------------------


def test_phase5_agent_entry_has_mcp_channel_id_default_none() -> None:
    """``AgentEntry`` defaults ``mcp_channel_id`` to ``None`` for socket-only agents."""
    from fno.agents.registry import AgentEntry

    entry = AgentEntry(
        name="x", harness="claude", cwd="/tmp", log_path="/tmp/x.log"
    )
    assert entry.mcp_channel_id is None


def test_phase5_v3_round_trip_preserves_mcp_channel_id(tmp_path: Path, monkeypatch) -> None:
    """v3 write -> read round-trip preserves a populated ``mcp_channel_id``."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="mcp-backed",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/x.log",
        mcp_channel_id="ch-abc-123",
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert len(loaded) == 1
    assert loaded[0].mcp_channel_id == "ch-abc-123"


def test_phase5_v2_entries_synthesized_to_v3_at_read(tmp_path: Path, monkeypatch) -> None:
    """A v2 on-disk registry reads back with ``mcp_channel_id=None`` for every
    row without mutating the file on disk."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    v2_payload = {
        "schema_version": 2,
        "agents": [
            {
                "name": "v2-row",
                "provider": "claude",
                "cwd": "/tmp",
                "log_path": "/tmp/v2.log",
                "claude_short_id": "abc12345",
                "codex_session_id": None,
                "gemini_session_id": None,
                "created_at": "2026-05-19T00:00:00Z",
                "status": "live",
                "last_message_at": None,
            }
        ],
    }
    on_disk_text = json.dumps(v2_payload)
    registry_path.write_text(on_disk_text, encoding="utf-8")
    pre_mtime = registry_path.stat().st_mtime_ns

    loaded = load_registry(path=registry_path)

    assert len(loaded) == 1
    assert loaded[0].name == "v2-row"
    assert loaded[0].mcp_channel_id is None
    # No auto-mutation; the file stays at v2 until next write.
    assert registry_path.read_text(encoding="utf-8") == on_disk_text
    assert registry_path.stat().st_mtime_ns == pre_mtime


def test_session_id_property_resolves_provider_specific_id() -> None:
    """AgentEntry.session_id maps to the provider's resume-target field."""
    from fno.agents.registry import AgentEntry

    claude = AgentEntry(
        name="c", harness="claude", cwd="/tmp", log_path="/tmp/c.log",
        short_id="abc12345",
    )
    codex = AgentEntry(
        name="x", harness="codex", cwd="/tmp", log_path="/tmp/x.log",
        harness_session_id="019e51db-a995-75e1-a3bb-3dde6b207661",
    )
    gemini = AgentEntry(
        name="g", harness="gemini", cwd="/tmp", log_path="/tmp/g.log",
        harness_session_id="gem-sess-1",
    )

    assert claude.session_id == "abc12345"
    assert codex.session_id == "019e51db-a995-75e1-a3bb-3dde6b207661"
    assert gemini.session_id == "gem-sess-1"


def test_session_id_property_none_when_uncaptured() -> None:
    """session_id is None when the provider id was never recorded."""
    from fno.agents.registry import AgentEntry

    entry = AgentEntry(
        name="x", harness="codex", cwd="/tmp", log_path="/tmp/x.log",
        harness_session_id=None,
    )
    assert entry.session_id is None


def test_session_id_property_excluded_from_asdict_serialization(
    tmp_path: Path, monkeypatch
) -> None:
    """The property must not leak into the on-disk dataclass serialization.

    asdict (used by write_registry) serializes fields only, not
    properties, so a write -> read round-trip stays byte-stable and does
    not introduce a phantom 'session_id' storage field.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="x", harness="codex", cwd="/tmp", log_path="/tmp/x.log",
        harness_session_id="sess-1",
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    raw = json.loads(registry_path.read_text(encoding="utf-8"))
    assert "session_id" not in raw["agents"][0]
    # Round-trips back to a real entry whose property still resolves.
    assert load_registry(path=registry_path)[0].session_id == "sess-1"


def test_session_id_property_matches_resume_cli_session_id_for() -> None:
    """The session_id property and resume_cli._session_id_for must agree.

    Both implement the same provider -> resume-target-id mapping in
    separate places (the property cannot delegate because _session_id_for
    duck-types against test fakes that lack the property). This parity
    test is the mechanical gate that fires if a future provider is added
    to one switch but not the other — the exact drift the property
    docstring warns about.
    """
    from fno.agents.registry import AgentEntry
    from fno.agents.resume_cli import _session_id_for

    cases = [
        ("claude", "short_id", "abc12345"),
        ("codex", "harness_session_id", "019e51db-a995-75e1-a3bb-3dde6b207661"),
        ("gemini", "harness_session_id", "gem-sess-1"),
    ]
    for provider, field_name, value in cases:
        entry = AgentEntry(
            name="t", harness=provider, cwd="/t", log_path="/t.log",
            **{field_name: value},
        )
        assert entry.session_id == _session_id_for(entry) == value, provider


def test_harness_session_id_fields_covers_known_providers() -> None:
    """Every dispatchable harness must have a resume-target field mapping.

    Guards against adding a provider to KNOWN_PROVIDERS without teaching
    session_id / _session_id_for how to resolve its resume id (which
    would silently return None for the new harness). Keyed on the sole
    HARNESS_SESSION_ID_FIELDS map (x-880e removed the provider alias).

    Containment, not equality: a pane-hostable harness can carry a resume
    field without a Python dispatch adapter. opencode is exactly that (x-830c) -
    it resumes via harness_session_id but is driven through the Rust spawn
    paths, and promoting it into KNOWN_PROVIDERS would leak it into headless/bg
    Python dispatch that has no opencode codepath. The upper bound stays
    READABLE_PROVIDERS so a typo'd harness still fails.
    """
    from fno.agents.providers import KNOWN_PROVIDERS, READABLE_PROVIDERS
    from fno.agents.registry import HARNESS_SESSION_ID_FIELDS

    assert set(KNOWN_PROVIDERS) <= set(HARNESS_SESSION_ID_FIELDS)
    assert set(HARNESS_SESSION_ID_FIELDS) <= set(READABLE_PROVIDERS)


# ---------------------------------------------------------------------------
# host_mode (interactive-drive node, ab-26b5fe82): schema add + AC1-EDGE
# default compatibility + cross-language round-trip parity.
# ---------------------------------------------------------------------------


def test_host_mode_interactive_round_trips(tmp_path: Path, monkeypatch) -> None:
    """An interactive host_mode written by Python reads back unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="bot2",
        harness="codex",
        cwd="/tmp",
        harness_session_id="019e7157-4236-7bb1-b274-ebbac6040ace",
        log_path="/tmp/bot2.log",
        host_mode="interactive",
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert loaded[0].host_mode == "interactive"


def test_host_mode_default_entry_reads_as_exec(tmp_path: Path, monkeypatch) -> None:
    """A Python entry left at the default (None) materializes as the concrete
    string "exec" after a write+read cycle (the coercion never surfaces None)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(name="a", harness="codex", cwd="/tmp", log_path="/tmp/a.log")
    assert entry.host_mode is None  # default before persistence
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert loaded[0].host_mode == "exec"


def test_host_mode_absent_key_coerces_to_exec(tmp_path: Path, monkeypatch) -> None:
    """AC1-EDGE: a row written before this change (no host_mode key) loads as
    'exec'. This is the shape a Rust daemon writes for an exec row
    (skip_serializing_if drops the key)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 3,
                "agents": [
                    {
                        "name": "legacy",
                        "provider": "codex",
                        "cwd": "/tmp",
                        "log_path": "/tmp/legacy.log",
                        "status": "live",
                        # no host_mode key at all
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert loaded[0].host_mode == "exec"


def test_host_mode_null_coerces_to_exec(tmp_path: Path, monkeypatch) -> None:
    """An explicit JSON null host_mode is coerced to 'exec' (not left as None)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 3,
                "agents": [
                    {
                        "name": "n",
                        "provider": "gemini",
                        "cwd": "/tmp",
                        "log_path": "/tmp/n.log",
                        "status": "live",
                        "host_mode": None,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert loaded[0].host_mode == "exec"


def test_host_mode_rust_written_interactive_row_reads_in_python(
    tmp_path: Path, monkeypatch
) -> None:
    """Cross-language parity (Rust -> Python): a Python-shaped row carrying an
    explicit host_mode='interactive' (the value a Rust daemon writes) loads as
    'interactive'. NOTE: a real Rust *PTY* interactive row also carries a
    non-empty short_id, which Python's AgentEntry(**row) rejects (the documented
    residual mixed-registry gap, reference_fno_agents_registry_cross_language_schema);
    this test pins the host_mode field contract on the Python-readable subset."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 3,
                "agents": [
                    {
                        "name": "bot2",
                        "provider": "codex",
                        "cwd": "/tmp",
                        "log_path": "/tmp/bot2.log",
                        "status": "live",
                        "codex_session_id": "019e7157-4236-7bb1-b274-ebbac6040ace",
                        "host_mode": "interactive",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert loaded[0].host_mode == "interactive"
    assert loaded[0].harness_session_id == "019e7157-4236-7bb1-b274-ebbac6040ace"


def test_host_mode_alien_value_rejected(tmp_path: Path, monkeypatch) -> None:
    """An alien non-null host_mode (typo, wrong type) is rejected like an alien
    status, not silently coerced -- defense-in-depth (sigma-review)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 3,
                "agents": [
                    {
                        "name": "typo",
                        "provider": "codex",
                        "cwd": "/tmp",
                        "log_path": "/tmp/typo.log",
                        "status": "live",
                        "host_mode": "intractive",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    with pytest.raises(RegistryVersionError):
        load_registry(path=registry_path)


def test_host_mode_attached_value_accepted(tmp_path: Path, monkeypatch) -> None:
    """An adopted claude --bg row (host_mode="attached", G1 x-26df) written by the
    Rust adopt path loads cleanly from Python instead of bricking the registry
    with RegistryVersionError (codex P1)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 5,
                "agents": [
                    {
                        "name": "cc-a1b2c3d4",
                        "provider": "claude",
                        "cwd": "/tmp",
                        "log_path": None,
                        "claude_session_uuid": "a1b2c3d4-1111-2222-3333-444455556666",
                        "claude_short_id": "a1b2c3d4",
                        "status": "live",
                        "host_mode": "attached",
                        "pid": 5001,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert loaded[0].host_mode == "attached"


# ---------------------------------------------------------------------------
# claude_session_uuid (Task 1.1 - full UUID resume target, distinct from jobId)
# ---------------------------------------------------------------------------


def test_claude_session_uuid_round_trips(tmp_path: Path, monkeypatch) -> None:
    """The full session UUID (the stream-json --resume target) persists and
    reads back unchanged, distinct from the 8-hex claude_short_id/jobId."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="claude-peer",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/claude-peer.log",
        short_id="7c5dcf5d",
        harness_session_id="019e7157-4236-7bb1-b274-ebbac6040ace",
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert loaded[0].short_id == "7c5dcf5d"
    assert loaded[0].harness_session_id == "019e7157-4236-7bb1-b274-ebbac6040ace"


def test_claude_session_uuid_defaults_to_none(tmp_path: Path, monkeypatch) -> None:
    """A new entry without the session id defaults to None (it is captured later,
    at adopt time, by the daemon host lane)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry

    entry = AgentEntry(
        name="c", harness="claude", cwd="/tmp", log_path="/tmp/c.log",
        short_id="7c5dcf5d",
    )
    assert entry.harness_session_id is None


def test_claude_session_uuid_absent_key_reads_as_none(tmp_path: Path, monkeypatch) -> None:
    """A row written before this change (no claude_session_uuid key) loads with
    None - back-compat with pre-stream-json registries and Rust exec rows that
    skip the key when absent."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 4,
                "agents": [
                    {
                        "name": "legacy-claude",
                        "provider": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/legacy.log",
                        "claude_short_id": "7c5dcf5d",
                        "status": "idle",
                        # no claude_session_uuid key at all
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert loaded[0].harness_session_id is None


# ---------------------------------------------------------------------------
# ab-b946b59c: mixed registry (Python rows + genuine Rust PTY rows) is fully
# Python-readable and round-trips without dropping the Rust-only PTY fields.
# ---------------------------------------------------------------------------


def _rust_pty_row(name: str = "worker-claude", **overrides) -> dict:
    """A registry row exactly as the Rust daemon serializes a live PTY agent:
    non-empty short_id/project_root + pid + worker socket, etc. Before the fix,
    AgentEntry(**row) raised TypeError on `short_id`, bricking every Python read.
    """
    row = {
        "name": name,
        "short_id": "wk-abc123",
        "provider": "claude",
        "cwd": "/Users/x/proj",
        "project_root": "/Users/x/proj",
        "messaging_socket_path": "/tmp/abilities/sock/wk-abc123.sock",
        "status": "live",
        "created_at": "2026-05-26T00:00:00Z",
        "pid": 4242,
        "pid_start_time": 123456789,
        "cc_session_id": "cc-xyz",
        "last_reconciled_at": "2026-05-26T01:00:00Z",
        "log_path": "/Users/x/.fno/agents/worker-claude.log",
    }
    row.update(overrides)
    return row


def _write_raw(registry_path: Path, rows: list[dict]) -> None:
    from fno.agents.registry import SCHEMA_VERSION

    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": SCHEMA_VERSION, "agents": rows}),
        encoding="utf-8",
    )


def test_rust_pty_row_loads_without_bricking(tmp_path: Path, monkeypatch) -> None:
    """A genuine Rust PTY row loads (no RegistryVersionError) and preserves the
    Rust-only fields, instead of TypeError-ing on `short_id`."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    _write_raw(registry_path, [_rust_pty_row()])

    loaded = load_registry(path=registry_path)  # must not raise
    assert len(loaded) == 1
    e = loaded[0]
    assert e.short_id == "wk-abc123"
    assert e.project_root == "/Users/x/proj"
    assert e.pid == 4242
    assert e.pid_start_time == 123456789
    assert e.messaging_socket_path == "/tmp/abilities/sock/wk-abc123.sock"
    assert e.cc_session_id == "cc-xyz"
    assert e.last_reconciled_at == "2026-05-26T01:00:00Z"


def test_rust_pty_row_with_stored_session_id_is_not_a_brick(
    tmp_path: Path, monkeypatch
) -> None:
    """A row that also serializes `session_id` (a computed @property on the
    Python side) loads -- the key is dropped, and the property recomputes the
    same projection from the provider's session-id field."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    _write_raw(registry_path, [_rust_pty_row(session_id="abc123")])

    loaded = load_registry(path=registry_path)  # must not raise
    # session_id is the claude projection of short_id (v9 unified transport key).
    assert loaded[0].session_id == "wk-abc123"


def test_mixed_registry_python_and_rust_rows(tmp_path: Path, monkeypatch) -> None:
    """A registry holding BOTH a thin Python ask row and a fat Rust PTY row
    loads both -- the mixed case PR #364 left unsolved."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    python_ask_row = {
        "name": "ask-codex",
        "provider": "codex",
        "cwd": "/p",
        "log_path": "/l",
        "codex_session_id": "sid",
        "status": "exited",
        "created_at": "2026-05-26T00:00:00Z",
    }
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    _write_raw(registry_path, [python_ask_row, _rust_pty_row(name="pty-claude")])

    loaded = load_registry(path=registry_path)
    by_name = {e.name: e for e in loaded}
    assert set(by_name) == {"ask-codex", "pty-claude"}
    assert by_name["ask-codex"].short_id == ""  # thin row defaults to empty
    assert by_name["pty-claude"].short_id == "wk-abc123"


def test_rust_pty_row_roundtrips_losslessly(tmp_path: Path, monkeypatch) -> None:
    """load -> write -> reload preserves the Rust-only PTY fields (no data loss
    when Python rewrites a registry that contains a Rust row)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    _write_raw(registry_path, [_rust_pty_row()])

    loaded = load_registry(path=registry_path)
    write_registry(loaded, path=registry_path)  # Python rewrites the store
    reloaded = load_registry(path=registry_path)

    e = reloaded[0]
    assert e.short_id == "wk-abc123"
    assert e.project_root == "/Users/x/proj"
    assert e.pid == 4242
    assert e.pid_start_time == 123456789
    assert e.cc_session_id == "cc-xyz"
    assert e.last_reconciled_at == "2026-05-26T01:00:00Z"


def test_python_write_emits_rust_readable_values(tmp_path: Path, monkeypatch) -> None:
    """A Python-authored row must serialize short_id/project_root as the EMPTY
    STRING (never null) so the Rust `String` fields deserialize it, and the
    Option fields as null. This is the load-bearing cross-language contract."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    entry = AgentEntry(
        name="py-ask",
        harness="codex",
        cwd="/p",
        log_path="/l",
        harness_session_id="sid",
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    raw = json.loads(registry_path.read_text(encoding="utf-8"))
    row = raw["agents"][0]
    # Rust `String` fields: empty string, NOT null (null would fail deserialize).
    assert row["short_id"] == ""
    assert row["project_root"] == ""
    # Rust `Option` fields: null is fine (reads as None).
    assert row["pid"] is None
    assert row["pid_start_time"] is None
    assert row["cc_session_id"] is None
    # `session_id` is a @property, never serialized as a stored field.
    assert "session_id" not in row


# ---------------------------------------------------------------------------
# 4a-G2: mux ref mirror + one-live-ref invariant
# ---------------------------------------------------------------------------


def test_mux_ref_roundtrips_and_reaches_rust_shape(tmp_path: Path, monkeypatch) -> None:
    """The mux ref survives a Python write/read cycle and serializes as the
    exact ``{"session": ..., "pane_id": ...}`` dict the Rust ``MuxRef``
    deserializes (X3 mixed-language rule)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="mux-agent",
        harness="claude",
        cwd="/p",
        log_path="/l",
        mux={"session": "work", "pane_id": 7},
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    raw = json.loads(registry_path.read_text(encoding="utf-8"))
    assert raw["agents"][0]["mux"] == {"session": "work", "pane_id": 7}

    loaded = load_registry(path=registry_path)
    assert loaded[0].mux == {"session": "work", "pane_id": 7}
    # Non-mux rows carry an explicit null (Rust reads it as None).
    entry_plain = AgentEntry(name="plain", harness="claude", cwd="/p", log_path="/l")
    assert entry_plain.mux is None


def test_write_registry_rejects_double_ref_rows(tmp_path: Path, monkeypatch) -> None:
    """One live ref per row (brief Locked 7): a mux ref alongside a non-empty
    short_id (a worker key or, since v9, a bg jobId) is refused at write time,
    and the prior store is left intact."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry(
        [AgentEntry(name="ok", harness="claude", cwd="/p", log_path="/l")],
        path=registry_path,
    )

    worker_double = AgentEntry(
        name="w",
        harness="codex",
        cwd="/p",
        log_path="/l",
        short_id="wk-1",
        mux={"session": "main", "pane_id": 1},
    )
    with pytest.raises(ValueError, match="one live ref"):
        write_registry([worker_double], path=registry_path)

    bg_double = AgentEntry(
        name="b",
        harness="claude",
        cwd="/p",
        log_path="/l",
        short_id="abcd1234",
        mux={"session": "main", "pane_id": 2},
    )
    with pytest.raises(ValueError, match="one live ref"):
        write_registry([bg_double], path=registry_path)

    # The refused writes must not have clobbered the store.
    raw = json.loads(registry_path.read_text(encoding="utf-8"))
    assert [r["name"] for r in raw["agents"]] == ["ok"]


# ---------------------------------------------------------------------------
# v9 claude_short_id removal + load-time backfill (x-1b1e)
# ---------------------------------------------------------------------------


def test_v9_agent_entry_has_no_claude_short_id_field() -> None:
    """AC3-HP: the removed field is not a constructor kwarg any more."""
    from fno.agents.registry import AgentEntry

    with pytest.raises(TypeError):
        AgentEntry(
            name="c", harness="claude", cwd="/tmp", log_path="/l",
            claude_short_id="deadbeef",  # type: ignore[call-arg]
        )


def test_v9_legacy_row_backfills_claude_short_id_into_short_id(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-EDGE: a legacy v8 row carrying only claude_short_id resolves by that
    value after load; on write-back it carries short_id and no claude_short_id."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 8,
                "agents": [
                    {
                        "name": "legacy",
                        "provider": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/legacy.log",
                        "claude_short_id": "7c5dcf5d",
                        "created_at": "2026-05-19T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    loaded = load_registry(path=registry_path)
    assert loaded[0].short_id == "7c5dcf5d"
    # The claude projection resolves by the backfilled short.
    assert loaded[0].session_id == "7c5dcf5d"

    # Write-back drops the legacy key and carries short_id at the current schema.
    write_registry(loaded, path=registry_path)
    raw = json.loads(registry_path.read_text(encoding="utf-8"))
    assert raw["schema_version"] == 10
    row = raw["agents"][0]
    assert "claude_short_id" not in row
    assert row["short_id"] == "7c5dcf5d"


def test_v9_conflicting_legacy_pair_keeps_short_id_and_warns(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC3-EDGE: a row carrying BOTH short_id and a DIFFERENT claude_short_id
    keeps short_id (the drift this removal kills), warns once, and the legacy
    value no longer resolves."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 8,
                "agents": [
                    {
                        "name": "conflict",
                        "provider": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/c.log",
                        "short_id": "aaaaaaaa",
                        "claude_short_id": "bbbbbbbb",
                        "created_at": "2026-05-19T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    loaded = load_registry(path=registry_path)
    assert loaded[0].short_id == "aaaaaaaa"  # short_id wins
    err = capsys.readouterr().err
    assert "conflict" in err and "keeping short_id" in err
