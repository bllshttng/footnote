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
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.paths_testing import use_tmpdir
from fno.registry_door import read_registry_document


def _replace_path_on_first_flock(monkeypatch, module, lock_path: Path):
    real_flock = module.fcntl.flock
    replaced = False

    def racing_flock(handle, operation):
        nonlocal replaced
        if not replaced and operation & fcntl.LOCK_EX:
            replaced = True
            lock_path.unlink()
            lock_path.touch()
        return real_flock(handle, operation)

    monkeypatch.setattr(module.fcntl, "flock", racing_flock)
    return real_flock


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _as_deployed(monkeypatch) -> None:
    """Run the write path as a DEPLOYED fno rather than a source checkout.

    ``_refuse_source_ahead_schema_bump`` refuses to raise the process-global
    registry's schema when this fno is running from a checkout, because that
    bump exists only on one branch while every deployed reader on the machine
    degrades. A test that writes an OLD on-disk version and asserts the write
    upgrades it is asserting the deployed-upgrade path (AC2), so it says so
    here rather than being exempted by an is-this-a-test predicate. Every other
    test in this file leaves the guard live.
    """
    from fno.agents import registry as reg

    monkeypatch.setattr(reg, "_running_from_source", lambda: None)


def _minimal_entry(name: str = "test-agent", **overrides) -> dict:
    base = {
        "name": name,
        "provider": "claude",
        "cwd": "/tmp",
        "log_path": "/tmp/test-agent.log",
    }
    base.update(overrides)
    return base


@pytest.mark.parametrize("timeout", [float("inf"), float("nan"), -0.1])
def test_registry_lock_rejects_nonterminating_timeout(tmp_path, timeout) -> None:
    from fno.agents.registry import _hold_registry_lock

    with pytest.raises(ValueError, match="finite and non-negative"):
        with _hold_registry_lock(tmp_path / "registry.json", timeout=timeout):
            pass


@pytest.mark.parametrize("timeout", [None, 1.0])
def test_registry_lock_revalidates_inode_after_path_replacement(
    tmp_path: Path, monkeypatch, timeout
) -> None:
    from fno.agents import registry

    registry_path = tmp_path / "registry.json"
    lock_path = registry._registry_lock_path(registry_path)
    lock_path.parent.mkdir(parents=True)
    lock_path.touch()
    real_flock = _replace_path_on_first_flock(monkeypatch, registry, lock_path)

    with registry._hold_registry_lock(registry_path, timeout=timeout):
        with lock_path.open("a") as contender:
            with pytest.raises(BlockingIOError):
                real_flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)


