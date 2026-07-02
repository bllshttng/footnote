"""Task 3.1 (ab-f1b0ccd1): capture the full resume UUID at claude spawn.

Every ``/agents spawn claude`` worker must register with both its 8-hex
``claude_short_id`` (the jobId) AND a best-effort full ``claude_session_uuid``
(the stream-json ``--resume`` target the live ``/agents chat`` lane keys on).

Coverage:
  - ``resolve_session_uuid_at_spawn`` retry/backoff/never-raise unit logic.
  - AC1-HP: the claude create path persists a resolved UUID on the registry row.
  - The "full UUID unresolvable at spawn" row: a miss leaves the field None and
    the launch still reports its real short-id (resolution never gates).
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir
from fno.agents.providers import claude as claude_mod
from fno.agents.providers.claude import resolve_session_uuid_at_spawn
from fno.agents.registry import load_registry


# ---------------------------------------------------------------------------
# Unit: resolve_session_uuid_at_spawn (bounded, best-effort, never raises)
# ---------------------------------------------------------------------------


def test_resolve_at_spawn_first_probe_hit() -> None:
    """A resolvable jobId returns on the first probe with no sleep."""
    calls: list[str] = []
    sleeps: list[float] = []

    def resolver(sid: str) -> str:
        calls.append(sid)
        return "11111111-2222-3333-4444-555555555555"

    out = resolve_session_uuid_at_spawn(
        "7c5dcf5d", _resolver=resolver, _sleep=sleeps.append
    )
    assert out == "11111111-2222-3333-4444-555555555555"
    assert calls == ["7c5dcf5d"]  # returned on the first probe
    assert sleeps == []  # no sleep when the first probe hits


def test_resolve_at_spawn_retries_then_hits(monkeypatch) -> None:
    """A lagging mapping resolves after a bounded retry; sleeps between probes."""
    monkeypatch.setattr(claude_mod, "_SPAWN_UUID_RETRY_ATTEMPTS", 6)
    monkeypatch.setattr(claude_mod, "_SPAWN_UUID_RETRY_BACKOFF_SEC", 0.3)
    seq = [None, None, "late-uuid"]
    sleeps: list[float] = []

    out = resolve_session_uuid_at_spawn(
        "7c5dcf5d", _resolver=lambda sid: seq.pop(0), _sleep=sleeps.append
    )
    assert out == "late-uuid"
    assert sleeps == [0.3, 0.3]  # slept between the three probes, not after the hit


def test_resolve_at_spawn_exhausts_to_none(monkeypatch) -> None:
    """An unresolvable jobId exhausts the bounded window and returns None."""
    monkeypatch.setattr(claude_mod, "_SPAWN_UUID_RETRY_ATTEMPTS", 3)
    monkeypatch.setattr(claude_mod, "_SPAWN_UUID_RETRY_BACKOFF_SEC", 0.0)
    calls: list[str] = []

    out = resolve_session_uuid_at_spawn(
        "7c5dcf5d", _resolver=lambda sid: calls.append(sid) or None, _sleep=lambda s: None
    )
    assert out is None
    assert len(calls) == 3  # all attempts consumed


def test_resolve_at_spawn_resolver_raises_returns_none() -> None:
    """A reader error is a best-effort miss, never propagated."""

    def boom(sid: str) -> str:
        raise RuntimeError("sessions dir vanished mid-read")

    out = resolve_session_uuid_at_spawn(
        "7c5dcf5d", _resolver=boom, _sleep=lambda s: None
    )
    assert out is None


def test_resolve_at_spawn_empty_short_id_is_none() -> None:
    """An empty short-id short-circuits to None without probing."""
    calls: list[str] = []
    out = resolve_session_uuid_at_spawn(
        "", _resolver=lambda sid: calls.append(sid) or "x", _sleep=lambda s: None
    )
    assert out is None
    assert calls == []


# ---------------------------------------------------------------------------
# Integration: the claude create path persists the resolved UUID (AC1-HP)
# ---------------------------------------------------------------------------


@pytest.fixture
def workdir_claude(tmp_path, monkeypatch):
    """Isolated fno home with the fake claude on PATH (mirrors
    test_dispatch_spawn's fixture; the fake emits short_id 7c5dcf5d)."""
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


def test_spawn_claude_persists_resolved_uuid(workdir_claude, monkeypatch) -> None:
    """AC1-HP: a claude spawn whose jobId resolves persists both the short-id
    and the full claude_session_uuid on the registry row."""
    from fno.agents.cli import agents_app

    # Override the autouse stub: resolve the fake's jobId to a full UUID. The
    # file-layout read itself is unit-tested in _claude_session_registry's own
    # suite; here we prove the create path persists whatever it resolves.
    full_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    monkeypatch.setattr(claude_mod, "resolve_session_uuid", lambda short_id: full_uuid)

    result = CliRunner().invoke(
        agents_app,
        ["spawn", "uuid-agent", "-p", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output

    entry = next((e for e in load_registry() if e.name == "uuid-agent"), None)
    assert entry is not None, "registry row must exist after claude spawn"
    assert entry.claude_short_id == "7c5dcf5d"
    assert entry.claude_session_uuid == full_uuid


def test_spawn_claude_unresolved_uuid_still_launches(workdir_claude) -> None:
    """The "full UUID unresolvable at spawn" row: resolution miss leaves the
    field None, but the short-id is still reported and the worker registers."""
    from fno.agents.cli import agents_app

    # autouse _isolate_spawn_uuid_capture already stubs the reader -> None.
    result = CliRunner().invoke(
        agents_app,
        ["spawn", "nouuid-agent", "-p", "claude", "hello", "--substrate", "bg"],
        catch_exceptions=False,
    )
    assert result.exit_code == 0, result.output

    entry = next((e for e in load_registry() if e.name == "nouuid-agent"), None)
    assert entry is not None
    assert entry.claude_short_id == "7c5dcf5d"  # short-id still reported
    assert entry.claude_session_uuid is None  # uuid is a tolerated miss
