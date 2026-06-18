"""Tests for fno.agents.context — EventContext + build_context + caller_kind_from_env.

Task 1.1 from spec 2026-05-22-fno-agents-observability.md.

Locks in:
- EventContext shape (13 fields, frozen dataclass)
- request_id format invariant: 32 lowercase hex chars (UUIDv4, no dashes)
- caller_kind_from_env decision tree (env-only resolution; target-state wiring is Task 2.3)
- parse_target_session graceful degradation for stale/corrupt/missing files
"""
from __future__ import annotations

import os
import re
from dataclasses import FrozenInstanceError, fields
from pathlib import Path
from typing import Optional

import pytest


# ---------------------------------------------------------------------------
# EventContext dataclass shape
# ---------------------------------------------------------------------------


def test_event_context_is_frozen_dataclass() -> None:
    """EventContext is a frozen dataclass — fields cannot be reassigned."""
    from fno.agents.context import EventContext

    ctx = EventContext(
        from_name="fno",
        from_provider=None,
        from_session_id=None,
        from_cwd="/tmp",
        from_pid=1,
        caller_kind="human_cli",
        to_name="recipient",
        to_provider="codex",
        to_cwd=None,
        to_session_id=None,
        transport="direct-cli",
        request_id="0" * 32,
        target_session_id=None,
    )
    with pytest.raises(FrozenInstanceError):
        ctx.from_name = "other"  # type: ignore[misc]


def test_event_context_has_all_13_fields() -> None:
    """Locked-decision-9 + spec EventContext shape: 13 named fields, no extras."""
    from fno.agents.context import EventContext

    expected = {
        "from_name", "from_provider", "from_session_id", "from_cwd", "from_pid",
        "caller_kind",
        "to_name", "to_provider", "to_cwd", "to_session_id", "transport",
        "request_id", "target_session_id",
    }
    actual = {f.name for f in fields(EventContext)}
    assert actual == expected, (
        f"EventContext fields drifted: missing={expected - actual}, extra={actual - expected}"
    )


# ---------------------------------------------------------------------------
# caller_kind_from_env — env-only decision tree (Task 1.1 scope)
# ---------------------------------------------------------------------------


def _clear_caller_env(monkeypatch) -> None:
    """Strip every env var the caller-kind tree reads."""
    for k in (
        "FNO_AGENT_SELF",
        "FNO_AGENT_PROVIDER",
        "FNO_AGENT_SESSION",
        "MCP_CHANNEL_INBOUND_POKE",
        "CRON_JOB",
        "INVOCATION_ID",
    ):
        monkeypatch.delenv(k, raising=False)


def test_caller_kind_from_env_default_is_human_cli(monkeypatch) -> None:
    """With no signal env vars, caller_kind defaults to human_cli."""
    from fno.agents.context import caller_kind_from_env

    _clear_caller_env(monkeypatch)
    assert caller_kind_from_env() == "human_cli"


def test_caller_kind_from_env_nested_agent(monkeypatch) -> None:
    """FNO_AGENT_SELF set => nested_agent (priority 1)."""
    from fno.agents.context import caller_kind_from_env

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("FNO_AGENT_SELF", "parent-name")
    assert caller_kind_from_env() == "nested_agent"


def test_caller_kind_from_env_mcp_channel(monkeypatch) -> None:
    """MCP_CHANNEL_INBOUND_POKE set => mcp_channel (priority 2)."""
    from fno.agents.context import caller_kind_from_env

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("MCP_CHANNEL_INBOUND_POKE", "1")
    assert caller_kind_from_env() == "mcp_channel"


def test_caller_kind_from_env_cron_job(monkeypatch) -> None:
    """CRON_JOB env => cron (priority 4)."""
    from fno.agents.context import caller_kind_from_env

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("CRON_JOB", "1")
    assert caller_kind_from_env() == "cron"


def test_caller_kind_from_env_invocation_id(monkeypatch) -> None:
    """INVOCATION_ID (systemd) => cron (priority 4 alt signal)."""
    from fno.agents.context import caller_kind_from_env

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("INVOCATION_ID", "abc-def")
    assert caller_kind_from_env() == "cron"