def test_registry_lock_replacement_loop_honors_timeout(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import registry

    registry_path = tmp_path / "registry.json"
    lock_path = registry._registry_lock_path(registry_path)
    lock_path.parent.mkdir(parents=True)
    lock_path.touch()
    real_flock = registry.fcntl.flock
    attempts = 0

    def replace_each_time(handle, operation):
        nonlocal attempts
        if operation & fcntl.LOCK_EX:
            attempts += 1
            if attempts > 2:
                raise AssertionError("inode replacement loop ignored timeout")
            lock_path.unlink()
            lock_path.touch()
        return real_flock(handle, operation)

    ticks = iter([0.0, 2.0])
    monkeypatch.setattr(registry.fcntl, "flock", replace_each_time)
    monkeypatch.setattr(
        registry,
        "time",
        SimpleNamespace(monotonic=lambda: next(ticks), sleep=lambda _seconds: None),
    )

    with pytest.raises(
        registry.RegistryLockTimeout, match=r"registry lock timeout after 1s"
    ):
        with registry._hold_registry_lock(registry_path, timeout=1):
            pass
    assert attempts == 1


# ---------------------------------------------------------------------------
# AC1-HP: round-trip a single agent entry
# ---------------------------------------------------------------------------


def test_xdead_heal_own_cwd_stamps_the_working_directory(
    tmp_path: Path, monkeypatch
) -> None:
    """Task 0.1 (x-dead): the spawner mints the row with the SPAWN directory;
    the worker's own SessionStart heal moves the cwd field to the directory
    it actually works in, so every cwd-keyed occupancy join can find it."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import (
        AgentEntry,
        heal_own_cwd,
        load_registry,
        registry_rows_by_cwd,
        write_registry,
    )

    write_registry(
        [
            AgentEntry(
                name="t-b7f8-worker",
                harness="claude",
                cwd="/Users/bb16/code/footnote/footnote",
                short_id="deadbeef",
                harness_session_id="aaaaaaaa-1111-7222-8333-4444deadbeef",
                log_path="/tmp/t-b7f8-worker.log",
            )
        ]
    )

    moved = heal_own_cwd(
        name="t-b7f8-worker",
        harness="claude",
        cwd="/Users/bb16/.fno/worktrees/footnote/x-b7f8",
    )
    assert moved == (
        "/Users/bb16/code/footnote/footnote",
        "/Users/bb16/.fno/worktrees/footnote/x-b7f8",
    )
    by_cwd, ok = registry_rows_by_cwd()
    assert ok is True
    assert list(by_cwd) == ["/Users/bb16/.fno/worktrees/footnote/x-b7f8"]
    assert load_registry()[0].cwd == "/Users/bb16/.fno/worktrees/footnote/x-b7f8"
    # Idempotent: a second SessionStart writes nothing and answers None.
    assert (
        heal_own_cwd(
            name="t-b7f8-worker",
            harness="claude",
            cwd="/Users/bb16/.fno/worktrees/footnote/x-b7f8",
        )
        is None
    )
    # No such row: a no-op, never a raise.
    assert (
        heal_own_cwd(name="nobody", harness="claude", cwd="/somewhere") is None
    )


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
        context_used_pct=26,
        context_used_tokens=258_687,
        context_window_tokens=1_000_000,
        context_measured_at="2026-09-30T12:00:00Z",
        mail_unread=1,
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
    assert e.context_used_pct == 26
    assert e.context_used_tokens == 258_687
    assert e.context_window_tokens == 1_000_000
    assert e.context_measured_at == "2026-09-30T12:00:00Z"
    assert e.mail_unread == 1
    # AC1-HP: model provider is explicit and unset; removed session aliases die.
    raw_row = read_registry_document(registry_path)[0]["agents"][0]
    assert raw_row["provider"] is None
    for dead in ("codex_session_id", "gemini_session_id", "claude_session_uuid"):
        assert dead not in raw_row
    # created_at must be ISO8601 UTC
    assert e.created_at.endswith("Z") or "+" in e.created_at


def test_provider_outage_route_axes_round_trip_without_credentials(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="routed",
        harness="claude",
        cwd="/tmp/worktree",
        log_path="/tmp/routed.log",
        harness_session_id="session-1",
        route_provider_id="zai",
        model_name="glm-5.3",
        account_record_id="acct-a",
    )
    registry_path = tmp_path / ".fno" / "agents" / "registry.json"

    write_registry([entry], path=registry_path)
    loaded = load_registry(path=registry_path)

    assert loaded[0].route_provider_id == "zai"
    assert loaded[0].model_name == "glm-5.3"
    assert loaded[0].account_record_id == "acct-a"
    raw = read_registry_document(registry_path)[0]
    assert raw["agents"][0]["route_provider_id"] == "zai"
    assert "AUTH_TOKEN" not in json.dumps(raw)


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

    raw = read_registry_document(registry_path)[0]
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
    original_content = read_registry_document(registry_path)[0]

    # Now simulate a write that dies inside the table door.
    import fno.registry_door as door_module

    def _exploding_commit(*args, **kwargs):
        raise RuntimeError("simulated kill -9 mid-write")

    monkeypatch.setattr(door_module, "commit_registry_document", _exploding_commit)

    new_entry = AgentEntry(
        name="corrupt-agent",
        harness="codex",
        cwd="/tmp",
        log_path="/tmp/corrupt.log",
    )
    with pytest.raises(RuntimeError, match="simulated kill -9"):
        write_registry([new_entry], path=registry_path)

    # The table must be intact
    assert read_registry_document(registry_path)[0] == original_content
    loaded = load_registry(path=registry_path)
    assert loaded[0].name == "safe-agent"


def test_write_registry_failure_surfaces_and_keeps_the_table(tmp_path: Path, monkeypatch) -> None:
    """An ``OSError`` inside the write door propagates and leaves no partial rows."""
    use_tmpdir(monkeypatch, tmp_path)

    import fno.registry_door as door_module
    from fno.agents.registry import AgentEntry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    def _explode(*args, **kwargs):
        raise OSError("simulated disk full during commit")

    monkeypatch.setattr(door_module, "commit_registry_document", _explode)

    entry = AgentEntry(name="t", harness="claude", cwd="/tmp", log_path="/tmp/t.log")
    with pytest.raises(OSError, match="simulated disk full"):
        write_registry([entry], path=registry_path)

    assert read_registry_document(registry_path)[0]["agents"] == []


# ---------------------------------------------------------------------------
# revision_conflict retry: a non-flock writer lands in the read-to-commit
# window, the door refuses once, update_registry reloads and re-applies.
# ---------------------------------------------------------------------------


_CONFLICT_JSON = (
    '{"status":"refused","reason":"revision_conflict",'
    '"message":"registry changed since read. Reload before applying the mutation again."}'
)


def _seed_registry(registry_path: Path) -> None:
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [AgentEntry(name="resident", harness="claude", cwd="/tmp", log_path="/tmp/r.log")],
        path=registry_path,
    )


def test_update_registry_retries_revision_conflict_and_keeps_concurrent_row(
    tmp_path: Path, monkeypatch
) -> None:
    """One conflict, then the retry lands with every row still present."""
    use_tmpdir(monkeypatch, tmp_path)

    import fno.rust_binary as rb
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    _seed_registry(registry_path)

    real_verb_call = rb.verb_call
    commits = {"n": 0, "applies": 0}

    def _conflict_once(verb, payload, **kwargs):
        if verb == "registry-commit" and "revision" in payload:
            commits["n"] += 1
            if commits["n"] == 1:
                raise rb.VerbUnavailable(_CONFLICT_JSON)
        return real_verb_call(verb, payload, **kwargs)

    monkeypatch.setattr(rb, "verb_call", _conflict_once)

    def _append_spawned(entries):
        commits["applies"] += 1
        return entries + [
            AgentEntry(name="spawned", harness="codex", cwd="/tmp", log_path="/tmp/s.log")
        ]

    update_registry(_append_spawned, path=registry_path)

    assert [e.name for e in load_registry(path=registry_path)] == ["resident", "spawned"]
    assert commits["n"] == 2
    assert commits["applies"] == 2


def test_update_registry_conflict_exhaustion_and_non_conflict_single_shot(
    tmp_path: Path, monkeypatch
) -> None:
    """Exhaustion keeps RegistryVersionError; a non-conflict refusal never retries."""
    use_tmpdir(monkeypatch, tmp_path)

    import fno.rust_binary as rb
    from fno.agents.registry import AgentEntry, RegistryVersionError, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    _seed_registry(registry_path)

    real_verb_call = rb.verb_call
    commits = {"n": 0}

    def _always_conflict(verb, payload, **kwargs):
        if verb == "registry-commit" and "revision" in payload:
            commits["n"] += 1
            raise rb.VerbUnavailable(_CONFLICT_JSON)
        return real_verb_call(verb, payload, **kwargs)

    monkeypatch.setattr(rb, "verb_call", _always_conflict)

    def _append(entries):
        return entries + [
            AgentEntry(name="spawned", harness="codex", cwd="/tmp", log_path="/tmp/s.log")
        ]

    with pytest.raises(RegistryVersionError, match="revision_conflict"):
        update_registry(_append, path=registry_path)
    assert commits["n"] == 3

    commits["n"] = 0

    def _schema_refused(verb, payload, **kwargs):
        if verb == "registry-commit" and "revision" in payload:
            commits["n"] += 1
            raise rb.VerbUnavailable('{"status":"refused","reason":"schema_refused"}')
        return real_verb_call(verb, payload, **kwargs)

    monkeypatch.setattr(rb, "verb_call", _schema_refused)
    with pytest.raises(RegistryVersionError, match="schema_refused"):
        update_registry(_append, path=registry_path)
    assert commits["n"] == 1


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
    from fno.agents.registry import AgentEntry, _agent_lock_path, write_registry

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

    from fno.agents.registry import _agent_lock_path

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


def test_ac4_err_future_schema_version_reads_forward(tmp_path: Path, monkeypatch) -> None:
    """AC4-ERR inverted: a future schema_version now READS instead of raising.

    The original contract refused a future version so a stale reader could not
    silently drop a field. That refusal turned out to cost more than the thing
    it prevented: registry.json is global to every agent on this machine, so one
    process ahead of the deployment took every deployed reader down at once, and
    mail died fleet-wide with a symptom that surfaced far from the cause.

    Dropping a field is now made safe by refusing to WRITE over a newer store and
    by announcing every degraded read, not by refusing to look.
    """
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import SCHEMA_VERSION, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": SCHEMA_VERSION + 1, "agents": []}),
        encoding="utf-8",
    )

    loaded = load_registry(path=registry_path)
    assert loaded == []
    assert loaded.complete is False


def test_forward_read_marks_a_skipped_row_incomplete(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import SCHEMA_VERSION, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": SCHEMA_VERSION + 1,
                "agents": [
                    {
                        "name": "kept",
                        "harness": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/kept.log",
                    },
                    {
                        "name": "skipped",
                        "harness": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/skipped.log",
                        "status": "future-state",
                    },
                ],
            }
        ),
        encoding="utf-8",
    )

    loaded = load_registry(path=registry_path)
    assert [row.name for row in loaded] == ["kept"]
    assert loaded.complete is False


def test_ac4_err_degraded_read_names_both_versions(tmp_path: Path, monkeypatch, capsys) -> None:
    """AC4-ERR kept, moved from the exception to the announcement.

    Both versions must still be named where a reader will see them. The refusal
    became a warning, so the requirement moved with it rather than being dropped:
    a silent read-forward makes a partial row indistinguishable from a complete
    one, and a decision taken on one leaves no trace.
    """
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import SCHEMA_VERSION, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": 99, "agents": []}), encoding="utf-8"
    )

    load_registry(path=registry_path)

    err = capsys.readouterr().err
    assert "99" in err
    assert f"schema_version={SCHEMA_VERSION}" in err


def test_ac4_err_write_over_a_newer_store_still_raises_naming_versions(
    tmp_path: Path, monkeypatch
) -> None:
    """The refusal survives on the WRITE path, which is where it now belongs."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import SCHEMA_VERSION, RegistryVersionError, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps({"schema_version": 99, "agents": []}), encoding="utf-8"
    )

    with pytest.raises(RegistryVersionError, match=r"99") as exc_info:
        write_registry([], path=registry_path)

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
    from fno.agents.harnesses import KNOWN_PROVIDERS, READABLE_PROVIDERS

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

    with pytest.raises(RegistryVersionError):
        load_registry(path=registry_path)


