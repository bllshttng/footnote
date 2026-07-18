"""Tests for the US4-gemini lifecycle-polish handoff items.

Three independent fixes bundled into one PR (handoff backlog node
``ab-f1ab5a29``):

1. ``stop_agent`` flips registry status to ``"orphaned"`` on successful
   ``claude stop`` so the Phase 8 TUI doesn't keep the row coloured
   ``live`` until the next reconcile.
2. ``_stamp_status`` accepts a ``Callable[[], str]`` for
   ``last_message_at`` so the timestamp is generated INSIDE the
   ``update_registry`` lock — making concurrent followups strictly
   monotonic instead of last-lock-winner-wins on a possibly-earlier
   pre-lock timestamp.
3. ``reconcile_agents`` preserves the current entry's ``last_message_at``
   when its batched ``_apply`` writes a status flip. Pre-fix, the
   pending-updates dict captured the WHOLE snapshot entry so a
   dispatch_ask interleaving between the probe loop and the atomic
   apply would have its ``last_message_at`` bump silently dropped.

Tests are intentionally small and direct; each closes one item from
``internal/fno/handoffs/fno-agents-us4-gemini-handoff.md``.
"""
from __future__ import annotations

import threading
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _seed_registry(*entries):
    """Persist the given AgentEntry-kwargs dicts as the active registry."""
    from fno.agents.registry import AgentEntry, write_registry

    out: list[AgentEntry] = []
    for kwargs in entries:
        kwargs.setdefault("cwd", "/tmp")
        kwargs.setdefault("log_path", "/tmp/x.log")
        # v10 (x-880e): map legacy identity kwargs to the canonical fields.
        if "provider" in kwargs:
            kwargs["harness"] = kwargs.pop("provider")
        for _k in ("codex_session_id", "gemini_session_id", "claude_session_uuid"):
            if _k in kwargs:
                kwargs.setdefault("harness_session_id", kwargs.pop(_k))
        out.append(AgentEntry(**kwargs))
    write_registry(out)
    return out


def _force_claude_on_path(monkeypatch, tmp_path: Path) -> None:
    """Make ``is_provider_available('claude')`` return True without a real binary."""
    from fno.agents import dispatch as dispatch_mod

    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir(exist_ok=True)
    fake = bin_dir / "claude"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))
    assert dispatch_mod.is_provider_available("claude") is True


def _load_entry(name: str):
    from fno.agents.registry import load_registry

    for entry in load_registry():
        if entry.name == name:
            return entry
    raise AssertionError(f"entry {name!r} not found in registry")


# ---------------------------------------------------------------------------
# Fix 1: stop_agent flips status to "orphaned" on successful claude stop
# ---------------------------------------------------------------------------


def test_stop_claude_flips_status_to_orphaned(tmp_path: Path, monkeypatch) -> None:
    """After ``claude stop`` exits 0, the registry row reads ``status=orphaned``.

    Pre-fix the row kept ``status="live"`` until the next reconcile, so
    ``fno agents list`` would lie about reachability for any window
    between the stop and the next reconcile pass. Important for the
    Phase 8 TUI's status colouring. Handoff sigma-review H6.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            status="live",
            last_message_at="2026-05-20T12:00:00Z",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod,
        "claude_stop",
        lambda short_id, *, timeout=30.0: (0, ""),
    )

    dispatch.stop_agent("worker-claude")

    entry = _load_entry("worker-claude")
    assert entry.status == "orphaned"
    # ``last_message_at`` must be preserved across the stop — the stop
    # flag does not invalidate the historical timestamp.
    assert entry.last_message_at == "2026-05-20T12:00:00Z"


def test_stop_claude_nonzero_exit_leaves_status_untouched(
    tmp_path: Path, monkeypatch
) -> None:
    """A failed ``claude stop`` (non-zero exit) does NOT flip status.

    The status flip is the success-path forensic signal; a stop that
    surfaces stderr to the operator (e.g. "session already stopped") must
    not silently rewrite the registry to a state that wasn't earned.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            short_id="7c5dcf5d",
            status="live",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod,
        "claude_stop",
        lambda short_id, *, timeout=30.0: (5, "claude stop: session already stopped\n"),
    )

    with pytest.raises(dispatch.DispatchAskError):
        dispatch.stop_agent("worker-claude")

    entry = _load_entry("worker-claude")
    assert entry.status == "live"


# ---------------------------------------------------------------------------
# Fix 2: _stamp_status accepts Callable[[], str] for last_message_at
# ---------------------------------------------------------------------------


def test_stamp_status_with_callable_invokes_under_lock(
    tmp_path: Path, monkeypatch
) -> None:
    """``last_message_at=callable`` is invoked when ``_updater`` runs.

    The contract: a callable is evaluated inside ``update_registry``'s
    closure rather than at ``_stamp_status`` construction time. This is
    how callers (dispatch_ask paths) defer the timestamp into the lock
    so concurrent stamps stay monotonic per atomic write.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
            last_message_at=None,
        ),
    )

    from fno.agents import dispatch
    from fno.agents.registry import update_registry

    call_count = {"n": 0}

    def factory() -> str:
        call_count["n"] += 1
        return "2026-05-21T19:00:00Z"

    update_registry(
        dispatch._stamp_status(
            "worker-codex",
            status="live",
            last_message_at=factory,
        ),
    )

    assert call_count["n"] == 1  # invoked once, inside the lock
    entry = _load_entry("worker-codex")
    assert entry.last_message_at == "2026-05-21T19:00:00Z"


def test_stamp_status_callable_monotonic_under_serialized_writes(
    tmp_path: Path, monkeypatch
) -> None:
    """Two serialized _stamp_status calls produce a strictly later timestamp.

    Uses an injected clock so the test is deterministic. Calls _utc_now_iso
    twice with different return values; the second write must persist
    the second value because it was generated INSIDE the second lock
    acquisition.

    Pre-fix this property held only by accident — if both call sites
    evaluated ``_utc_now_iso()`` before lock acquisition (the old
    pattern), the lock-loser could have an earlier timestamp than the
    lock-winner; the lock-winner's atomic write would persist the
    earlier value, regressing the timeline.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
            last_message_at=None,
        ),
    )

    from fno.agents import dispatch
    from fno.agents.registry import update_registry

    # First write: T1
    update_registry(
        dispatch._stamp_status(
            "worker-codex",
            status="live",
            last_message_at=lambda: "2026-05-21T18:00:00Z",
        ),
    )

    # Second write: T2 > T1
    update_registry(
        dispatch._stamp_status(
            "worker-codex",
            status="live",
            last_message_at=lambda: "2026-05-21T19:00:00Z",
        ),
    )

    entry = _load_entry("worker-codex")
    assert entry.last_message_at == "2026-05-21T19:00:00Z"