def test_caller_kind_priority_nested_beats_mcp(monkeypatch) -> None:
    """Nested-agent wins over MCP channel when both env vars are set."""
    from fno.agents.context import caller_kind_from_env

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("FNO_AGENT_SELF", "parent")
    monkeypatch.setenv("MCP_CHANNEL_INBOUND_POKE", "1")
    assert caller_kind_from_env() == "nested_agent"


def test_caller_kind_priority_mcp_beats_cron(monkeypatch) -> None:
    """MCP channel wins over cron when both env signals are set."""
    from fno.agents.context import caller_kind_from_env

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("MCP_CHANNEL_INBOUND_POKE", "1")
    monkeypatch.setenv("CRON_JOB", "1")
    assert caller_kind_from_env() == "mcp_channel"


# ---------------------------------------------------------------------------
# parse_target_session — stale / corrupt / missing degradation (AC5-ERR, AC5-EDGE)
# ---------------------------------------------------------------------------


def test_parse_target_session_returns_id_when_in_progress(tmp_path: Path) -> None:
    """target-state.md with status: IN_PROGRESS returns the session_id."""
    from fno.agents.context import parse_target_session

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\nsession_id: 20260523T010000Z-99999-deadbe\n---\n"
    )
    assert parse_target_session(tmp_path) == "20260523T010000Z-99999-deadbe"


def test_parse_target_session_returns_none_when_complete(tmp_path: Path) -> None:
    """AC5-ERR: status: COMPLETE (stale) returns None — no false provenance."""
    from fno.agents.context import parse_target_session

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        "---\nstatus: COMPLETE\nsession_id: 20260523T010000Z-99999-deadbe\n---\n"
    )
    assert parse_target_session(tmp_path) is None


def test_parse_target_session_returns_none_when_blocked(tmp_path: Path) -> None:
    """status: BLOCKED is also stale — returns None."""
    from fno.agents.context import parse_target_session

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        "---\nstatus: BLOCKED\nsession_id: 20260523T010000Z-99999-deadbe\n---\n"
    )
    assert parse_target_session(tmp_path) is None


def test_parse_target_session_returns_none_when_missing(tmp_path: Path) -> None:
    """No target-state.md => returns None silently."""
    from fno.agents.context import parse_target_session

    assert parse_target_session(tmp_path) is None


def test_parse_target_session_returns_none_on_corrupt_yaml(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """AC5-EDGE: malformed frontmatter degrades to None + stderr WARN."""
    from fno.agents.context import parse_target_session

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        # Well-formed frontmatter brackets but the inner YAML is broken
        # — unbalanced bracket inside the value triggers yaml.YAMLError.
        "---\nstatus: IN_PROGRESS\nbroken: [unclosed list\n---\nbody\n"
    )
    result = parse_target_session(tmp_path)
    assert result is None
    err = capsys.readouterr().err
    assert "target-state" in err.lower() or "warn" in err.lower(), (
        f"expected stderr WARN on corrupt yaml; got: {err!r}"
    )


def test_parse_target_session_returns_none_when_no_status(tmp_path: Path) -> None:
    """target-state.md without a status: field returns None (defensive)."""
    from fno.agents.context import parse_target_session

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        "---\nsession_id: abc\n---\n"
    )
    assert parse_target_session(tmp_path) is None


def test_parse_target_session_returns_none_when_no_session_id(tmp_path: Path) -> None:
    """status IN_PROGRESS but missing session_id => None (defensive)."""
    from fno.agents.context import parse_target_session

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\n---\n"
    )
    assert parse_target_session(tmp_path) is None


# ---------------------------------------------------------------------------
# build_context — factory wiring (Task 1.1: env-only path; target wiring in 2.3)
# ---------------------------------------------------------------------------


REQUEST_ID_RE = re.compile(r"^[a-f0-9]{32}$")