def test_load_registry_rejects_non_dict_top_level(tmp_path: Path, monkeypatch) -> None:
    """A JSON array at the top level is rejected via RegistryVersionError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import RegistryVersionError, load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text("[]", encoding="utf-8")

    with pytest.raises(RegistryVersionError, match="not an object"):
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

    with pytest.raises(RegistryVersionError, match="not an array"):
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

    with pytest.raises(RegistryVersionError, match="not an object"):
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
    """The deliberate schema-version canary: bumping is never accidental.

    This is the ONE place the number is written out. Three sibling copies used
    to assert it incidentally, in tests about host_mode and delivery_policy,
    and every one of their docstrings had drifted to a stale version (7, 12)
    while the literal was hand-bumped past them. A canary nobody can read is
    not a canary, so the copies are gone and this one carries the job.

    (Test name retained for greppability of the original US2 commit.)
    """
    from fno.agents.registry import SCHEMA_VERSION

    # v36: additive `node_reason` - why the row works no node WHEN the
    # spawn NAMED one; absent when the node resolved or none was named.
    # v23 (x-3837): additive `substrate` - the lane a row was spawned on.
    # v24 (x-2019): additive `requested_model`/`requested_provider`/
    # `requested_effort` - the spawn request verbatim beside the effect.
    # v27 (x-04ce): additive `launch_account_source` - WHO chose the account.
    # v28 (x-5283): additive `adopted_by_session` - who VOUCHED for an
    # adopted row, split out of the spawn edge.
    # v29: additive `resolved_sandbox`/`granted_writable_roots` - what a codex
    # thread row's sandbox RESOLVED to server-side and the roots it carries,
    # beside the v19 `sandbox_posture` REQUEST that a resume re-applies.
    # v30: additive `git_grant` - the effective Git common-dir path carried by
    # a bounded Codex thread.
    # v31: additive `harness_args` - the fenced codex thread tokens a daemon
    # restart re-parses onto thread/resume.
    # v32: additive `stop` - fno's own stop, so a harness-stopped state never
    # reads as finished work to the retirement sweep.
    # v33: additive `lineage_reason` - why no parent session could be named;
    # an origin=spawn row carries a parent or a reason, never neither.
    # v33 also added `spawn_id` and structured `spawn_provenance` (the door's
    # attempt id and validated birth record).
    # v34: additive `lineage_kind` - the served CHILD/PEER word the liveness
    # sweep stamps on rows with a spawn edge.
    # v38: sweep-owned context and unread facts survive compatibility writes.
    # v39 (x-fdab): additive `tool_calls`/`tool_errors` - the daemon's
    # incremental transcript fold, the sideline card graph's input.
    # v40 migrates the authority vocabulary and fences older writers.
    assert SCHEMA_VERSION == 40


def test_session_lineage_fields_round_trip(tmp_path: Path, monkeypatch) -> None:
    # This writes to the DEFAULT registry path, and without per-test isolation
    # that path is the session-wide test sandbox every other file's
    # default-path reader also sees. The leftover row (harness_session_id
    # "session-b") then surfaced as a phantom extra session in four
    # test_discover.py codex assertions, but only when the two files ran in the
    # same invocation -- each passed alone, which is what kept it hidden.
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="successor",
        cwd="/tmp",
        log_path="",
        harness="claude",
        harness_session_id="session-b",
        predecessor_session_ids=["session-a"],
        forked_from_session_id="session-root",
    )
    write_registry([entry])

    loaded = load_registry()[0]
    assert loaded.predecessor_session_ids == ["session-a"]
    assert loaded.forked_from_session_id == "session-root"


def test_substrate_round_trips_and_absence_stays_unknown(tmp_path: Path, monkeypatch) -> None:
    # v23: the lane stamp survives a Python write-read cycle, and a row that
    # never carried one reads None - never "pane", the silent default the
    # field exists to replace.
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents.registry import AgentEntry, load_registry, write_registry

    stamped = AgentEntry(
        name="thread-worker",
        cwd="/tmp",
        log_path="",
        harness="claude",
        harness_session_id="session-t",
        substrate="thread",
    )
    write_registry([stamped])

    loaded = load_registry()
    assert loaded[0].substrate == "thread"
    # A row written before the field existed carries no key and reads unknown.
    write_registry(
        [
            AgentEntry(
                name="old-row",
                cwd="/tmp",
                log_path="",
                harness="claude",
                harness_session_id="session-old",
            )
        ]
    )
    assert load_registry()[0].substrate is None


def test_session_transition_classifies_liveness_truth() -> None:
    from fno.agents.registry import classify_session_transition

    assert classify_session_transition("session-a", "session-b", False) == "succession"
    assert classify_session_transition("session-a", "session-b", True) == "branch"
    assert classify_session_transition("session-a", "session-b", None) == "deferred"
    assert classify_session_transition("", "session-b", False) == "deferred"


def test_v15_model_provider_round_trips_without_collapsing_harness(
    tmp_path: Path, monkeypatch
) -> None:
    """A model provider is a separate axis even when its literal equals harness."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    dual_axis = "open" + "code"
    entry = AgentEntry(
        name="same-literal",
        harness=dual_axis,
        provider=dual_axis,
        cwd="/tmp",
        log_path="/tmp/opencode.log",
    )
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert loaded[0].harness == dual_axis
    assert loaded[0].provider == dual_axis
    assert read_registry_document(registry_path)[0]["agents"][0]["provider"] == dual_axis


