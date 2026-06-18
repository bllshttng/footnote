"""Tests for fno.agents.dispatch lifecycle verbs.

Wave-1 coverage (US4-lifecycle):

- ``stop_agent`` (AC1-* in the design doc)
- ``rm_agent`` (AC2-*)
- ``reconcile_agents`` (AC3-*)
- ``attach_agent`` (AC7-*)

Each test monkeypatches the corresponding helper in
``fno.agents.providers.{claude,codex}`` so we exercise the
dispatch surface in isolation; the provider adapters have their own
shellout-level tests.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional

import pytest

from fno.paths_testing import use_tmpdir


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


def _seed_registry(*entries):
    """Persist a list of AgentEntry dicts as the active registry."""
    from fno.agents.registry import AgentEntry, write_registry

    out: list[AgentEntry] = []
    for kwargs in entries:
        kwargs.setdefault("cwd", "/tmp")
        kwargs.setdefault("log_path", "/tmp/x.log")
        out.append(AgentEntry(**kwargs))
    write_registry(out)
    return out


def _force_claude_on_path(monkeypatch, tmp_path: Path) -> None:
    """Make ``is_provider_available('claude')`` return True without a real binary.

    We monkeypatch ``shutil.which`` via the ``dispatch`` module so all
    PATH checks see a positive result; the per-call ``claude_*`` helpers
    are independently patched per-test.
    """
    from fno.agents import dispatch as dispatch_mod

    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir(exist_ok=True)
    fake = bin_dir / "claude"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))
    # Defense against test order: the dispatch module imports shutil at
    # import time; PATH env tweak above is sufficient because
    # shutil.which is called on every is_provider_available invocation.
    assert dispatch_mod.is_provider_available("claude") is True


def _read_events(tmp_path: Path) -> list[dict]:
    """Return all events.jsonl records (or empty if file absent).

    Mirrors the helper in test_codex_fatal_error_dispatch.py; copied
    here so each lifecycle test can verify the forensic event-stream
    contract independently of provider failure tests.
    """
    from fno import paths

    events_path = paths.state_dir() / "events.jsonl"
    if not events_path.exists():
        return []
    out: list[dict] = []
    for line in events_path.read_text().splitlines():
        if not line.strip():
            continue
        out.append(json.loads(line))
    return out


# ---------------------------------------------------------------------------
# stop_agent — AC1-*
# ---------------------------------------------------------------------------


def test_stop_claude_happy_path(tmp_path: Path, monkeypatch, capsys) -> None:
    """AC1-HP: claude stop succeeds, emits agent_stopped, prints summary."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    calls: list[tuple[str, float]] = []

    def fake_claude_stop(short_id: str, *, timeout: float = 30.0):
        calls.append((short_id, timeout))
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_stop", fake_claude_stop)

    result = dispatch.stop_agent("worker-claude")

    assert result.name == "worker-claude"
    assert result.provider == "claude"
    assert result.claude_exit == 0
    assert calls == [("7c5dcf5d", 30.0)]

    out = capsys.readouterr().out
    assert "stopped: worker-claude (7c5dcf5d)" in out

    # AC1-HP forensic contract: agent_stopped event carries claude_exit=0,
    # provider=claude, and the short_id for downstream audit.
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0]["provider"] == "claude"
    assert stop_events[0]["claude_exit"] == 0
    assert stop_events[0]["short_id"] == "7c5dcf5d"


def test_stop_claude_nonzero_exit_propagates(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC1-ERR: claude stop non-zero passes stderr through and exit_code=1."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod,
        "claude_stop",
        lambda short_id, *, timeout=30.0: (
            5,
            "claude stop: session already stopped\n",
        ),
    )

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.stop_agent("worker-claude")

    assert exc_info.value.exit_code == 1
    err = capsys.readouterr().err
    assert "session already stopped" in err

    # AC1-ERR forensic contract: agent_stopped event must carry the
    # non-zero claude_exit even when the operator-facing flow raises.
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0]["claude_exit"] == 5


def test_stop_agent_not_found(tmp_path: Path, monkeypatch) -> None:
    """AC1-UI: stop on a non-existent name exits 2 without spawning subprocess."""
    use_tmpdir(monkeypatch, tmp_path)
    # No registry entry seeded.

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    spawn_called = False

    def fake_stop(short_id, *, timeout=30.0):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_stop", fake_stop)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.stop_agent("ghost")

    assert exc_info.value.exit_code == 2
    assert "not found in registry" in str(exc_info.value)
    assert spawn_called is False