def test_build_context_request_id_matches_invariant(monkeypatch) -> None:
    """AC4-INVARIANT: request_id is 32 lowercase hex chars (UUIDv4, no dashes)."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    ctx = build_context(to_name="r", to_provider="codex")
    assert REQUEST_ID_RE.match(ctx.request_id), (
        f"request_id {ctx.request_id!r} fails [a-f0-9]{{32}}"
    )


def test_build_context_request_id_unique_per_call(monkeypatch) -> None:
    """Two build_context() calls produce distinct request_ids."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    a = build_context(to_name="r", to_provider="codex").request_id
    b = build_context(to_name="r", to_provider="codex").request_id
    assert a != b


def test_build_context_default_caller_is_human_cli(monkeypatch) -> None:
    """With clean env and no target-state.md in cwd, caller_kind = human_cli, from_name = 'fno'."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.caller_kind == "human_cli"
    assert ctx.from_name == "fno"
    assert ctx.from_provider is None
    assert ctx.from_session_id is None


def test_build_context_nested_agent_from_env(monkeypatch) -> None:
    """FNO_AGENT_SELF/PROVIDER/SESSION populate from_* fields."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("FNO_AGENT_SELF", "parent-agent")
    monkeypatch.setenv("FNO_AGENT_PROVIDER", "claude")
    monkeypatch.setenv("FNO_AGENT_SESSION", "session-xyz")
    ctx = build_context(to_name="child", to_provider="codex")
    assert ctx.caller_kind == "nested_agent"
    assert ctx.from_name == "parent-agent"
    assert ctx.from_provider == "claude"
    assert ctx.from_session_id == "session-xyz"


def test_build_context_from_name_override_in_human_cli(monkeypatch) -> None:
    """human_cli with --from-name flag stamps from_name = override value."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    ctx = build_context(
        to_name="r", to_provider="codex", from_name_override="explicit-sender"
    )
    assert ctx.caller_kind == "human_cli"
    assert ctx.from_name == "explicit-sender"


def test_build_context_from_name_override_ignored_when_nested(monkeypatch) -> None:
    """Nested-agent identity (env) outranks human override."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("FNO_AGENT_SELF", "parent")
    ctx = build_context(
        to_name="r", to_provider="codex", from_name_override="ignored"
    )
    assert ctx.from_name == "parent"


def test_build_context_stamps_from_cwd_and_pid(monkeypatch) -> None:
    """from_cwd = os.getcwd(), from_pid = os.getpid()."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.from_cwd == os.getcwd()
    assert ctx.from_pid == os.getpid()


def test_build_context_recipient_fields_pass_through(monkeypatch) -> None:
    """to_name / to_provider / to_cwd / to_session_id / transport propagate from args."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    ctx = build_context(
        to_name="recv",
        to_provider="codex",
        to_cwd="/some/cwd",
        to_session_id="sess-1",
        transport="mcp",
    )
    assert ctx.to_name == "recv"
    assert ctx.to_provider == "codex"
    assert ctx.to_cwd == "/some/cwd"
    assert ctx.to_session_id == "sess-1"
    assert ctx.transport == "mcp"


def test_build_context_transport_default_is_direct_cli(monkeypatch) -> None:
    """Default transport is 'direct-cli' when not specified."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.transport == "direct-cli"


def test_build_context_target_session_id_none_when_no_state(
    monkeypatch, tmp_path: Path
) -> None:
    """build_context() in a cwd without target-state.md leaves target_session_id=None."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    monkeypatch.chdir(tmp_path)
    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.target_session_id is None


# ---------------------------------------------------------------------------
# Task 2.3 — target-state.md wired into build_context (AC5-HP / AC5-FR /
# AC5-ERR-as-build-context / priority tree placement)
# ---------------------------------------------------------------------------


def _seed_target_state(cwd: Path, status: str = "IN_PROGRESS", sid: str = "rsid-abc") -> None:
    (cwd / ".fno").mkdir(exist_ok=True)
    (cwd / ".fno" / "target-state.md").write_text(
        f"---\nstatus: {status}\nsession_id: {sid}\n---\nbody\n"
    )


