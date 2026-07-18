"""Tests for the with_agent_lock_and_entry context manager (US4-gemini Wave 1.2).

The helper encapsulates the pre-flock validation + ``hold_agent_lock`` +
post-flock re-read pattern that stop_agent / rm_agent used to open-code.
Tests assert:

- AC2-HP: the yielded entry is the post-lock re-read (not the pre-flock
  snapshot).
- AC2-ERR: if the entry disappears between pre-flock validation and
  lock acquisition, the post-lock re-read raises and the lock is
  released as the context manager unwinds.
- AC2-EDGE: the context manager composes with ``contextlib.ExitStack``
  for future cross-agent verbs.
- AC2-FR: the staged_resolve test pattern proves the post-lock entry
  reaches the call site (not the stale pre-flock snapshot).

Tests live in their own module so the lint-flock-pattern.sh enforcement
target is auditable in isolation.
"""
from __future__ import annotations

import contextlib
import os
from pathlib import Path
from typing import Iterator

import pytest

from fno.agents.dispatch import (
    DispatchAskError,
    with_agent_lock_and_entry,
)
from fno.agents.lock import AgentLockTimeout, hold_agent_lock
from fno.agents.registry import AgentEntry, update_registry


def _seed_entry(name: str, provider: str = "claude") -> AgentEntry:
    """Insert a minimal AgentEntry into the registry for the test."""
    entry = AgentEntry(
        name=name,
        harness=provider,
        cwd=str(Path.cwd()),
        log_path=str(Path.cwd() / f"{name}.log"),
        short_id="aaaaaaaa" if provider == "claude" else "",
        harness_session_id=(
            "deadbeef-dead-beef-dead-beefdeadbeef"
            if provider == "codex" else None
        ),
        created_at="2026-05-21T00:00:00Z",
        last_message_at="2026-05-21T00:00:00Z",
        status="live",
    )
    update_registry(lambda entries: entries + [entry])
    return entry


@pytest.fixture
def isolated_registry(tmp_path: Path, monkeypatch) -> Path:
    """Point the agents registry at a clean tmp_path for each test."""
    from fno import paths
    registry_path = tmp_path / "registry.jsonl"
    monkeypatch.setattr(paths, "agents_registry_path", lambda: registry_path)
    return registry_path


def test_yields_post_lock_entry(isolated_registry: Path) -> None:
    """AC2-HP: the yielded entry is the post-flock re-read.

    Seed an entry, enter the helper, and assert the destructured
    ``existing`` is a fresh load (we can prove this by comparing object
    identity against an externally-loaded entry — they must be distinct
    instances even though equal by value).
    """
    seeded = _seed_entry("worker-A")
    with with_agent_lock_and_entry("worker-A") as (lock_handle, existing):
        assert lock_handle.is_held() is True
        assert existing.name == "worker-A"
        assert existing.harness == "claude"
        # The helper does NOT yield the pre-flock snapshot — proven by the
        # fact that the seeded object's identity is not shared. Both
        # values are fresh AgentEntry instances loaded from the registry.
        assert existing is not seeded
    assert lock_handle.is_held() is False


def test_raises_when_agent_missing_pre_flock(isolated_registry: Path) -> None:
    """AC2-ERR (pre-flock leg): missing name raises before lock acquisition."""
    with pytest.raises(DispatchAskError) as exc_info:
        with with_agent_lock_and_entry("ghost"):
            pytest.fail("body must not execute when pre-flock validation fails")
    assert exc_info.value.exit_code == 2


def test_lock_released_on_in_body_exception(isolated_registry: Path) -> None:
    """The lock is released even when the with-body raises an exception.

    Sanity check for the context-manager semantics: any subsequent call
    to ``with with_agent_lock_and_entry(name)`` must succeed promptly
    (within the default timeout) after an in-body exception.
    """
    _seed_entry("worker-A")
    with pytest.raises(RuntimeError, match="boom"):
        with with_agent_lock_and_entry("worker-A") as (_, _):
            raise RuntimeError("boom")
    # Second acquire must succeed quickly; if the lock leaked, this
    # would block for the full timeout window.
    with with_agent_lock_and_entry("worker-A", timeout=2.0) as (_, existing):
        assert existing.name == "worker-A"


def test_staged_resolve_pattern_proves_post_lock_value_is_used(
    isolated_registry: Path, monkeypatch
) -> None:
    """AC2-FR: monkeypatch ``_resolve_registry_entry`` to return a stale
    snapshot on the FIRST call and the real entry on subsequent calls.

    The contract is: pre-flock read uses the stale value (acceptable
    because we discard it), and the post-lock read uses the real entry.
    The yielded ``existing`` MUST be the real entry, not the stale one.
    """
    real_entry = _seed_entry("worker-A", provider="claude")
    stale_entry = AgentEntry(
        name="worker-A",
        harness="claude",
        cwd=str(Path.cwd()),
        log_path=str(Path.cwd() / "worker-A.log"),
        short_id="00000000",  # stale short-id
        created_at="2026-01-01T00:00:00Z",
        last_message_at="2026-01-01T00:00:00Z",
        status="orphaned",
    )

    from fno.agents import dispatch as dispatch_mod

    real_resolve = dispatch_mod._resolve_registry_entry
    call_count = {"n": 0}

    def staged_resolve(name: str, **kwargs) -> AgentEntry:
        # kwargs absorbs registry_path forwarding from the helper.
        call_count["n"] += 1
        if call_count["n"] == 1:
            return stale_entry  # pre-flock: discarded
        return real_resolve(name, **kwargs)  # post-flock: yielded

    monkeypatch.setattr(dispatch_mod, "_resolve_registry_entry", staged_resolve)

    with with_agent_lock_and_entry("worker-A") as (_, existing):
        # AC2-FR: the yielded entry MUST be the real one, not the stale
        # snapshot. If the helper accidentally yielded the pre-flock
        # value, this assertion would fail because short_id would
        # be "00000000" and status would be "orphaned".
        assert existing.short_id == "aaaaaaaa"
        assert existing.status == "live"
    assert call_count["n"] == 2, "helper must read twice (pre + post lock)"