def test_stop_codex_is_no_op(tmp_path: Path, monkeypatch, capsys) -> None:
    """AC1-EDGE: codex agents print info on stderr and exit 0; no subprocess."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    spawn_called = False

    def fake_stop(*args, **kwargs):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_stop", fake_stop)

    result = dispatch.stop_agent("worker-codex")

    assert result.provider == "codex"
    assert result.claude_exit is None
    assert spawn_called is False
    err = capsys.readouterr().err
    assert "codex agents are synchronous" in err

    # AC1-EDGE forensic contract: codex stop emits the same event kind so
    # an external observer can count stop activity uniformly.
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0]["provider"] == "codex"
    assert stop_events[0]["claude_exit"] is None


def test_stop_claude_timeout_maps_to_exit_15(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-FR: shellout timeout raises DispatchAskError(exit_code=15)."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    def fake_stop(short_id, *, timeout=30.0):
        raise subprocess.TimeoutExpired(cmd=["claude", "stop", short_id], timeout=timeout)

    monkeypatch.setattr(claude_mod, "claude_stop", fake_stop)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.stop_agent("worker-claude")

    assert exc_info.value.exit_code == 15
    assert "timed out" in str(exc_info.value)

    # AC1-FR forensic contract: timeout path emits with timed_out=true so
    # observers can distinguish "shellout exit non-zero" from "shellout
    # never returned".
    events = _read_events(tmp_path)
    stop_events = [e for e in events if e.get("kind") == "agent_stopped"]
    assert len(stop_events) == 1
    assert stop_events[0].get("timed_out") is True
    assert stop_events[0]["claude_exit"] is None


# ---------------------------------------------------------------------------
# rm_agent — AC2-*
# ---------------------------------------------------------------------------


def test_rm_claude_happy_path(tmp_path: Path, monkeypatch, capsys) -> None:
    """AC2-HP: claude rm exits 0, registry row removed."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (0, ""),
    )

    result = dispatch.rm_agent("worker-claude")

    assert result.registry_changed is True
    assert result.claude_exit == 0
    assert load_registry() == []
    assert "removed: worker-claude" in capsys.readouterr().out

    # AC2-HP forensic contract: agent_removed event with claude_exit=0,
    # force=false, registry_changed=true.
    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0]["claude_exit"] == 0
    assert rm_events[0]["force"] is False
    assert rm_events[0]["registry_changed"] is True


def test_rm_claude_refusal_leaves_registry_unchanged(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC2-ERR: non-forceful claude refusal -> stderr passthrough, registry unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (
            1,
            "session has uncommitted changes; commit or stash first\n",
        ),
    )

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 1
    # Registry preserved.
    entries = load_registry()
    assert len(entries) == 1
    assert entries[0].name == "worker-claude"
    err = capsys.readouterr().err
    assert "uncommitted changes" in err

    # AC2-ERR forensic contract: refusal event with registry_changed=false
    # is what external `fno agents list` vs claude-supervisor diff
    # reconciliation depends on. Drop this emit and the chain breaks
    # silently.
    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0]["claude_exit"] == 1
    assert rm_events[0]["force"] is False
    assert rm_events[0]["registry_changed"] is False


def test_rm_force_overrides_claude_refusal(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC2-UI: --force removes the registry row even when claude rm fails."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (
            1,
            "session has uncommitted changes\n",
        ),
    )

    result = dispatch.rm_agent("worker-claude", force=True)

    assert result.force is True
    assert result.claude_exit == 1
    assert result.registry_changed is True
    assert load_registry() == []
    err = capsys.readouterr().err
    assert "WARN: claude rm failed but --force given" in err

    # AC2-UI: --force override emits with both claude_exit (preserved) and
    # registry_changed=true so post-hoc forensics can see "operator chose
    # to drop the row despite claude's refusal".
    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0]["claude_exit"] == 1
    assert rm_events[0]["force"] is True
    assert rm_events[0]["registry_changed"] is True