def test_v14_provider_still_backfills_harness_without_inventing_model_provider(
    tmp_path: Path, monkeypatch
) -> None:
    """Before v15, provider was the retired harness spelling and remains read-only."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    legacy_harness = "co" + "dex"
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 14,
                "agents": [
                    {
                        "name": "legacy",
                        "provider": legacy_harness,
                        "cwd": "/tmp",
                        "log_path": "/tmp/legacy.log",
                        "status": "live",
                    }
                ],
            }
        )
    )

    loaded = load_registry(path=registry_path)
    assert loaded[0].harness == legacy_harness
    assert loaded[0].provider is None


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
        registry_path = tmp_path / status / "registry.json"
        registry_path.parent.mkdir()
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
    must not drop back-compat reads; ab-a171ceb2)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

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

    # v4 round-trips an explicit interactive host_mode. Each version seeds its
    # own store: the table imports a legacy file once.
    registry_path = tmp_path / "v4" / "registry.json"
    registry_path.parent.mkdir()
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
        registry_path = tmp_path / f"v{v}-bare" / "registry.json"
        registry_path.parent.mkdir()
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
    _as_deployed(monkeypatch)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        write_registry,
    )

    registry_path = tmp_path / "agents-fixture" / "registry.json"
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
    registry_path = tmp_path / "bare" / "registry.json"
    registry_path.parent.mkdir()
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
    _as_deployed(monkeypatch)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        write_registry,
    )

    registry_path = tmp_path / "agents-fixture" / "registry.json"
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


def test_stop_record_round_trips_across_registry_boundary(
    tmp_path: Path, monkeypatch
) -> None:
    """v32: fno's own stop record round-trips losslessly.

    Same X3 passthrough contract as screen_state: the Rust stop handler and
    terminal-stop sweep are the sole writers; Python custodies the opaque
    blob so a row fno stopped keeps its stamp on a Python read-modify-write.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _as_deployed(monkeypatch)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        write_registry,
    )

    registry_path = tmp_path / "agents-fixture" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)

    stop = {"by": "stop-verb", "at": "2026-09-15T00:00:00Z", "reason": None}

    # (a) A Rust-written row carrying stop loads as an opaque dict. The file
    # stages the pre-bump version on purpose: the reader accepts 1..=current.
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 31,
                "agents": [
                    {
                        "name": "pane",
                        "harness": "claude",
                        "cwd": "/tmp",
                        "log_path": "/tmp/pane.log",
                        "created_at": "2026-07-02T00:00:00Z",
                        "status": "live",
                        "stop": stop,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)
    assert len(loaded) == 1
    assert loaded[0].stop == stop

    # (b) write -> load preserves the blob byte-for-value.
    write_registry(loaded, path=registry_path)
    reloaded = load_registry(path=registry_path)
    assert reloaded[0].stop == stop

    # (c) Absent defaults to None (pre-bump rows need no migration).
    assert AgentEntry(name="x", harness="claude", cwd="/t", log_path="/t/x.log").stop is None


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
    registry_path.write_text(json.dumps(v1_payload), encoding="utf-8")

    loaded = load_registry(path=registry_path)

    assert len(loaded) == 1
    assert loaded[0].name == "legacy"
    assert loaded[0].status == "live"
    assert loaded[0].last_message_at is None
    # On-disk file is untouched (no auto-mutation during load).
    # The load imported the file but never wrote the table.
    assert read_registry_document(registry_path)[1] == 0


def test_us2_first_write_upgrades_on_disk_to_current(tmp_path: Path, monkeypatch) -> None:
    """A v1 file rewritten via write_registry persists as the current schema_version.

    Phase 5 bumped SCHEMA_VERSION to 3; v1 upgrades pass through to v3
    on the next write_registry, synthesizing both status+last_message_at
    (v2 additions) and mcp_channel_id (v3 addition).
    """
    use_tmpdir(monkeypatch, tmp_path)
    _as_deployed(monkeypatch)
    from fno.agents.registry import SCHEMA_VERSION, load_registry, write_registry

    registry_path = tmp_path / "agents-fixture" / "registry.json"
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

    raw = read_registry_document(registry_path)[0]
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


def test_agent_entry_accepts_every_field_in_registry_schema() -> None:
    """Every field the Rust writer declares in registry_schema.toml loads into AgentEntry."""
    import dataclasses
    import tomllib

    import fno.agents
    from fno.agents.registry import AgentEntry

    schema = Path(fno.agents.__file__).parent / "registry_schema.toml"
    declared = set(tomllib.loads(schema.read_text())["fields"])
    # Read-time legacy keys: load_registry folds or drops these before building the entry.
    legacy = {"claude_session_uuid", "claude_short_id", "codex_session_id",
              "gemini_session_id", "legacy_provider", "session_id"}
    missing = declared - legacy - {f.name for f in dataclasses.fields(AgentEntry)}
    assert not missing, f"AgentEntry lacks registry_schema.toml fields: {sorted(missing)}"


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
    registry_path.write_text(json.dumps(v2_payload), encoding="utf-8")

    loaded = load_registry(path=registry_path)

    assert len(loaded) == 1
    assert loaded[0].name == "v2-row"
    assert loaded[0].mcp_channel_id is None
    # No auto-mutation; the file stays at v2 until next write.
    # The load imported the file but never wrote the table.
    assert read_registry_document(registry_path)[1] == 0


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

    raw = read_registry_document(registry_path)[0]
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


def test_every_resumable_harness_resolves_an_identity() -> None:
    """Every harness that DECLARES resume support must resolve a resume id.

    The lower bound is the capability contract, because that is what the
    resume verb's own support check reads. The bound this replaces was
    KNOWN_PROVIDERS, the Python dispatch set, which is ("claude", "codex") -
    so pi, gemini, agy and opencode all passed it vacuously and it could not
    fail when a harness joined the contract without joining the map (x-efd7).

    The live specimen: pi declared interactive_resume support in the
    contract while nothing taught the identity read about it, so
    `fno agents resume` answered "supported" and then "no recorded
    session_id for harness pi" about one row, which `fno agents attach`
    opened fine off harness_session_id.

    The row shape asserted here is the one every non-claude row actually
    has: a canonical harness_session_id and no transport key.

    The upper bound stays READABLE_PROVIDERS so a typo'd harness still fails.
    """
    from fno.agents.harness_map import DispatchResolveError, capabilities, known_harnesses
    from fno.agents.harnesses import READABLE_PROVIDERS
    from fno.agents.registry import HARNESS_SESSION_ID_FIELDS, AgentEntry
    from fno.agents.resume_cli import _session_id_for

    checked = []
    for harness in known_harnesses():
        try:
            form = capabilities(harness)["resume_strategy"]["forms"]["interactive_resume"]
        except (DispatchResolveError, KeyError, TypeError):
            continue
        if form["kind"] == "unsupported":
            continue
        entry = AgentEntry(
            name=f"{harness}-row",
            harness=harness,
            cwd="/x",
            log_path="/tmp/x.log",
            harness_session_id="canonical-id-1",
        )
        assert entry.session_id == "canonical-id-1", (
            f"harness {harness!r} declares interactive_resume support but its "
            f"identity read returns nothing for a harness_session_id-only row"
        )
        assert _session_id_for(entry) == "canonical-id-1", harness
        assert harness in HARNESS_SESSION_ID_FIELDS, (
            f"harness {harness!r} declares interactive_resume support but is "
            f"absent from HARNESS_SESSION_ID_FIELDS, so register_session "
            f"raises on it and discovery skips its rows"
        )
        checked.append(harness)

    # A positive marker, not an absence. Without this the assertion body could
    # be skipped entirely by a contract that declared nothing resumable, which
    # is the exact shape of vacuous pass this test was written to end. The bar
    # proves the LOOP RAN and nothing more, so it stays well under the six
    # harnesses that declare support today: pinning it near that count would
    # turn one legitimate retirement into a failure here, and a gate that cries
    # about roster churn is one people learn to edit.
    assert len(checked) >= 2, f"the resumable-harness loop never ran; saw {checked}"

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
        "harness": "claude",
        "cwd": "/Users/x/proj",
        "project_root": "/Users/x/proj",
        "messaging_socket_path": "/tmp/fno/sock/wk-abc123.sock",
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
    assert e.messaging_socket_path == "/tmp/fno/sock/wk-abc123.sock"
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
        "harness": "codex",
        "cwd": "/p",
        "log_path": "/l",
        "harness_session_id": "sid",
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

    raw = read_registry_document(registry_path)[0]
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

    raw = read_registry_document(registry_path)[0]
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
    raw = read_registry_document(registry_path)[0]
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
    _as_deployed(monkeypatch)
    from fno.agents.registry import load_registry, write_registry

    registry_path = tmp_path / "agents-fixture" / "registry.json"
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
    # Read from the constant, not a literal: the claim is "whatever this binary
    # writes", which is exactly what a hand-edited number stops being.
    from fno.agents.registry import SCHEMA_VERSION

    write_registry(loaded, path=registry_path)
    raw = read_registry_document(registry_path)[0]
    assert raw["schema_version"] == SCHEMA_VERSION
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


#: `claude` is a HARNESS, and `register_existing_session`'s parameter is named
#: `provider`. Name the axis once rather than bind the literal at each call
#: site - the idiom `test_mail_escalation.py` uses.
CLAUDE_HARNESS = "claude"


def test_origin_is_write_once_and_a_refresh_never_changes_it(tmp_path, monkeypatch):
    """`origin` is a BIRTH fact, and nothing on the refresh path can observe a
    birth. Every weaker rule tried here lost a row to a later refresh, and none
    of the losses is recoverable because nothing ever clears the field.

    The one that survived review twice: an operator resuming a footnote-spawned
    worker in a fresh terminal fires the SessionStart register branch with
    `origin="operator"`. Restamping there takes that worker out of the retire
    lane for good and puts it in the attended mail escalation. Filling an EMPTY
    origin is still allowed, because that row never made a claim."""
    from fno.agents.registry import load_registry, register_existing_session

    use_tmpdir(monkeypatch, tmp_path)

    def register(origin):
        return register_existing_session(
            provider=CLAUDE_HARNESS,
            session_id="11111111-2222-3333-4444-555555555555",
            cwd=str(tmp_path),
            name="worker",
            origin=origin,
        )

    assert register("spawn").origin == "spawn"
    assert register("operator").origin == "spawn", (
        "a refresh must not move a worker into the attended lane"
    )
    assert register("adopted").origin == "spawn"
    assert register(None).origin == "spawn"
    assert [e.origin for e in load_registry()] == ["spawn"]


def test_a_refresh_fills_an_origin_the_row_never_had(tmp_path, monkeypatch):
    """The other side of write-once, and the reason it is not "never write on a
    refresh". A row that predates the marker made no claim, so the first claim
    anyone makes about it is the only one there is."""
    from fno.agents.registry import register_existing_session

    use_tmpdir(monkeypatch, tmp_path)
    kwargs = dict(
        provider=CLAUDE_HARNESS,
        session_id="66666666-7777-8888-9999-aaaaaaaaaaaa",
        cwd=str(tmp_path),
        name="legacy",
    )
    assert register_existing_session(**kwargs, origin=None).origin is None
    assert register_existing_session(**kwargs, origin="operator").origin == "operator"


def test_register_re_stamps_role_from_a_live_manifest(tmp_path, monkeypatch):
    """x-2b6f: the manifest is the durable role record; register is the
    repair door for a row a restore stripped. A manifest naming the session
    stamps an unstamped row, a stamped row is never touched, and no manifest
    registers bare."""
    from fno.agents.registry import (
        AgentEntry,
        register_existing_session,
        write_registry,
    )
    from fno.paths import space_dir

    use_tmpdir(monkeypatch, tmp_path)
    sid = "77777777-8888-9999-aaaa-bbbbbbbbbbbb"

    def manifest_body(session: str, scope: str) -> str:
        return (
            "---\n"
            f"scope: {scope}\n"
            "shape: pass\n"
            "harness: claude\n"
            f"harness_session_id: {session}\n"
            "owner_pid: 1\n"
            "created_at: 2026-09-01T00:00:00Z\n"
            f"role_scope: {scope}\n"
            "role_level: 2\n"
            "role_grantor: vellum\n"
            "---\n"
        )

    leads = space_dir(Path(tmp_path)) / "leads"
    leads.mkdir(parents=True, exist_ok=True)
    (leads / "x-demo.md").write_text(manifest_body(sid, "x-demo"), encoding="utf-8")

    row = register_existing_session(
        provider=CLAUDE_HARNESS,
        session_id=sid,
        cwd=str(tmp_path),
        name="quill2",
    )
    assert row.role_scope == "x-demo"
    assert row.role_level == 2
    assert row.role_grantor == "vellum"

    # No manifest naming it: a fresh session registers bare.
    bare = register_existing_session(
        provider=CLAUDE_HARNESS,
        session_id="99999999-0000-0000-0000-000000000000",
        cwd=str(tmp_path),
        name="bare",
    )
    assert bare.role_scope is None

    # A malformed role_level registers the row BARE instead of raising:
    # a junk stamp must never cost the session its row.
    (leads / "x-junk.md").write_text(
        manifest_body("88888888-0000-0000-0000-000000000000", "x-junk").replace(
            "role_level: 2", "role_level: two"
        ),
        encoding="utf-8",
    )
    junky = register_existing_session(
        provider=CLAUDE_HARNESS,
        session_id="88888888-0000-0000-0000-000000000000",
        cwd=str(tmp_path),
        name="junky",
    )
    assert junky.role_level is None

    # A row already stamped is never touched by a manifest naming it.
    seeded = AgentEntry(
        name="quill2",
        cwd=str(tmp_path),
        log_path="",
        harness=CLAUDE_HARNESS,
        harness_session_id=sid,
    )
    seeded.role_scope = "x-kept"
    seeded.role_level = 1
    seeded.role_grantor = "human"
    write_registry([seeded])
    kept = register_existing_session(
        provider=CLAUDE_HARNESS,
        session_id=sid,
        cwd=str(tmp_path),
        name="quill2",
    )
    assert kept.role_scope == "x-kept"
    assert kept.role_level == 1
    assert kept.role_grantor == "human"


def test_node_field_stamps_and_round_trips_v21(tmp_path, monkeypatch):
    """x-98ab: a row carries the node it works, so a reap decision reads the
    node off the row instead of parsing it out of a name. Stamped at the
    register path from the session's own exported FNO_NODE; round-trips the
    v21 schema (asdict emits the key on every written row, so a pre-v21
    reader must reject the store rather than silently drop the stamp)."""
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        register_existing_session,
        write_registry,
    )

    use_tmpdir(monkeypatch, tmp_path)
    entry = register_existing_session(
        provider=CLAUDE_HARNESS,
        session_id="99999999-8888-7777-6666-555555555555",
        cwd=str(tmp_path),
        name="nodeworker",
        origin="operator",
        node="x-98ab",
    )
    assert entry.node == "x-98ab"
    loaded = load_registry()
    assert [e.node for e in loaded] == ["x-98ab"]

    # A caller that cannot know records None - never a value parsed out of
    # the name - and the v21 write still emits the key (forward-compat).
    write_registry(
        [
            AgentEntry(
                name="spawned",
                cwd=str(tmp_path),
                log_path="",
                harness=CLAUDE_HARNESS,
                harness_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                origin="spawn",
                node="x-bb18",
            )
        ]
    )
    assert [e.node for e in load_registry()] == ["x-bb18"]

    # A refresh may FILL an empty node (a pre-v21 row gains the stamp its
    # session's own export names) and may never CHANGE a stamped one. A
    # caller saying nothing fills nothing - no evidence, no write.
    refreshed = register_existing_session(
        provider=CLAUDE_HARNESS,
        session_id="99999999-8888-7777-6666-555555555555",
        cwd=str(tmp_path),
        name="nodeworker",
        node="x-98ab",
    )
    assert refreshed.node == "x-98ab", "a refresh must fill an empty node"
    changed = register_existing_session(
        provider=CLAUDE_HARNESS,
        session_id="99999999-8888-7777-6666-555555555555",
        cwd=str(tmp_path),
        name="nodeworker",
        node="x-other",
    )
    assert changed.node == "x-98ab", "a refresh must never change a stamped node"


