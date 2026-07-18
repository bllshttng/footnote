"""Integration smoke tests for the gemini provider (Wave 2.3).

Runs through the dispatch layer end-to-end with monkeypatched
gemini.create/resume but real registry I/O, so we verify the wire
shape between dispatch and provider survives schema-pinned fixtures.

The pinned-JSON-keys drift detector verifies that _GEMINI_KEYS still
matches the fixture committed in Wave 2.0. A real-binary smoke test
(GEMINI_SMOKE=1, @pytest.mark.smoke) lives in
test_gemini_from_name_marker.py.

AC5-EDGE (cwd-pinning) is the load-bearing assertion here — a future
regression that drops the registry-cwd override and uses call-time cwd
would surface as a wrong-cwd value in the resume() invocation log.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from fno.agents import dispatch as dispatch_mod
from fno.agents.dispatch import dispatch_ask


smoke = pytest.mark.smoke


def _gemini_on_path() -> bool:
    try:
        subprocess.run(
            ["gemini", "--version"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
            timeout=5,
        )
        return True
    except (FileNotFoundError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return False


@pytest.fixture
def isolated_state(tmp_path: Path, monkeypatch) -> Path:
    """Point registry + state dir at tmp_path; mark gemini available."""
    from fno import paths
    registry_path = tmp_path / "registry.jsonl"
    state_dir = tmp_path / "state"
    monkeypatch.setattr(paths, "agents_registry_path", lambda: registry_path)
    monkeypatch.setattr(paths, "state_dir", lambda: state_dir)
    monkeypatch.setattr(
        dispatch_mod, "is_provider_available", lambda p: p == "gemini"
    )
    return tmp_path


def test_pinned_keys_match_fixture(tmp_path: Path) -> None:
    """Wave 2.3 drift detector: _GEMINI_KEYS constants block matches the
    Wave 2.0 captured fixture's top-level keys exactly.

    A future gemini release that renames any of these keys will fail
    this test before the parser silently degrades.
    """
    from fno.agents.providers import gemini as gemini_mod
    fixture = Path(__file__).parent / "fixtures" / "gemini-json-sample.json"
    data = json.loads(fixture.read_text(encoding="utf-8"))
    for slot, pinned_key in gemini_mod._GEMINI_KEYS.items():
        assert pinned_key in data, (
            f"_GEMINI_KEYS[{slot!r}]={pinned_key!r} not in fixture keys "
            f"{sorted(data.keys())} — schema drift, update _GEMINI_KEYS "
            f"after re-running scripts/smoke/capture-gemini-json.sh"
        )


@smoke
@pytest.mark.skipif(
    os.environ.get("GEMINI_SMOKE", "0") != "1",
    reason="GEMINI_SMOKE=1 not set (real-binary test; excluded from CI)",
)
@pytest.mark.skipif(
    not _gemini_on_path(),
    reason="gemini binary not on PATH",
)
def test_live_gemini_json_schema_smoke() -> None:
    """Nightly drift detector: real gemini JSON still matches _GEMINI_KEYS."""
    script = Path(__file__).parents[2] / "scripts" / "smoke" / "capture-gemini-json.sh"
    result = subprocess.run(
        ["bash", str(script)],
        cwd=Path(__file__).parents[2],
        capture_output=True,
        text=True,
        timeout=90,
        env={**os.environ, "GEMINI_SMOKE": "1"},
    )
    assert result.returncode == 0, (
        f"capture-gemini-json failed rc={result.returncode}\n"
        f"stdout={result.stdout}\nstderr={result.stderr}"
    )
    data = json.loads(result.stdout)
    assert set(data.keys()) >= {"session_id", "response", "stats"}


def test_full_create_then_followup_through_dispatch(
    isolated_state: Path, monkeypatch
) -> None:
    """End-to-end smoke: create gemini agent via _gemini_create_path, then
    follow up via dispatch_ask through the registry + provider layer.

    dispatch_ask no longer auto-creates unknown agents (ask de-overload,
    task 1.1); create now goes through the spawn verb / _gemini_create_path
    helper directly. The integration intent is preserved: create persists a
    registry row that the follow-up dispatch_ask finds and resumes correctly.
    """
    from fno.agents.dispatch import _gemini_create_path
    from fno.agents.providers import gemini as gemini_mod

    # Stub provider with deterministic results.
    create_calls = []
    resume_calls = []

    class _FakeLockHandle:
        def detach(self) -> None:
            pass

    def fake_create(*, cwd, prompt, from_name, yolo, output_path, **kwargs):
        create_calls.append({"cwd": str(cwd), "prompt": prompt})
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.touch()
        return gemini_mod.GeminiResult(
            exit_code=0,
            session_id="cedb6b44-d140-4fa4-86f1-3b3e7aed339d",
            last_msg="created",
            duration_ms=100,
        )

    def fake_resume(*, session_id, cwd, prompt, from_name, yolo, output_path, **kwargs):
        resume_calls.append({"session_id": session_id, "cwd": str(cwd)})
        return gemini_mod.GeminiResult(
            exit_code=0,
            session_id=session_id,
            last_msg="resumed",
            duration_ms=200,
        )

    monkeypatch.setattr(gemini_mod, "create", fake_create)
    monkeypatch.setattr(gemini_mod, "resume", fake_resume)

    work_dir = isolated_state / "work"
    work_dir.mkdir()

    # 1. Create via helper (spawn verb's path); persists registry row.
    create_result = _gemini_create_path(
        name="worker-A",
        message="hello",
        cwd=work_dir,
        from_name="orchestrator",
        yolo=False,
        timeout_sec=30.0,
        lock_handle=_FakeLockHandle(),
    )
    assert create_result.reply == "created"
    assert len(create_calls) == 1

    # 2. Follow-up from a DIFFERENT cwd goes through dispatch_ask, which now
    # finds the registry row and routes to gemini.resume (AC5-EDGE).
    different_dir = isolated_state / "different"
    different_dir.mkdir()

    followup_result = dispatch_ask(
        name="worker-A",
        message="follow up",
        provider=None,  # let registry decide
        cwd=different_dir,
        from_name="orchestrator",
    )
    assert followup_result.reply == "resumed"
    assert len(resume_calls) == 1
    assert resume_calls[0]["cwd"] == str(work_dir), (
        "AC5-EDGE: follow-up cwd MUST be the registry-recorded cwd, "
        f"not the call-time cwd {different_dir}"
    )


def test_create_persists_registry_with_gemini_provider_field(
    isolated_state: Path, monkeypatch
) -> None:
    """After gemini.create() succeeds via _gemini_create_path, the registry
    row has provider="gemini" and the captured gemini_session_id.

    Repointed at _gemini_create_path (create contract moved from dispatch_ask
    to spawn verb; dispatch_ask now rejects unknown agents with exit 16).
    """
    from fno.agents.dispatch import _gemini_create_path
    from fno.agents.providers import gemini as gemini_mod
    from fno.agents.registry import load_registry

    class _FakeLockHandle:
        def detach(self) -> None:
            pass

    def fake_create(**kwargs):
        output_path = kwargs["output_path"]
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.touch()
        return gemini_mod.GeminiResult(
            exit_code=0,
            session_id="aaaaaaaa-1111-2222-3333-444444444444",
            last_msg="hi",
            duration_ms=10,
        )
    monkeypatch.setattr(gemini_mod, "create", fake_create)

    work_dir = isolated_state / "work"
    work_dir.mkdir()

    _gemini_create_path(
        name="worker-A",
        message="hi",
        cwd=work_dir,
        from_name="orchestrator",
        yolo=False,
        timeout_sec=30.0,
        lock_handle=_FakeLockHandle(),
    )

    entries = load_registry()
    assert len(entries) == 1
    entry = entries[0]
    assert entry.harness == "gemini"
    assert entry.harness_session_id == "aaaaaaaa-1111-2222-3333-444444444444"
    assert entry.cwd == str(work_dir)
    assert entry.status == "live"


def test_reachability_probe_matches_real_session_layout(
    tmp_path: Path, monkeypatch
) -> None:
    """The reachability probe finds a session at the same layout that
    real gemini 0.42.0 writes to: ~/.gemini/tmp/<cwd-basename>/chats/
    session-<TS>-<short-uuid>.jsonl."""
    from fno.agents.providers import gemini as gemini_mod

    fake_home = tmp_path / "home"
    chats_dir = fake_home / ".gemini" / "tmp" / "myproject" / "chats"
    chats_dir.mkdir(parents=True)
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    session_id = "ffeeddcc-1111-2222-3333-444444444444"
    # Match the real gemini filename shape from Wave 2.0 fixture findings.
    session_file = chats_dir / "session-2026-05-21T22-13-ffeeddcc.jsonl"
    session_file.write_text(
        json.dumps({
            "sessionId": session_id,
            "projectHash": "deadbeef",
            "startTime": "2026-05-21T22:13:00Z",
            "kind": "main",
        }) + "\n"
    )

    cwd = tmp_path / "myproject"
    cwd.mkdir()
    assert gemini_mod.gemini_session_reachable(session_id, cwd) is True

    # Negative: a different short-prefix UUID is NOT found.
    bogus = "11223344-1111-2222-3333-444444444444"
    assert gemini_mod.gemini_session_reachable(bogus, cwd) is False