def test_composes_with_exit_stack_for_two_agents(
    isolated_registry: Path,
) -> None:
    """AC2-EDGE: the tuple shape lets us lock two agents via ExitStack.

    A future verb (e.g. cross-agent transfer) needs to lock two agents in
    one scope. The 2-tuple yield form composes naturally with
    ``contextlib.ExitStack.enter_context`` without a custom wrapper.
    """
    _seed_entry("worker-A")
    _seed_entry("worker-B")

    with contextlib.ExitStack() as stack:
        a_lock, a_entry = stack.enter_context(
            with_agent_lock_and_entry("worker-A")
        )
        b_lock, b_entry = stack.enter_context(
            with_agent_lock_and_entry("worker-B")
        )
        assert a_entry.name == "worker-A"
        assert b_entry.name == "worker-B"
        # Each agent has its own per-name lock; the ExitStack composition
        # does not deadlock for distinct names. (Same name would self-
        # deadlock; implementer's responsibility to avoid cycles.)


def test_registry_path_override_routes_both_lock_and_entry_reads(
    tmp_path: Path, monkeypatch
) -> None:
    """Codex P2 on PR #317: a ``registry_path`` override MUST forward
    to both the lock acquisition AND the entry-resolution reads. Pre-
    fix the helper passed the override to ``hold_agent_lock`` but the
    inner ``_resolve_registry_entry`` calls fell through to the default
    registry — a caller using a non-default path would lock against
    one file and validate/return entries from another.
    """
    from fno import paths
    from fno.agents.registry import AgentEntry, update_registry

    default_registry = tmp_path / "default-registry.jsonl"
    override_registry = tmp_path / "override-registry.jsonl"

    # Point the global default at default_registry; seed it with an
    # agent whose name we WILL look up via override. Pre-fix the
    # helper would (incorrectly) succeed by reading this entry from
    # default_registry instead of failing as it should — because the
    # override file doesn't have a worker-A entry.
    monkeypatch.setattr(paths, "agents_registry_path", lambda: default_registry)
    update_registry(lambda entries: entries + [
        AgentEntry(
            name="worker-A",
            harness="claude",
            cwd=str(tmp_path),
            log_path=str(tmp_path / "default-w.log"),
            short_id="ddddffff",  # default-registry short_id
            status="live",
        ),
    ])

    # Seed the override registry with the SAME name but a DIFFERENT
    # short_id so any leak from default_registry into the helper's
    # post-lock read shows up as the wrong short_id.
    update_registry(
        lambda entries: entries + [
            AgentEntry(
                name="worker-A",
                harness="claude",
                cwd=str(tmp_path),
                log_path=str(tmp_path / "override-w.log"),
                short_id="0fffff11",  # override-registry short_id
                status="live",
            ),
        ],
        path=override_registry,
    )

    # Now run the helper with the override. The yielded entry's
    # short_id MUST be the override one — if it's "ddddffff" (the
    # default-registry value), the bug is back.
    with with_agent_lock_and_entry(
        "worker-A", registry_path=override_registry
    ) as (_lock, existing):
        assert existing.short_id == "0fffff11", (
            "registry_path override was not honored by the "
            "_resolve_registry_entry calls; helper read entries from "
            f"the default registry instead: {existing!r}"
        )


def test_re_read_under_lock_raises_when_entry_deleted_mid_block(
    isolated_registry: Path, monkeypatch
) -> None:
    """Hard case: pre-flock read succeeds, lock is acquired, but the
    POST-LOCK re-read finds the entry has been deleted (another process
    raced through after we released the registry-read lock but before we
    acquired the per-agent flock).

    The helper raises DispatchAskError(exit_code=2) inside the with-block
    and the lock is released as the context manager unwinds.
    """
    _seed_entry("worker-A")

    from fno.agents import dispatch as dispatch_mod

    real_resolve = dispatch_mod._resolve_registry_entry
    call_count = {"n": 0}

    def disappearing(name: str, **kwargs) -> AgentEntry:
        # kwargs absorbs registry_path forwarding from the helper.
        call_count["n"] += 1
        if call_count["n"] == 1:
            return real_resolve(name, **kwargs)  # pre-flock: entry exists
        # Post-flock: entry has "vanished". Raise the same shape
        # _resolve_registry_entry would have raised.
        raise DispatchAskError(
            f"agent {name!r} not found in registry",
            exit_code=2,
        )

    monkeypatch.setattr(
        dispatch_mod, "_resolve_registry_entry", disappearing
    )

    with pytest.raises(DispatchAskError) as exc_info:
        with with_agent_lock_and_entry("worker-A"):
            pytest.fail("yield must not occur when post-lock read fails")
    assert exc_info.value.exit_code == 2

    # After the exception, the lock must be released — a fresh acquire
    # (with the real _resolve_registry_entry restored) succeeds without
    # blocking.
    monkeypatch.setattr(
        dispatch_mod, "_resolve_registry_entry", real_resolve
    )
    with with_agent_lock_and_entry("worker-A", timeout=2.0) as (_, existing):
        assert existing.name == "worker-A"