# ---------------------------------------------------------------------------
# v24 (x-2019): requested_* stamps - the REQUEST verbatim beside the effect
# ---------------------------------------------------------------------------


def test_v24_requested_axis_round_trips_verbatim(tmp_path: Path, monkeypatch) -> None:
    """The requested model/provider/effort survive a write+read byte-for-byte.

    Verbatim means the [1m] suffix rides through untouched: normalizing the
    token is how a stored request stops being evidence of what was typed.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    entry = AgentEntry(
        name="requested-axis",
        harness="claude",
        provider="zai",
        model="glm-5.3[1m]",
        model_basis="requested",
        effort="high",
        requested_model="glm-5.3[1m]",
        requested_provider="zai",
        requested_effort="high",
        cwd="/tmp",
        log_path="/tmp/requested-axis.log",
    )
    write_registry([entry], path=registry_path)

    raw = read_registry_document(registry_path)[0]["agents"][0]
    assert raw["requested_model"] == "glm-5.3[1m]"
    assert raw["requested_provider"] == "zai"
    assert raw["requested_effort"] == "high"

    loaded = load_registry(path=registry_path)[0]
    assert loaded.requested_model == "glm-5.3[1m]"
    assert loaded.requested_provider == "zai"
    assert loaded.requested_effort == "high"


def test_v24_requested_axis_defaults_to_none_and_absence_reads_none(
    tmp_path: Path, monkeypatch
) -> None:
    """Unset on a new entry; a pre-v24 row without the keys reads None, never a guess."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    entry = AgentEntry(
        name="no-request",
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/no-request.log",
    )
    assert entry.requested_model is None
    assert entry.requested_provider is None
    assert entry.requested_effort is None
    write_registry([entry], path=registry_path)
    loaded = load_registry(path=registry_path)[0]
    assert loaded.requested_model is None
    assert loaded.requested_provider is None
    assert loaded.requested_effort is None