def test_rm_codex_is_registry_only(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC2-EDGE: codex rm removes registry row; no subprocess spawned."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    spawn_called = False

    def fake_rm(*args, **kwargs):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    result = dispatch.rm_agent("worker-codex")

    assert spawn_called is False
    assert load_registry() == []
    assert result.registry_changed is True
    assert "removed: worker-codex" in capsys.readouterr().out


def test_rm_claude_not_on_path(tmp_path: Path, monkeypatch) -> None:
    """AC2-FR: claude not on PATH exits 14, registry unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    monkeypatch.setenv("PATH", "/nonexistent")

    from fno.agents import dispatch
    from fno.agents.registry import load_registry

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 14
    entries = load_registry()
    assert len(entries) == 1  # registry untouched


# ---------------------------------------------------------------------------
# reconcile_agents — AC3-*
# ---------------------------------------------------------------------------


def test_reconcile_orphan_detection(tmp_path: Path, monkeypatch) -> None:
    """AC3-HP: live claude agent flips to orphaned when logs probe fails."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            claude_short_id="7c5dcf5d",
            status="live",
            last_message_at="2026-05-01T00:00:00Z",
        ),
    )
    # is_provider_available("claude") must return True so reconcile reaches
    # the claude_logs_reachable monkeypatch instead of routing to errors
    # (sigma-review C1 fix: missing claude -> errors, not orphaned).
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable",
        lambda short_id, *, timeout=10.0: False,
    )

    result = dispatch.reconcile_agents()

    assert result.scanned == 1
    assert len(result.orphaned) == 1
    assert result.orphaned[0]["name"] == "worker-claude"
    # last_message_at preserved.
    entries = load_registry()
    assert entries[0].status == "orphaned"
    assert entries[0].last_message_at == "2026-05-01T00:00:00Z"

    # AC3-HP forensic contract: reconcile_done aggregate with the right counts.
    events = _read_events(tmp_path)
    done = [e for e in events if e.get("kind") == "reconcile_done"]
    assert len(done) == 1
    assert done[0]["scanned"] == 1
    assert done[0]["orphaned"] == 1
    assert done[0]["recovered"] == 0


def test_reconcile_recovery(tmp_path: Path, monkeypatch) -> None:
    """AC3-ERR: orphaned claude agent flips back to live on reachable probe."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            claude_short_id="7c5dcf5d",
            status="orphaned",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        claude_mod, "claude_logs_reachable",
        lambda short_id, *, timeout=10.0: True,
    )

    result = dispatch.reconcile_agents()

    assert len(result.recovered) == 1
    assert load_registry()[0].status == "live"

    # AC3-ERR forensic contract.
    events = _read_events(tmp_path)
    done = [e for e in events if e.get("kind") == "reconcile_done"]
    assert len(done) == 1
    assert done[0]["recovered"] == 1
    assert done[0]["orphaned"] == 0


def test_reconcile_json_shape_round_trips_gemini_error(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-UI: result lists are JSON-friendly. Post-#316 (Wave 3.3),
    gemini agents are PROBED rather than skipped — a malformed session
    id ("some-id" doesn't match the gemini UUID short-prefix layout)
    routes to errors with a gemini-probe-failed reason."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-gemini",
            provider="gemini",
            gemini_session_id="some-id",  # too short for the 8-hex prefix probe
        ),
    )

    from fno.agents import dispatch

    result = dispatch.reconcile_agents()
    assert result.scanned == 1
    # Probe rejects on too-short UUID; reachability inconclusive -> errors.
    assert len(result.errors) == 1
    assert result.errors[0]["provider"] == "gemini"
    assert "gemini-probe-failed" in result.errors[0]["reason"]
    # Round-trips through json without error.
    payload = {
        "scanned": result.scanned,
        "orphaned": result.orphaned,
        "recovered": result.recovered,
        "skipped": result.skipped,
        "errors": result.errors,
    }
    encoded = json.dumps(payload)
    decoded = json.loads(encoded)
    assert decoded["errors"][0]["provider"] == "gemini"


def test_reconcile_codex_index_missing(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC3-EDGE: missing ~/.codex/session_index.jsonl yields an error entry,
    codex statuses untouched, exit 0."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    # Override the index path to a non-existent file.
    missing = tmp_path / "no-codex-index.jsonl"
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: missing
    )

    result = dispatch.reconcile_agents()

    assert len(result.errors) == 1
    assert result.errors[0]["reason"] == "codex-session-index-missing"
    # Status unchanged.
    assert load_registry()[0].status == "live"
    err = capsys.readouterr().err
    assert "codex session index missing" in err


def test_reconcile_codex_reachable_flips_to_live(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-FR companion: codex session listed in the index flips orphaned -> live."""
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id=session_id,
            status="orphaned",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    index = tmp_path / "session_index.jsonl"
    index.write_text(json.dumps({"session_id": session_id}) + "\n")
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: index
    )

    result = dispatch.reconcile_agents()

    assert len(result.recovered) == 1
    assert load_registry()[0].status == "live"


