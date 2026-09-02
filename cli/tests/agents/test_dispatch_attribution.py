"""Tests for dispatch.py emit-site migration to EventContext.

Task 2.1 from 2026-05-22-fno-agents-observability.md.

Locks in:
- AC4-HP: agent_ask_started + agent_ask_done share the same request_id
  (consistent correlation within a single dispatch).
- from_name asymmetry closed: agent_ask_done on the create path now
  carries from_name (parity with followup-path emits which already do).
- AC3-HP: nested-agent attribution propagates from_* fields when
  FNO_AGENT_SELF and friends are set on the dispatcher's env.
- Module ContextVar reset: leaves no state between dispatches (sibling
  dispatches on the same thread don't observe each other's ctx).

We monkeypatch ``events.emit`` to capture records in-memory. Since
``emit_with_context`` delegates to ``emit``, the patched function sees
records from both legacy and migrated call sites.
"""
from __future__ import annotations

import re
from pathlib import Path
from typing import Any

import pytest

from fno.paths_testing import use_tmpdir
from tests.agents._fake_claude import configure_fake, install_fake_claude


REQUEST_ID_RE = re.compile(r"^[a-f0-9]{32}$")


def _install_fake_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Put a fake claude on PATH so dispatch_ask's create path succeeds."""
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    configure_fake(monkeypatch)


def _patch_emit_capture(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, dict[str, Any]]]:
    """Replace events.emit with an in-memory list. Returns the list."""
    captured: list[tuple[str, dict[str, Any]]] = []
    from fno.agents import events as events_mod

    def fake_emit(kind: str, **kw: Any) -> None:
        captured.append((kind, dict(kw)))

    monkeypatch.setattr(events_mod, "emit", fake_emit)
    return captured


def _clear_agent_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for k in (
        "FNO_AGENT_SELF",
        "FNO_AGENT_HARNESS",
        "FNO_AGENT_SESSION",
        "MCP_CHANNEL_INBOUND_POKE",
        "CRON_JOB",
        "INVOCATION_ID",
    ):
        monkeypatch.delenv(k, raising=False)


# ---------------------------------------------------------------------------
# AC4-HP — request_id correlation
# ---------------------------------------------------------------------------


def _seed_agent_entry(tmp_path: Path, name: str, provider: str = "claude") -> None:
    """Pre-register an agent so dispatch_ask routes to follow-up (not unknown-agent)."""
    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name=name,
            harness=provider,
            cwd=str(tmp_path),
            log_path=str(tmp_path / f"{name}.log"),
            short_id="abc12345" if provider == "claude" else "",
            harness_session_id="sess-abc" if provider in ("codex", "gemini") else None,
        )
    ])


# ---------------------------------------------------------------------------


@pytest.mark.parametrize("bad_char", ["\x00", "\n", "\r", "="])
def test_dispatch_rejects_name_with_env_corrupting_char(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, bad_char: str
) -> None:
    """sigma-review H4: names containing NUL / \\n / \\r / = are rejected at validate time.

    Pre-fix: such a name would land in FNO_AGENT_SELF and crash
    subprocess.run with ValueError (NUL) or split env values across
    lines (\\n / \\r) or break key=value shape (=). Post-fix: explicit
    validation rejects at the boundary with exit_code=2.
    """
    use_tmpdir(monkeypatch, tmp_path)
    _clear_agent_env(monkeypatch)
    from fno.agents.dispatch import DispatchAskError, _validate_inputs

    name = f"agent{bad_char}name"
    with pytest.raises(DispatchAskError) as excinfo:
        _validate_inputs(name=name, message="hi", from_name="fno")
    assert excinfo.value.exit_code == 2
    assert "forbidden character" in str(excinfo.value)


# ---------------------------------------------------------------------------
# Spawn-path context (codex P2, PR #457): dispatch_spawn wraps the create
# helpers in the same _DISPATCH_CTX envelope the old ask dispatcher used, so
# the helpers' agent_ask_started/agent_ask_done emits keep request_id /
# from_name / caller attribution after the create moved off ask.
# ---------------------------------------------------------------------------


def test_spawn_create_events_carry_context(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """dispatch_spawn (claude plain) emits create events with the full envelope."""
    use_tmpdir(monkeypatch, tmp_path)
    _install_fake_path(tmp_path, monkeypatch)
    _clear_agent_env(monkeypatch)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    (tmp_path / "home").mkdir()
    captured = _patch_emit_capture(monkeypatch)

    from fno.agents.dispatch import dispatch_spawn

    result = dispatch_spawn(
        name="spawned-alpha",
        message="hello",
        harness="claude",
        cwd=tmp_path,
        from_name="orchestrator",
    )
    assert result.kind == "created"

    started = [e for e in captured if e[0] == "agent_ask_started"]
    done = [e for e in captured if e[0] == "agent_ask_done"]
    assert started, f"missing agent_ask_started; got: {[e[0] for e in captured]}"
    assert done, f"missing agent_ask_done; got: {[e[0] for e in captured]}"
    for label, ev in (("started", started[0][1]), ("done", done[0][1])):
        assert "request_id" in ev, f"{label} event lacks request_id"
        assert REQUEST_ID_RE.match(ev["request_id"]), (
            f"{label} request_id format invariant violated: {ev['request_id']!r}"
        )
        assert ev.get("from_name") == "orchestrator", (
            f"{label} event lacks from_name attribution: {ev}"
        )
    assert started[0][1]["request_id"] == done[0][1]["request_id"], (
        "started/done must share one request_id"
    )