def test_v24_pre_v24_row_without_requested_keys_reads_none(
    tmp_path: Path, monkeypatch
) -> None:
    """A legacy row hand-shaped without the new keys still loads, axis None."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(
        json.dumps(
            {
                "schema_version": 23,
                "agents": [
                    {
                        "name": "legacy-row",
                        "harness": "claude",
                        "provider": "zai",
                        "model": "glm-5.3[1m]",
                        "model_basis": "requested",
                        "cwd": "/tmp",
                        "log_path": "/tmp/legacy.log",
                        "origin": "spawn",
                    }
                ],
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    loaded = load_registry(path=registry_path)[0]
    assert loaded.requested_model is None
    assert loaded.requested_provider is None
    assert loaded.requested_effort is None
# x-a879: removal accounting at the write choke point
# ---------------------------------------------------------------------------


def _seed_rows(registry_path: Path, rows: list) -> None:
    from fno.agents.registry import update_registry

    def seed(entries):
        entries.extend(rows)
        return entries

    update_registry(seed, path=registry_path)


def _removal_events(events_path: Path) -> list[dict]:
    from tests._event_rows import event_rows

    return [e for e in event_rows(events_path) if e["type"] == "registry_row_removed"]


def test_update_registry_accounts_for_a_removed_row(
    tmp_path: Path, monkeypatch
) -> None:
    """A filtering updater leaves one event and one receipt, receipt first.

    The event rides the canonical writer (``source: "agents"``) onto the
    global stream beside the daemon's events, and the receipt lands in the
    same ``reap-receipts/`` directory the Rust and watchdog writers use.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    events_path = tmp_path / ".fno" / "agents" / "events.jsonl"
    _seed_rows(
        registry_path,
        [
            AgentEntry(
                name="kept-a",
                harness="claude",
                harness_session_id="a-s",
                cwd="/tmp",
                log_path="/tmp/a.log",
            ),
            AgentEntry(
                name="dropped",
                harness="claude",
                harness_session_id="dropped-s",
                cwd="/tmp",
                log_path="/tmp/d.log",
            ),
        ],
    )

    calls: list[dict] = []

    def fake_spawn_axes_call(payload: dict) -> dict:
        calls.append(payload)
        return {"removed": 1}

    monkeypatch.setattr(
        "fno.agents.spawn_axes_client.spawn_axes_call", fake_spawn_axes_call
    )
    update_registry(
        lambda es: [e for e in es if e.name != "dropped"], path=registry_path
    )

    assert len(calls) == 1, "one accounting ask per write"
    ask = calls[0]["removal_accounting"]
    assert ask["registry"] == str(registry_path)
    dropped = [row for row in ask["before"] if row["name"] == "dropped"]
    assert len(dropped) == 1
    assert dropped[0]["harness_session_id"] == "dropped-s", (
        "the pre-write snapshot rides the payload whole"
    )