# ---------------------------------------------------------------------------
# attach_agent — AC7-*
# ---------------------------------------------------------------------------


def test_attach_claude_inherits_stdio_and_propagates_exit(
    tmp_path: Path, monkeypatch
) -> None:
    """AC7-HP: claude attach returns claude's exit code."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    calls: list[str] = []

    def fake_attach(short_id: str) -> int:
        calls.append(short_id)
        return 0

    monkeypatch.setattr(claude_mod, "claude_attach", fake_attach)

    result = dispatch.attach_agent("worker-claude")
    assert result.provider == "claude"
    assert result.exit_code == 0
    assert calls == ["7c5dcf5d"]


def test_attach_codex_refused_with_exit_13(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """AC7-ERR: codex attach exits 13 with explanatory stderr; no subprocess."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    called = False

    def fake_attach(short_id):
        nonlocal called
        called = True
        return 0

    monkeypatch.setattr(claude_mod, "claude_attach", fake_attach)

    result = dispatch.attach_agent("worker-codex")
    assert result.exit_code == 13
    assert called is False
    err = capsys.readouterr().err
    assert "one-shot" in err
    assert "Phase 6" in err


def test_attach_agent_not_found(tmp_path: Path, monkeypatch) -> None:
    """AC7-UI: missing agent name exits 2, no subprocess."""
    use_tmpdir(monkeypatch, tmp_path)

    from fno.agents import dispatch

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.attach_agent("ghost")
    assert exc_info.value.exit_code == 2


def test_attach_claude_propagates_nonzero_exit(
    tmp_path: Path, monkeypatch
) -> None:
    """AC7-EDGE: claude attach exit 4 surfaces as result.exit_code=4."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod, "claude_attach", lambda short_id: 4
    )

    result = dispatch.attach_agent("worker-claude")
    assert result.exit_code == 4


# ---------------------------------------------------------------------------
# Sigma-review follow-ups: tests for fixes landed in the review-fixes commit.
# ---------------------------------------------------------------------------


def test_rm_claude_timeout_preserves_registry(
    tmp_path: Path, monkeypatch
) -> None:
    """Sigma #3: rm timeout must NOT mutate the registry.

    Atomicity invariant (Locked Decision 6): claude shellout FIRST,
    registry mutation AFTER. A timeout in the shellout layer must leave
    the registry untouched and emit the agent_removed event with
    timed_out=true + registry_changed=false.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    def fake_rm(short_id, *, timeout=30.0):
        raise subprocess.TimeoutExpired(
            cmd=["claude", "rm", short_id], timeout=timeout
        )

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 15
    entries = load_registry()
    assert len(entries) == 1, "registry must stay intact on timeout"

    events = _read_events(tmp_path)
    rm_events = [e for e in events if e.get("kind") == "agent_removed"]
    assert len(rm_events) == 1
    assert rm_events[0].get("timed_out") is True
    assert rm_events[0]["registry_changed"] is False
    assert rm_events[0]["claude_exit"] is None