def test_build_context_target_session_when_live(
    monkeypatch, tmp_path: Path
) -> None:
    """AC5-HP: status=IN_PROGRESS stamps caller_kind=target_session + target_session_id."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    _seed_target_state(tmp_path, status="IN_PROGRESS", sid="target-live-1")
    monkeypatch.chdir(tmp_path)

    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.caller_kind == "target_session"
    assert ctx.target_session_id == "target-live-1"
    assert ctx.from_name == "target"
    assert ctx.from_session_id == "target-live-1"


def test_build_context_stale_target_state_falls_back_to_human_cli(
    monkeypatch, tmp_path: Path
) -> None:
    """AC5-ERR via build_context: COMPLETE state => caller_kind=human_cli, target_session_id=None."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    _seed_target_state(tmp_path, status="COMPLETE", sid="target-stale")
    monkeypatch.chdir(tmp_path)

    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.caller_kind == "human_cli"
    assert ctx.target_session_id is None
    assert ctx.from_name == "fno"


def test_build_context_immutable_after_state_flip(
    monkeypatch, tmp_path: Path
) -> None:
    """AC5-FR: ctx is frozen — mutating target-state.md mid-flight cannot change ctx."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    _seed_target_state(tmp_path, status="IN_PROGRESS", sid="immutable-1")
    monkeypatch.chdir(tmp_path)

    ctx = build_context(to_name="r", to_provider="codex")
    snapshot_kind = ctx.caller_kind
    snapshot_rsid = ctx.target_session_id

    # Simulate mid-dispatch status flip: COMPLETE under our feet.
    _seed_target_state(tmp_path, status="COMPLETE", sid="immutable-1")

    # The already-built ctx must not change. frozen=True enforces this
    # at the object level; the assertion confirms the captured value
    # is still the IN_PROGRESS snapshot, not re-read.
    assert ctx.caller_kind == snapshot_kind == "target_session"
    assert ctx.target_session_id == snapshot_rsid == "immutable-1"


def test_build_context_nested_agent_beats_target_state(
    monkeypatch, tmp_path: Path
) -> None:
    """Priority tree #1 wins over #3: nested_agent overrides target_session."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("FNO_AGENT_SELF", "parent")
    _seed_target_state(tmp_path, status="IN_PROGRESS", sid="should-be-ignored")
    monkeypatch.chdir(tmp_path)

    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.caller_kind == "nested_agent"
    assert ctx.from_name == "parent"
    # target_session_id should be None because the env-attribution path
    # short-circuits before target-state is consulted.
    assert ctx.target_session_id is None


def test_build_context_target_state_beats_cron(
    monkeypatch, tmp_path: Path
) -> None:
    """Priority tree #3 wins over #4: target_session promotes over cron."""
    from fno.agents.context import build_context

    _clear_caller_env(monkeypatch)
    monkeypatch.setenv("CRON_JOB", "1")
    _seed_target_state(tmp_path, status="IN_PROGRESS", sid="target-over-cron")
    monkeypatch.chdir(tmp_path)

    ctx = build_context(to_name="r", to_provider="codex")
    assert ctx.caller_kind == "target_session"
    assert ctx.target_session_id == "target-over-cron"


def test_parse_target_session_handles_non_utf8(tmp_path: Path) -> None:
    """sigma-review H5: non-UTF8 bytes in target-state.md degrade silently.

    Pre-fix: read_text(encoding="utf-8") raised UnicodeDecodeError, which
    is NOT a subclass of OSError; the exception escaped parse_target_session
    and tore down every dispatch in that cwd. Post-fix: errors="replace"
    converts bad bytes to U+FFFD; the YAML parser then either ignores
    them (in comments / unused fields) or fails with YAMLError which is
    already handled.
    """
    from fno.agents.context import parse_target_session

    (tmp_path / ".fno").mkdir()
    state = tmp_path / ".fno" / "target-state.md"
    # Latin-1 byte 0x96 in a comment line; not valid UTF-8.
    state.write_bytes(
        b"---\nstatus: IN_PROGRESS\nsession_id: ok-sid\n# bad-byte: \x96 here\n---\n"
    )
    # Must NOT raise; should still extract the session_id (the bad byte
    # is in a comment line, replaced with U+FFFD, ignored by YAML).
    assert parse_target_session(tmp_path) == "ok-sid"