def test_update_registry_emits_nothing_when_nothing_is_removed(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    events_path = tmp_path / ".fno" / "agents" / "events.jsonl"
    _seed_rows(
        registry_path,
        [
            AgentEntry(
                name="solo", harness="claude", cwd="/tmp", log_path="/tmp/s.log"
            )
        ],
    )

    def add_only(entries):
        entries.append(
            AgentEntry(
                name="added", harness="codex", cwd="/tmp", log_path="/tmp/n.log"
            )
        )
        return entries

    update_registry(add_only, path=registry_path)

    assert not events_path.exists(), "a removal-free write never opens the stream"


def test_update_registry_journals_rows_lost_naming_the_writer(
    tmp_path: Path, monkeypatch
) -> None:
    """x-f0d2: a lossy save journals one grouped registry_rows_lost event.

    The per-row registry_row_removed events carry the remover; the grouped
    event is the writer-naming instrument the 09-03 investigation lacked:
    writer (python), pid, the verb that ran, and every lost id with its name.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    events_path = tmp_path / ".fno" / "agents" / "events.jsonl"
    _seed_rows(
        registry_path,
        [
            AgentEntry(
                name="kept", harness="claude", harness_session_id="kept-s",
                cwd="/tmp", log_path="/tmp/k.log",
            ),
            AgentEntry(
                name="dropped", harness="claude", harness_session_id="dropped-s",
                cwd="/tmp", log_path="/tmp/d.log",
            ),
        ],
    )

    calls: list[dict] = []

    def fake_spawn_axes_call(payload: dict) -> dict:
        calls.append(payload)
        return {"removed": 1}

    monkeypatch.setattr(
        "fno.agents.spawn_axes_client.spawn_axes_call", fake_spawn_axes_call
    )
    update_registry(
        lambda es: [e for e in es if e.name != "dropped"], path=registry_path
    )

    # The grouped-loss contract moved Rust-side with the accounting; the
    # Python door pins its half: the ask carries the whole pre-write
    # snapshot, kept and dropped rows alike, in store order.
    ask = calls[0]["removal_accounting"]
    assert [row["name"] for row in ask["before"]] == ["kept", "dropped"]


def test_update_registry_announces_a_removal_through_the_bridge(
    tmp_path: Path, monkeypatch
) -> None:
    """update_registry hands the before-rows to the Rust choke point's
    removal-accounting op; the write succeeds either way."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    _seed_rows(
        registry_path,
        [
            AgentEntry(
                name="kept", harness="claude", cwd="/tmp", log_path="/tmp/k.log"
            ),
            # No session identity: no resume record is renderable.
            AgentEntry(
                name="identity-less",
                harness="claude",
                cwd="/tmp",
                log_path="/tmp/i.log",
            ),
        ],
    )
    calls: list[dict] = []

    def fake_spawn_axes_call(payload: dict) -> dict:
        calls.append(payload)
        return {"removed": 1}

    monkeypatch.setattr(
        "fno.agents.spawn_axes_client.spawn_axes_call", fake_spawn_axes_call
    )

    update_registry(
        lambda es: [e for e in es if e.name != "identity-less"],
        path=registry_path,
    )

    survivors = load_registry(path=registry_path)
    assert [e.name for e in survivors] == ["kept"]
    assert len(calls) == 1
    ask = calls[0]["removal_accounting"]
    assert ask["registry"] == str(registry_path)
    assert [row["name"] for row in ask["before"]] == ["kept", "identity-less"]


def test_write_registry_has_exactly_one_production_caller() -> None:
    """The low-level write stays a primitive of update_registry alone (x-a879).

    ``write_registry`` gains no removal accounting; its only legitimate
    production caller is ``update_registry`` itself. A future direct caller
    is a new silent door, so it must land as a failing count here first.
    """
    import fno.agents.registry as reg_module

    fno_pkg = Path(reg_module.__file__).parents[1]
    callers = []
    for py in sorted(fno_pkg.rglob("*.py")):
        if "test" in py.name:
            continue
        for lineno, line in enumerate(
            py.read_text(encoding="utf-8").splitlines(), 1
        ):
            stripped = line.strip()
            if "write_registry(" in line and not stripped.startswith(
                "def write_registry"
            ):
                callers.append(f"{py.relative_to(fno_pkg)}:{lineno}")

    assert len(callers) == 1, f"write_registry grew a second caller: {callers}"
    assert callers[0].startswith("agents/registry.py:")