def test_rm_print_lands_after_registry_write(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Sigma C3: 'removed:' confirmation must NOT print until update_registry succeeds.

    Forces update_registry to raise OSError; the operator must see the
    claude shellout already happened (event emitted) but must NOT see a
    'removed:' confirmation in stdout — that print is now gated on
    registry write success.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id="7c5dcf5d"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(
        claude_mod, "claude_rm",
        lambda short_id, *, timeout=30.0: (0, ""),
    )

    def boom(*args, **kwargs):
        raise OSError("disk full")

    monkeypatch.setattr(dispatch_mod, "update_registry", boom)

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")

    assert exc_info.value.exit_code == 12
    out = capsys.readouterr().out
    assert "removed: worker-claude" not in out, (
        "stdout must not lie about removal when the registry write fails"
    )


def test_reconcile_skips_claude_when_cli_missing(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Sigma C1: missing claude CLI must NOT mass-orphan every claude agent.

    Mirrors AC3-EDGE for the codex side: when reachability cannot be
    probed, statuses stay untouched and the entries land in `errors`
    with a precise reason.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            claude_short_id="7c5dcf5d",
            status="live",
        ),
        dict(
            name="worker-claude-2",
            provider="claude",
            claude_short_id="abcd1234",
            status="live",
        ),
    )
    # Force claude OFF PATH so is_provider_available returns False.
    monkeypatch.setenv("PATH", "/nonexistent")

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    probe_called = False

    def fake_probe(short_id, *, timeout=10.0):
        nonlocal probe_called
        probe_called = True
        return False

    monkeypatch.setattr(claude_mod, "claude_logs_reachable", fake_probe)

    result = dispatch.reconcile_agents()

    assert probe_called is False, (
        "claude_logs_reachable must NOT be called when claude is not on PATH"
    )
    assert len(result.errors) == 2
    assert all(
        e["reason"] == "claude-cli-not-on-path" for e in result.errors
    )
    # Statuses must stay 'live' — false mass-orphaning would be the C1 bug.
    for entry in load_registry():
        assert entry.status == "live"
    err = capsys.readouterr().err
    assert "claude CLI not on PATH" in err


def test_rm_force_removes_orphan_row_without_short_id(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1: rm --force on a corrupted claude row (no short_id) must
    drop the registry entry instead of refusing.

    The pre-fix code raised DispatchAskError(exit_code=12) before checking
    --force. Help text told the operator to retry with --force, but --force
    was never honored — the row stayed forever.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id=None),
    )

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import load_registry

    spawn_called = False

    def fake_rm(*args, **kwargs):
        nonlocal spawn_called
        spawn_called = True
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    result = dispatch.rm_agent("worker-claude", force=True)

    assert result.registry_changed is True
    assert result.force is True
    assert result.claude_exit is None
    assert load_registry() == []
    # No subprocess fired - we can't shell out without a short_id.
    assert spawn_called is False
    err = capsys.readouterr().err
    assert "registry entry has no claude_short_id" in err


