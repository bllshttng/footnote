"""Coverage gap 3: fatal codex error propagation through dispatch.

US4-codex's provider tests in ``test_providers_codex_create.py`` cover
``CodexInvocationError`` / ``NoSessionIdError`` at the provider layer.
The dispatch layer is tested with happy-path mocks. This file pins the
chain end-to-end: provider raises -> dispatch maps to the right
``DispatchAskError`` exit code, emits ``agent_ask_failed`` with the
right ``stage`` field, and leaves the registry empty (atomicity).

Spec note on the design vs reality reframing: codex 0.130.0 does NOT
emit a ``task.error`` JSONL event for fatal errors; the speculative
event-type from the US4-codex design doc never materialized. Real
fatal errors surface as one of two shapes:

  - non-zero exit + zero useful JSONL events (auth failure, model
    error, missing ``--cd`` directory) -> CodexInvocationError(exit_code)
  - exit 0 with a JSONL stream missing ``thread.started``
    -> NoSessionIdError(types_seen)

Both are covered here.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _force_codex_on_path(monkeypatch, tmp_path: Path) -> None:
    """Make ``is_provider_available('codex')`` return True without a real binary."""
    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir(exist_ok=True)
    fake = bin_dir / "codex"
    fake.write_text("#!/bin/sh\nexit 1\n")
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(bin_dir))


def _read_events(tmp_path: Path) -> list[dict]:
    """Return all events.jsonl records (or empty if file absent)."""
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


class _FakeLockHandle:
    """Minimal lock handle stub for _codex_create_path tests."""
    def detach(self) -> None:
        pass


def test_codex_invocation_error_maps_to_exit_1(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-HP: CodexInvocationError(1) -> DispatchAskError(exit_code=1).
    Repointed at _codex_create_path (create contract moved from dispatch_ask to spawn verb)."""
    use_tmpdir(monkeypatch, tmp_path)
    _force_codex_on_path(monkeypatch, tmp_path)

    from fno.agents.dispatch import DispatchAskError, _codex_create_path
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    def fake_create(**_kwargs):
        raise codex_mod.CodexInvocationError(1)

    monkeypatch.setattr(codex_mod, "create", fake_create)

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    with pytest.raises(DispatchAskError) as exc_info:
        _codex_create_path(
            name="failed-codex",
            message="msg",
            cwd=cwd,
            from_name="orchestrator",
            yolo=False,
            timeout_sec=10.0,
            lock_handle=_FakeLockHandle(),
        )

    assert exc_info.value.exit_code == 1

    # Atomicity: no registry entry for the failed-create name.
    entries = [e for e in load_registry() if e.name == "failed-codex"]
    assert entries == []

    # Forensic event with the right stage + returncode.
    events = _read_events(tmp_path)
    matches = [
        e for e in events
        if e.get("kind") == "agent_ask_failed"
        and e.get("stage") == "codex-subprocess"
        and e.get("name") == "failed-codex"
    ]
    assert len(matches) == 1, f"expected one agent_ask_failed event, got: {events}"
    assert matches[0]["returncode"] == 1
    assert matches[0]["provider"] == "codex"


def test_codex_invocation_error_propagates_nonzero_exit_code(
    tmp_path: Path, monkeypatch
) -> None:
    """Variant of AC6-HP: structured non-1 exit codes pass through verbatim.
    Repointed at _codex_create_path (create contract moved from dispatch_ask to spawn verb)."""
    use_tmpdir(monkeypatch, tmp_path)
    _force_codex_on_path(monkeypatch, tmp_path)

    from fno.agents.dispatch import DispatchAskError, _codex_create_path
    from fno.agents.providers import codex as codex_mod

    # exit_code=12 is the structured "tee-open EACCES" provider code per
    # the US4-codex commentary; dispatch must propagate, not collapse to 1.
    def fake_create(**_kwargs):
        raise codex_mod.CodexInvocationError(12)

    monkeypatch.setattr(codex_mod, "create", fake_create)

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    with pytest.raises(DispatchAskError) as exc_info:
        _codex_create_path(
            name="permission-fail",
            message="msg",
            cwd=cwd,
            from_name="orchestrator",
            yolo=False,
            timeout_sec=10.0,
            lock_handle=_FakeLockHandle(),
        )

    assert exc_info.value.exit_code == 12


def test_no_session_id_error_maps_to_exit_11(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-ERR: NoSessionIdError -> DispatchAskError(exit_code=11).
    Repointed at _codex_create_path (create contract moved from dispatch_ask to spawn verb)."""
    use_tmpdir(monkeypatch, tmp_path)
    _force_codex_on_path(monkeypatch, tmp_path)

    from fno.agents.dispatch import DispatchAskError, _codex_create_path
    from fno.agents.providers import codex as codex_mod
    from fno.agents.registry import load_registry

    types_seen = {"turn.started", "turn.completed"}

    def fake_create(**_kwargs):
        raise codex_mod.NoSessionIdError(types_seen)

    monkeypatch.setattr(codex_mod, "create", fake_create)

    cwd = tmp_path / "workdir"
    cwd.mkdir()

    with pytest.raises(DispatchAskError) as exc_info:
        _codex_create_path(
            name="no-session",
            message="msg",
            cwd=cwd,
            from_name="orchestrator",
            yolo=False,
            timeout_sec=10.0,
            lock_handle=_FakeLockHandle(),
        )

    assert exc_info.value.exit_code == 11
    assert load_registry() == []

    events = _read_events(tmp_path)
    matches = [
        e for e in events
        if e.get("kind") == "agent_ask_failed"
        and e.get("stage") == "codex-no-session"
        and e.get("name") == "no-session"
    ]
    assert len(matches) == 1
    # types_seen is sorted in the event payload (dispatch normalizes for
    # determinism); both expected items present.
    assert "turn.started" in matches[0]["types_seen"]
    assert "turn.completed" in matches[0]["types_seen"]