def test_update_registry_keeps_a_receipt_the_sweep_already_staged(
    tmp_path: Path, monkeypatch
) -> None:
    """The sweep or watchdog staged the receipt before the rows were dropped.

    Rewriting it would re-sign a record another door already made. The
    assertion is byte-identity: the file must not change at all.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    events_path = tmp_path / ".fno" / "agents" / "events.jsonl"
    _seed_rows(
        registry_path,
        [
            AgentEntry(
                name="swept",
                harness="claude",
                harness_session_id="swept-s",
                cwd="/tmp",
                log_path="/tmp/s.log",
            )
        ],
    )
    receipt_path = (
        tmp_path / ".fno" / "agents" / "reap-receipts" / "claude-swept-s.json"
    )
    receipt_path.parent.mkdir(parents=True, exist_ok=True)
    receipt_path.write_text(
        json.dumps({"row_name": "swept", "resume": "claude --resume swept-s"}),
        encoding="utf-8",
    )
    before = receipt_path.read_bytes()

    calls: list[dict] = []

    def fake_spawn_axes_call(payload: dict) -> dict:
        calls.append(payload)
        return {"removed": 1}

    monkeypatch.setattr(
        "fno.agents.spawn_axes_client.spawn_axes_call", fake_spawn_axes_call
    )
    update_registry(
        lambda es: [e for e in es if e.name != "swept"], path=registry_path
    )

    assert (
        receipt_path.read_bytes() == before
    ), "the sweep's receipt was rewritten by the choke point"
    assert len(calls) == 1, "the choke point was still consulted"


def test_update_registry_reports_a_stale_binary_and_writes_nothing(
    tmp_path: Path, monkeypatch
) -> None:
    """An unavailable bridge skips the audit, never the write: the survivors
    persist and the write returns them."""
    use_tmpdir(monkeypatch, tmp_path)

    import fno.agents.spawn_axes_client as spawn_axes_client_module

    def _unavailable(payload):
        raise RuntimeError("spawn-axes answered no removal_accounting field")

    monkeypatch.setattr(spawn_axes_client_module, "spawn_axes_call", _unavailable)
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    events_path = tmp_path / ".fno" / "agents" / "events.jsonl"
    _seed_rows(
        registry_path,
        [
            AgentEntry(
                name="kept", harness="claude", harness_session_id="kept-s",
                cwd="/tmp", log_path="/tmp/k.log",
            ),
            AgentEntry(
                name="dropped", harness="claude", harness_session_id="dropped-s",
                cwd="/tmp", log_path="/tmp/d.log",
            ),
        ],
    )

    survivors = update_registry(
        lambda es: [e for e in es if e.name != "dropped"], path=registry_path
    )

    assert [e.name for e in survivors] == ["kept"]
    assert [e.name for e in load_registry(path=registry_path)] == ["kept"]



def test_a_failed_write_announces_nothing(tmp_path: Path, monkeypatch) -> None:
    """When the registry write fails, the row stayed: no removal is announced."""
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.registry as reg_module
    from fno.agents.registry import AgentEntry, update_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    events_path = tmp_path / ".fno" / "agents" / "events.jsonl"
    _seed_rows(
        registry_path,
        [
            AgentEntry(
                name="stays",
                harness="claude",
                harness_session_id="stays-s",
                cwd="/tmp",
                log_path="/tmp/st.log",
            )
        ],
    )

    def _explode(*args, **kwargs):
        raise OSError("simulated disk full during rename")

    monkeypatch.setattr(reg_module, "write_registry", _explode)
    try:
        update_registry(
            lambda es: [e for e in es if e.name != "stays"], path=registry_path
        )
        raise AssertionError("the simulated write failure must propagate")
    except OSError:
        pass

    assert not events_path.exists(), "an unpersisted removal must not be announced"


def _write_rows(registry_path: Path, rows: list[dict]) -> None:
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    from fno.agents.registry import SCHEMA_VERSION

    registry_path.write_text(
        json.dumps({"schema_version": SCHEMA_VERSION, "agents": rows}),
        encoding="utf-8",
    )


def test_v32_lineage_kind_round_trip(tmp_path: Path, monkeypatch) -> None:
    """A v33 registry whose rows carry lineage_kind reads back and the
    field survives a load/write round trip."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    entry = AgentEntry(
        name="sob-t-x-1-glm",
        harness="zai",
        cwd="/tmp",
        log_path="/tmp/sob.log",
        lineage_kind="peer",
    )

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry([entry], path=registry_path)

    loaded = load_registry(path=registry_path)
    assert loaded[0].lineage_kind == "peer"

    write_registry(loaded, path=registry_path)
    reloaded = load_registry(path=registry_path)
    assert reloaded[0].lineage_kind == "peer"


# ---------------------------------------------------------------------------
# v37: session-report stamps (transcript_path / start_source) round-trip
# ---------------------------------------------------------------------------


def test_session_report_fields_round_trip(monkeypatch, tmp_path) -> None:
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    registry_path = tmp_path / ".fno" / "agents" / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="w1",
                harness="claude",
                cwd="/repo",
                log_path="",
                harness_session_id="uuid-1",
                transcript_path="/t/w1.jsonl",
                start_source="resume",
            )
        ],
        path=registry_path,
    )
    raw = read_registry_document(registry_path)[0]
    assert raw["agents"][0]["transcript_path"] == "/t/w1.jsonl"
    assert raw["agents"][0]["start_source"] == "resume"
    row = load_registry(path=registry_path)[0]
    assert row.transcript_path == "/t/w1.jsonl"
    assert row.start_source == "resume"

    write_registry([row], path=registry_path)
    reloaded = load_registry(path=registry_path)[0]
    assert reloaded.transcript_path == "/t/w1.jsonl"
    assert reloaded.start_source == "resume"


def test_guard_refuses_probe_and_mass_drop_on_shared_root(
    tmp_path: Path, monkeypatch
) -> None:
    """A probe (a process under a test marker) writing the real shared
    registry is refused, and so is a write that drops most live rows without
    the override; the refusal leaves the file intact."""
    import tempfile

    from fno.agents.registry import (
        AgentEntry,
        RegistryWriteRefused,
        load_registry,
        write_registry,
    )

    def probe_row(name: str) -> AgentEntry:
        return AgentEntry(
            name=name,
            harness="claude",
            cwd="/repo",
            log_path=f"/tmp/{name}.log",
        )

    # Seed with the guard standing down: the shared root sits under the temp
    # dir view, which is the sandbox shape the carve-out exists for. Armed
    # through the resolver's own first branch (FNO_AGENTS_HOME) so the
    # guard's comparison and the seeded path cannot drift.
    agents_home = tmp_path / "home" / ".fno" / "agents"
    monkeypatch.setenv("FNO_AGENTS_HOME", str(agents_home))
    shared = agents_home / "registry.json"
    write_registry([probe_row(f"worker-{i}") for i in range(5)], path=shared)

    # Arm the guard: present a "real" home OUTSIDE the temp dir view.
    monkeypatch.setattr(tempfile, "gettempdir", lambda: str(tmp_path / "not-temp"))

    with pytest.raises(RegistryWriteRefused, match="test or probe process"):
        write_registry([probe_row("fixture-probe")], path=shared)
    assert len(load_registry(path=shared)) == 5

    monkeypatch.delenv("PYTEST_CURRENT_TEST", raising=False)
    monkeypatch.delenv("FNO_TEST_HERMETIC", raising=False)
    with pytest.raises(RegistryWriteRefused, match="drops live registry rows from 5 to 1"):
        write_registry([probe_row("fixture-probe")], path=shared)
    assert len(load_registry(path=shared)) == 5

    monkeypatch.setenv("FNO_REGISTRY_ALLOW_ROW_LOSS", "1")
    write_registry([probe_row("fixture-probe")], path=shared)
    assert len(load_registry(path=shared)) == 1