def test_rm_without_force_on_orphan_row_still_refuses(
    tmp_path: Path, monkeypatch
) -> None:
    """The non-force path on a corrupted row keeps the legacy refusal so
    operators see the diagnostic and choose to add --force explicitly."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-claude", provider="claude", claude_short_id=None),
    )

    from fno.agents import dispatch
    from fno.agents.registry import load_registry

    with pytest.raises(dispatch.DispatchAskError) as exc_info:
        dispatch.rm_agent("worker-claude")  # default force=False

    assert exc_info.value.exit_code == 12
    assert "no claude_short_id" in str(exc_info.value)
    # Registry untouched.
    assert len(load_registry()) == 1


def test_rm_uses_locked_short_id_after_concurrent_recreate(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1 round-3: rm must re-resolve the registry entry UNDER the lock.

    Scenario: Process A enters rm_agent, resolves the pre-flock entry
    (short_id=A). Before A acquires the flock, Process B removes and
    recreates the same agent name with short_id=B. When A acquires the
    flock, it MUST shell out `claude rm B` (current truth), NOT
    `claude rm A` (stale pre-flock snapshot).

    Simulated via monkeypatching _resolve_registry_entry to return
    different entries on its two call-sites (pre-flock vs locked).
    """
    use_tmpdir(monkeypatch, tmp_path)
    # Seed with the LATER (post-recreate) short_id so the locked re-resolve
    # picks it up via the real load_registry path.
    _seed_registry(
        dict(name="racy", provider="claude", claude_short_id="bbbbbbbb"),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.registry import AgentEntry

    # Patch _resolve_registry_entry to return a STALE entry on the first
    # call (pre-flock fast-fail) and fall through to the real resolver
    # for subsequent calls (the locked re-resolve will read the seeded
    # registry).
    real_resolve = dispatch._resolve_registry_entry
    call_count = {"n": 0}

    def staged_resolve(name: str, **kwargs):
        # kwargs absorbs registry_path forwarding from
        # with_agent_lock_and_entry (Codex P2 on PR #317).
        call_count["n"] += 1
        if call_count["n"] == 1:
            # Stale entry — pretends short_id was aaaaaaaa pre-flock.
            return AgentEntry(
                name=name, provider="claude", cwd="/tmp", log_path="/tmp/x",
                claude_short_id="aaaaaaaa",
            )
        return real_resolve(name, **kwargs)

    monkeypatch.setattr(dispatch, "_resolve_registry_entry", staged_resolve)

    received: list[str] = []

    def fake_rm(short_id, *, timeout=30.0):
        received.append(short_id)
        return (0, "")

    monkeypatch.setattr(claude_mod, "claude_rm", fake_rm)

    dispatch.rm_agent("racy")

    # The shellout must target the locked-resolve short_id, NOT the stale
    # pre-flock one. If the bug regressed, received would equal ["aaaaaaaa"].
    assert received == ["bbbbbbbb"], (
        f"rm_agent shelled out with stale short_id: {received!r}"
    )
    assert call_count["n"] >= 2, "expected at least 2 _resolve calls (pre-flock + locked)"


def test_reconcile_preserves_claude_status_on_probe_error(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1 round-5: transient claude probe failures (timeout / OSError)
    must NOT flip healthy agents to orphaned.

    Pre-fix, claude_logs_reachable returned False on timeout / OSError,
    and reconcile interpreted False as "supervisor lost session" →
    orphaned flip. A single slow probe could mass-orphan the fleet.

    Fix: claude_logs_reachable raises ReachabilityProbeError on
    inconclusive outcomes; reconcile catches and routes to errors
    with reason=claude-probe-failed, preserving status.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-claude",
            provider="claude",
            claude_short_id="7c5dcf5d",
            status="live",
        ),
    )
    _force_claude_on_path(monkeypatch, tmp_path)

    from fno.agents import dispatch
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError
    from fno.agents.registry import load_registry

    def boom(short_id, *, timeout=10.0):
        raise ReachabilityProbeError(
            provider="claude", reason="timeout after 10s"
        )

    monkeypatch.setattr(claude_mod, "claude_logs_reachable", boom)

    result = dispatch.reconcile_agents()

    # Routed to errors, NOT orphaned/recovered.
    assert len(result.orphaned) == 0
    assert len(result.recovered) == 0
    assert len(result.errors) == 1
    assert result.errors[0]["reason"].startswith("claude-probe-failed")
    # Status preserved.
    assert load_registry()[0].status == "live"


def test_claude_logs_reachable_raises_on_timeout(
    tmp_path: Path, monkeypatch
) -> None:
    """Direct unit-level coverage of the tri-state probe behavior."""
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError

    def slow_run(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd=args[0], timeout=10.0)

    monkeypatch.setattr(claude_mod, "_subprocess_run", slow_run)

    with pytest.raises(ReachabilityProbeError) as exc_info:
        claude_mod.claude_logs_reachable("7c5dcf5d", timeout=10.0)
    assert "timeout" in exc_info.value.reason


def test_claude_logs_reachable_raises_on_oserror(
    tmp_path: Path, monkeypatch
) -> None:
    """OSError (permission, device error) also raises the probe-error,
    not a silent False."""
    from fno.agents.providers import claude as claude_mod
    from fno.agents.providers.base import ReachabilityProbeError

    def broken_run(*args, **kwargs):
        raise PermissionError("EACCES")

    monkeypatch.setattr(claude_mod, "_subprocess_run", broken_run)

    with pytest.raises(ReachabilityProbeError):
        claude_mod.claude_logs_reachable("7c5dcf5d", timeout=10.0)