# ---------------------------------------------------------------------------
# Fix 3: reconcile_agents preserves last_message_at on concurrent update
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "provider,session_field,session_id",
    [
        (
            "codex",
            "codex_session_id",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ),
        (
            "gemini",
            "gemini_session_id",
            "11111111-2222-3333-4444-555555555555",
        ),
    ],
)
def test_concurrent_reconcile_and_ask_preserves_both_fields(
    tmp_path: Path,
    monkeypatch,
    provider: str,
    session_field: str,
    session_id: str,
) -> None:
    """Reconcile + ask race: both ``status`` and ``last_message_at`` land.

    Setup:
      1. Seed a registry entry with ``status=live, last_message_at=None``.
      2. Monkeypatch the provider's reachability probe so reconcile
         decides to flip ``status`` to ``"orphaned"``. Block the probe
         on a barrier so reconcile pauses with ``pending_updates`` held
         but before the atomic apply.
      3. From a second thread, run an inline ``_stamp_status`` cycle
         simulating dispatch_ask's ``last_message_at`` bump.
      4. Release the barrier; let reconcile's atomic apply finish.

    Pre-fix: reconcile's ``_apply`` builds the new entries from the
    pre-probe snapshot via ``dataclasses.replace(entry, status=...)``
    where ``entry`` is the captured-at-probe snapshot. The mid-flight
    ``last_message_at`` bump is overwritten.

    Post-fix: ``_apply`` uses the current registry entry as the base
    and only overrides ``status`` from the pending-updates dict.
    ``last_message_at`` survives.

    Final state asserted:
      - No ``OSError`` raised by either thread.
      - ``entry.status == "orphaned"`` (reconcile won the status flip).
      - ``entry.last_message_at == "2026-05-21T19:30:00Z"`` (ask
        survived the apply).
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name=f"worker-{provider}",
            provider=provider,
            status="live",
            last_message_at=None,
            **{session_field: session_id},
        ),
    )

    from fno.agents import dispatch
    from fno.agents.registry import update_registry

    probe_started = threading.Event()
    apply_may_proceed = threading.Event()

    if provider == "codex":
        from fno.agents.providers import codex as codex_mod

        # Make the codex session index "ready" so the reconcile loop
        # reaches the per-entry reachability check.
        monkeypatch.setattr(
            codex_mod, "session_index_exists", lambda *, session_index_path=None: True
        )

        def fake_load_known(*, session_index_path=None):
            # Block until the test thread has run the dispatch_ask
            # simulation; report "not known" so reconcile flips to
            # orphaned.
            probe_started.set()
            apply_may_proceed.wait(timeout=5.0)
            return set()

        monkeypatch.setattr(codex_mod, "load_known_session_ids", fake_load_known)

    else:  # gemini
        from fno.agents.providers import gemini as gemini_mod

        def fake_reachable(sid, cwd):
            probe_started.set()
            apply_may_proceed.wait(timeout=5.0)
            return False  # not reachable -> orphaned

        monkeypatch.setattr(gemini_mod, "gemini_session_reachable", fake_reachable)

    errors: list[Exception] = []

    def run_reconcile():
        try:
            dispatch.reconcile_agents()
        except Exception as exc:  # noqa: BLE001
            errors.append(exc)

    reconciler = threading.Thread(target=run_reconcile, daemon=True)
    reconciler.start()

    # Wait for reconcile to be paused INSIDE the probe loop (i.e. it
    # has loaded the registry snapshot, started probing, and is
    # blocked on the barrier).
    assert probe_started.wait(timeout=5.0), "reconcile probe never started"

    # Simulate dispatch_ask bumping last_message_at while reconcile is
    # paused with a stale snapshot. We use _stamp_status directly to
    # mimic what dispatch_ask does post-shellout without spinning up a
    # whole subprocess fixture.
    update_registry(
        dispatch._stamp_status(
            f"worker-{provider}",
            status="live",
            last_message_at=lambda: "2026-05-21T19:30:00Z",
        ),
    )

    # Let reconcile finish.
    apply_may_proceed.set()
    reconciler.join(timeout=5.0)
    assert not reconciler.is_alive(), "reconcile thread did not finish"

    assert errors == [], f"reconcile raised: {errors}"

    entry = _load_entry(f"worker-{provider}")
    # Reconcile flipped status to orphaned.
    assert entry.status == "orphaned", (
        f"expected reconcile to flip status to orphaned, got {entry.status!r}"
    )
    # Dispatch_ask's last_message_at survived reconcile's apply.
    assert entry.last_message_at == "2026-05-21T19:30:00Z", (
        f"dispatch_ask's last_message_at was lost by reconcile's _apply; "
        f"got {entry.last_message_at!r}"
    )