def test_reconcile_codex_index_stat_permission_does_not_abort(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1 round-4: a PermissionError from session_index_exists must
    NOT propagate out of reconcile_agents.

    Pre-fix: Path.exists() can raise PermissionError when the parent
    directory is unreadable, which used to abort the whole reconcile
    call. Fix wraps the probe in OSError catch and treats the codex
    side as 'unreadable' — codex agents land in errors, non-codex
    agents still get reconciled.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
        ),
        dict(name="worker-gemini", provider="gemini",
             gemini_session_id="g-1"),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    def boom(*args, **kwargs):
        raise PermissionError("simulated: ~/.codex unreadable")

    monkeypatch.setattr(codex_mod, "session_index_exists", boom)

    # Should NOT raise — must classify and continue.
    result = dispatch.reconcile_agents()

    # Codex side: routed to errors with unreadable reason.
    codex_errors = [e for e in result.errors if e["provider"] == "codex"]
    assert len(codex_errors) == 1
    assert codex_errors[0]["reason"] == "codex-session-index-unreadable"
    # Status untouched.
    codex_entry = next(e for e in load_registry() if e.name == "worker-codex")
    assert codex_entry.status == "live"

    # Non-codex agents (gemini here) still classified normally. Wave 3.3
    # post-#316: gemini is probed via its cwd-pinned chats dir. "g-1" is
    # too short to match the 8-hex prefix layout, so the probe raises
    # ReachabilityProbeError and gemini lands in `errors` (not `skipped`).
    gemini_errors = [e for e in result.errors if e["provider"] == "gemini"]
    assert len(gemini_errors) == 1
    assert "gemini-probe-failed" in gemini_errors[0]["reason"]

    err = capsys.readouterr().err
    assert "codex session index path unreadable" in err


def test_reconcile_codex_index_unreadable_routes_to_errors(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """Codex P1: codex_session_index unreadable must NOT mass-orphan
    every codex agent. Each entry lands in `errors` with
    reason=codex-session-index-unreadable; statuses stay 'live'.
    """
    use_tmpdir(monkeypatch, tmp_path)
    session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    _seed_registry(
        dict(
            name="worker-codex",
            provider="codex",
            codex_session_id=session_id,
            status="live",
        ),
        dict(
            name="worker-codex-2",
            provider="codex",
            codex_session_id="ffffffff-bbbb-cccc-dddd-eeeeeeeeeeee",
            status="live",
        ),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod
    from fno.agents.providers.base import ReachabilityProbeError
    from fno.agents.registry import load_registry

    # Create a real file (so session_index_exists returns True) but
    # monkeypatch load_known_session_ids to simulate a read error.
    index = tmp_path / "session_index.jsonl"
    index.write_text("placeholder")
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: index
    )

    def fake_loader(*, session_index_path=None):
        raise ReachabilityProbeError(
            provider="codex", reason="permission denied"
        )

    monkeypatch.setattr(codex_mod, "load_known_session_ids", fake_loader)

    result = dispatch.reconcile_agents()

    # All codex entries land in errors with the precise reason.
    assert len(result.errors) == 2
    assert all(
        e["reason"] == "codex-session-index-unreadable" for e in result.errors
    )
    # Statuses must stay 'live' — false mass-orphaning would be the P1 bug.
    for entry in load_registry():
        assert entry.status == "live"
    err = capsys.readouterr().err
    assert "codex session index unreadable" in err


def test_reconcile_entries_share_key_schema(
    tmp_path: Path, monkeypatch
) -> None:
    """Sigma #3 (type-design): every list entry exposes the same key set.

    Pre-fix, the unknown-provider branch in reconcile omitted the ``id``
    key while other branches included it (sometimes None, sometimes a
    real id). Result: a consumer doing ``entry['id']`` could KeyError
    on the corner case. The fix normalizes every entry to a uniform
    schema; this test pins it.

    Tested via the reachable paths: gemini (skipped) and codex with
    a missing session-id (errors with id=None). Together they cover
    both lists' schemas.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _seed_registry(
        dict(name="worker-gemini", provider="gemini",
             gemini_session_id="some-id"),
        dict(name="worker-codex", provider="codex",
             codex_session_id=None),
    )

    from fno.agents import dispatch
    from fno.agents.providers import codex as codex_mod

    # Force the codex index path so the codex branch runs to the
    # missing-session-id check (rather than codex-index-missing).
    index = tmp_path / "session_index.jsonl"
    index.write_text("")  # exists but empty
    monkeypatch.setattr(
        codex_mod, "default_session_index_path", lambda: index
    )

    result = dispatch.reconcile_agents()

    required = {"name", "provider", "id", "reason"}
    for entry in result.skipped + result.errors:
        keys = set(entry.keys())
        assert required.issubset(keys), (
            f"entry {entry!r} missing one of {required}; has {keys}"
        )
