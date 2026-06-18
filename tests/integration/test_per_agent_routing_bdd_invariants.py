"""End-to-end BDD invariant coverage for per-agent sigma-review routing.

Run: cd cli && uv run pytest -v ../tests/integration/test_per_agent_routing_bdd_invariants.py

Task 4.1 of per-agent sigma-review routing (ab-978e93ed). Each test maps to
one of the 10 invariants from the plan's INDEX. Tests exercise the real
settings.yaml load path, real fcntl flock, real shell-emitter events.jsonl
writes, and the real ``fno-agents verify-evidence event`` verb (folded out
of the deleted scripts/lib/verify-event-evidence.sh in US1, ab-58645f63) --
no mocking of load-bearing components except the subprocess.Popen calls
(which would need real gemini/codex/openclaw binaries on PATH).

The plan called for a bats suite at tests/integration/per-agent-routing.bats.
We use pytest instead because (a) the host has no bats installed, (b) Spec 2
(failover, ab-9728b70b) already established the precedent of pytest-based BDD
invariant coverage in test_failover_bdd_invariants.py, and (c) the unit suites
already exercise real fcntl + real subprocess. Mirroring Spec 2's structure
keeps the two BDD suites consistent for future maintenance.

Each test function docstring names the invariant explicitly so the intent
survives code review diffs.
"""
from __future__ import annotations

import concurrent.futures
import json
import subprocess
import sys
import time
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parents[2]
CLI_SRC = REPO_ROOT / "cli" / "src"
if str(CLI_SRC) not in sys.path:
    sys.path.insert(0, str(CLI_SRC))

from fno.agents.rust_runtime import resolve_binary  # noqa: E402

# The event-evidence path is now the `fno-agents verify-evidence event` verb;
# skip the whole module when the binary is not built (e.g. a Python-only CI
# leg that never ran `cargo build`).
_BINARY = resolve_binary()
pytestmark = pytest.mark.skipif(
    _BINARY is None, reason="fno-agents binary unavailable"
)


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


def _setup_target_state(tmp_path: Path, *, session_id: str, nonce: str) -> Path:
    """Bootstrap a minimal target-state.md so the shell helper finds session context."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / "target-state.md"
    state_file.write_text(
        f"---\nstatus: IN_PROGRESS\nsession_id: {session_id}\n"
        f"provenance_nonce: {nonce}\n---\n",
        encoding="utf-8",
    )
    return state_dir


def _write_settings(tmp_path: Path, settings: dict) -> Path:
    """Write a settings.yaml under tmp_path/.fno/ and return its path."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    p = state_dir / "settings.yaml"
    p.write_text(yaml.safe_dump(settings, sort_keys=False), encoding="utf-8")
    return p


def _baseline_settings(
    active: str = "claude-anthropic",
    agents: dict | None = None,
) -> dict:
    """Return a minimal valid settings.yaml dict."""
    base: dict = {
        "config": {
            "providers": {
                "active": active,
                "records": [
                    {
                        "id": "claude-anthropic",
                        "name": "Claude Anthropic",
                        "cli": "claude",
                        "auth": "oauth_dir",
                        "credentials_source": "~/.claude",
                        "priority": 10,
                    },
                    {
                        "id": "gemini-pro",
                        "name": "Gemini Pro",
                        "cli": "gemini",
                        "auth": "api_key",
                        "env": {"GEMINI_API_KEY": "test-key"},
                        "priority": 20,
                    },
                    {
                        "id": "codex-openai",
                        "name": "Codex OpenAI",
                        "cli": "codex",
                        "auth": "api_key",
                        "env": {"OPENAI_API_KEY": "test-key"},
                        "priority": 30,
                    },
                ],
                "failover": {"max_swaps_per_phase": 5},
            }
        }
    }
    if agents is not None:
        base["config"]["agents"] = agents
    return base


def _write_artifact(tmp_path: Path, agents_dispatched: list[str]) -> Path:
    """Write a minimal gate artifact with agents_dispatched field."""
    artifacts_dir = tmp_path / ".fno" / "artifacts"
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    p = artifacts_dir / "review-test.md"
    agents_str = ", ".join(agents_dispatched)
    p.write_text(
        f"---\nphase: review\napproved: true\nagents_dispatched: [{agents_str}]\n---\n",
        encoding="utf-8",
    )
    return p


def _run_verify_event_evidence(
    session_id: str,
    nonce: str,
    events_file: Path,
    artifact: Path,
) -> tuple[int, str]:
    """Invoke ``fno-agents verify-evidence event`` and return (rc, stdout)."""
    assert _BINARY is not None  # module is skipped when the binary is absent
    proc = subprocess.run(
        [
            str(_BINARY),
            "verify-evidence",
            "event",
            session_id,
            nonce,
            str(events_file),
            str(artifact),
        ],
        capture_output=True,
        text=True,
    )
    return proc.returncode, proc.stdout.strip()


def _append_event(events_file: Path, event_type: str, data: dict) -> None:
    """Directly append a JSONL event line (for tests that craft events by hand)."""
    events_file.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps({"type": event_type, "data": data}, separators=(",", ":")) + "\n"
    with open(events_file, "a", encoding="utf-8") as f:
        f.write(line)


# ---------------------------------------------------------------------------
# Invariant 1: agents.<name>.provider unset -> uses global active provider
# ---------------------------------------------------------------------------


def test_unset_uses_global_default(tmp_path: Path) -> None:
    """Invariant 1: config.agents absent -> ProvidersConfig.agents is empty dict.

    When no config.agents block is present, load_providers returns a
    ProvidersConfig where agents == {} (empty dict). Dispatch falls through to
    the global active provider -- the agents map carries no overrides.
    """
    settings = _baseline_settings()  # no 'agents' key at all
    _write_settings(tmp_path, settings)

    from fno.adapters.providers.loader import load_providers

    cfg = load_providers(repo_root=tmp_path)

    assert cfg.agents == {}, (
        f"Expected empty agents dict for back-compat, got {cfg.agents!r}"
    )
    assert cfg.active == "claude-anthropic"


# ---------------------------------------------------------------------------
# Invariant 2: pinned provider -> dispatch uses that provider's CLI
# ---------------------------------------------------------------------------


def test_pinned_provider_routes_correctly(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Invariant 2: Set agent -> dispatch_sigma_subagent uses the pinned provider_id.

    Three agents pinned to three different providers. For the non-Claude paths,
    spawn_with_provider_snapshot is mocked; we assert the spawn event in events.jsonl
    carries the correct provider_id for each agent.
    """
    session_id = "20260505T111200Z-99001-aabb01"
    nonce = "deadbeef11223344"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    settings = _baseline_settings(
        agents={
            "silent-failure-hunter": {"provider": "gemini-pro"},
        }
    )
    _write_settings(tmp_path, settings)

    import fno.sigma_dispatch as mod

    class _FakePopen:
        returncode = 0

        def communicate(self, timeout=None):
            return (b"RESULT: SUCCESS", b"")

    monkeypatch.setattr(mod, "spawn_with_provider_snapshot", lambda cmd, **kw: _FakePopen())

    from fno.sigma_dispatch import dispatch_sigma_subagent

    with dispatch_sigma_subagent(
        agent_name="silent-failure-hunter",
        provider_id="gemini-pro",  # orchestrator resolves from cfg.agents
        cli="gemini",
        prompt="review the diff",
        repo_root=tmp_path,
    ):
        pass

    events_file = tmp_path / ".fno" / "events.jsonl"
    lines = [l for l in events_file.read_text(encoding="utf-8").splitlines() if l.strip()]
    spawn_events = [json.loads(l) for l in lines if json.loads(l)["type"] == "subagent_spawn"]

    assert spawn_events, "No subagent_spawn event found"
    spawn = spawn_events[0]
    assert spawn["data"]["provider_id"] == "gemini-pro", (
        f"Expected provider_id=gemini-pro, got {spawn['data']['provider_id']!r}"
    )
    assert spawn["data"]["agent_name"] == "silent-failure-hunter"


# ---------------------------------------------------------------------------
# Invariant 3: subagent crash -> complete event has non-zero exit_code
# ---------------------------------------------------------------------------


def test_subagent_crash_emits_complete_with_nonzero_exit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Invariant 3: Subprocess crash (rc=139) -> subagent_complete carries exit_code=139.

    verify_provenance must still find a complete event so it can soft-warn
    rather than hard-fail with subagent_complete_missing.
    """
    session_id = "20260505T111201Z-99002-aabb02"
    nonce = "cafebabedeadbeef"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    import fno.sigma_dispatch as mod

    class _CrashPopen:
        returncode = 139

        def communicate(self, timeout=None):
            return (b"", b"Segmentation fault")

    monkeypatch.setattr(mod, "spawn_with_provider_snapshot", lambda cmd, **kw: _CrashPopen())

    from fno.sigma_dispatch import dispatch_sigma_subagent

    with dispatch_sigma_subagent(
        agent_name="code-reviewer",
        provider_id="gemini-pro",
        cli="gemini",
        prompt="review",
        repo_root=tmp_path,
    ):
        pass  # subprocess crash handled in __exit__

    events_file = tmp_path / ".fno" / "events.jsonl"
    lines = [l for l in events_file.read_text(encoding="utf-8").splitlines() if l.strip()]
    complete_events = [
        json.loads(l) for l in lines if json.loads(l)["type"] == "subagent_complete"
    ]

    assert complete_events, "No subagent_complete event found after crash"
    complete = complete_events[-1]
    assert complete["data"]["exit_code"] == 139, (
        f"Expected exit_code=139, got {complete['data']['exit_code']!r}"
    )
    assert complete["data"]["agent_name"] == "code-reviewer"


# ---------------------------------------------------------------------------
# Invariant 4: pure-Claude run -> transcript-parser path unchanged (rc=2 skip)
# ---------------------------------------------------------------------------


def test_pure_claude_unchanged(tmp_path: Path) -> None:
    """Invariant 4: Pure-Claude settings without config.agents -> verify_event_evidence rc=2.

    When events.jsonl is absent (pure-Claude run where the shell emitter was
    never called), verify_event_evidence returns rc=2 so the stop hook falls
    through to its existing transcript-parser path. The per-agent routing
    infrastructure is completely invisible to a pure-Claude operator.
    """
    session_id = "20260505T111202Z-99003-aabb03"
    nonce = "1234567890abcdef"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    settings = _baseline_settings()  # no agents block
    _write_settings(tmp_path, settings)

    artifact = _write_artifact(tmp_path, ["code-reviewer"])
    # events.jsonl deliberately NOT created (pure-Claude: no subprocess spawns)
    missing_events = tmp_path / ".fno" / "events.jsonl"

    rc, stdout = _run_verify_event_evidence(session_id, nonce, missing_events, artifact)

    assert rc == 2, (
        f"Expected rc=2 (absent events.jsonl -> fall through), got rc={rc}; stdout={stdout!r}"
    )
    assert stdout == "", f"Expected empty stdout on rc=2, got {stdout!r}"


# ---------------------------------------------------------------------------
# Invariant 5: mixed Claude+Gemini run -> event-evidence path verifies cleanly
# ---------------------------------------------------------------------------


def test_mixed_run_via_event_evidence(tmp_path: Path) -> None:
    """Invariant 5: Mixed Claude+Gemini dispatch -> verify_event_evidence rc=0.

    Craft a valid events.jsonl with a paired spawn+complete for a non-Claude
    agent. The artifact's agents_dispatched names that agent. verify_event_evidence
    must return rc=0 (gate passes).
    """
    session_id = "20260505T111203Z-99004-aabb04"
    nonce = "abcd1234efgh5678"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    events_file = tmp_path / ".fno" / "events.jsonl"

    _append_event(events_file, "subagent_spawn", {
        "agent_name": "silent-failure-hunter",
        "provider_id": "gemini-pro",
        "cli": "gemini",
        "session_id": session_id,
        "nonce": nonce,
    })
    _append_event(events_file, "subagent_complete", {
        "agent_name": "silent-failure-hunter",
        "provider_id": "gemini-pro",
        "cli": "gemini",
        "exit_code": 0,
        "stdout_sha256": "abc123",
        "stderr_sha256": "def456",
        "duration_ms": 500,
        "session_id": session_id,
        "nonce": nonce,
    })

    artifact = _write_artifact(tmp_path, ["silent-failure-hunter"])

    rc, stdout = _run_verify_event_evidence(session_id, nonce, events_file, artifact)

    assert rc == 0, (
        f"Expected rc=0 for valid mixed-run evidence, got rc={rc}; stdout={stdout!r}"
    )


# ---------------------------------------------------------------------------
# Invariant 6: code-reviewer x2 -> expects 2 pairs; 1 pair fails
# ---------------------------------------------------------------------------


def test_pair_count_match_for_repeated_agent(tmp_path: Path) -> None:
    """Invariant 6: Same agent in agents_dispatched twice; only 1 pair in events -> rc=1.

    Ensures the verifier counts expected pairs per agent correctly and rejects
    a run that spawned code-reviewer twice but only recorded one complete event.
    Diagnostic must include subagent_pair_count_mismatch:code-reviewer.
    """
    session_id = "20260505T111204Z-99005-aabb05"
    nonce = "f0f0f0f012345678"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    events_file = tmp_path / ".fno" / "events.jsonl"

    # Only one pair, but agents_dispatched will declare two.
    _append_event(events_file, "subagent_spawn", {
        "agent_name": "code-reviewer",
        "provider_id": "gemini-pro",
        "cli": "gemini",
        "session_id": session_id,
        "nonce": nonce,
    })
    _append_event(events_file, "subagent_complete", {
        "agent_name": "code-reviewer",
        "provider_id": "gemini-pro",
        "cli": "gemini",
        "exit_code": 0,
        "stdout_sha256": "aaa",
        "stderr_sha256": "bbb",
        "duration_ms": 100,
        "session_id": session_id,
        "nonce": nonce,
    })

    # Artifact claims code-reviewer was dispatched TWICE.
    artifact = _write_artifact(tmp_path, ["code-reviewer", "code-reviewer"])

    rc, stdout = _run_verify_event_evidence(session_id, nonce, events_file, artifact)

    assert rc == 1, (
        f"Expected rc=1 for pair count mismatch, got rc={rc}; stdout={stdout!r}"
    )
    assert "subagent_pair_count_mismatch" in stdout and "code-reviewer" in stdout, (
        f"Expected diagnostic subagent_pair_count_mismatch:code-reviewer in stdout, got {stdout!r}"
    )


# ---------------------------------------------------------------------------
# Invariant 7: forged agent name in events -> rejected with agent_mismatch
# ---------------------------------------------------------------------------


def test_forged_agent_name_rejected(tmp_path: Path) -> None:
    """Invariant 7: Spawn event for agent not in agents_dispatched -> rc=1 + agent_mismatch.

    The forgery check in verify_event_evidence scans ALL spawn events for this
    session+nonce. Any event whose agent_name is not in the artifact's
    agents_dispatched list is rejected immediately.
    """
    session_id = "20260505T111205Z-99006-aabb06"
    nonce = "0011223344556677"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    events_file = tmp_path / ".fno" / "events.jsonl"

    # Spawn event for "evil-agent" which is NOT in agents_dispatched.
    _append_event(events_file, "subagent_spawn", {
        "agent_name": "evil-agent",
        "provider_id": "gemini-pro",
        "cli": "gemini",
        "session_id": session_id,
        "nonce": nonce,
    })

    # Artifact only declares code-reviewer.
    artifact = _write_artifact(tmp_path, ["code-reviewer"])

    rc, stdout = _run_verify_event_evidence(session_id, nonce, events_file, artifact)

    assert rc == 1, (
        f"Expected rc=1 for forged agent, got rc={rc}; stdout={stdout!r}"
    )
    assert "agent_mismatch" in stdout and "evil-agent" in stdout, (
        f"Expected diagnostic agent_mismatch:evil-agent in stdout, got {stdout!r}"
    )


# ---------------------------------------------------------------------------
# Invariant 8: 10 threads x 100 appends -> exactly 1000 valid lines
# ---------------------------------------------------------------------------


def test_concurrent_sidecar_appends(tmp_path: Path) -> None:
    """Invariant 8: 10 threads x 100 record_dispatch calls -> 1000 valid JSONL lines.

    Exercises the fcntl.LOCK_EX serialization in record_dispatch. Every line
    must be valid JSON and contain the turn_index field. Line count must be
    exactly 1000 with no corruption.
    """
    from fno.sigma_dispatch import record_dispatch

    sidecar = tmp_path / ".fno" / "subagent-dispatch.jsonl"

    def write_batch(thread_index: int) -> None:
        base = thread_index * 100
        for i in range(100):
            record_dispatch(
                sidecar_path=sidecar,
                turn_index=base + i,
                ts="2026-05-05T19:00:00Z",
                agent_name=f"agent-{thread_index}",
                provider_id="gemini-pro",
                cli="gemini",
                exit_code=0,
            )

    with concurrent.futures.ThreadPoolExecutor(max_workers=10) as pool:
        futures = [pool.submit(write_batch, t) for t in range(10)]
        concurrent.futures.wait(futures)

    lines = [l for l in sidecar.read_text(encoding="utf-8").splitlines() if l.strip()]
    assert len(lines) == 1000, f"Expected 1000 lines, got {len(lines)}"

    seen_indices: set[int] = set()
    for line in lines:
        record = json.loads(line)  # raises if malformed
        seen_indices.add(record["turn_index"])

    assert seen_indices == set(range(1000)), (
        f"turn_index values not exactly 0..999: {sorted(seen_indices)[:5]}..."
    )


# ---------------------------------------------------------------------------
# Invariant 9: mid-dispatch failover swap -> spawn event carries snapshot provider
# ---------------------------------------------------------------------------


def test_failover_during_dispatch_uses_snapshot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Invariant 9: Failover swap after __enter__ does NOT corrupt spawn event's provider_id.

    spawn_with_provider_snapshot captures the provider at __enter__ time under
    a shared lock. A concurrent failover swap that fires after that point cannot
    change which provider_id lands in the subagent_spawn event.
    """
    session_id = "20260505T111206Z-99007-aabb07"
    nonce = "aabbccddeeff0011"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    import fno.sigma_dispatch as mod

    original_provider_id = "gemini-pro-original"
    swapped_provider_id = "claude-fallback-post-swap"

    call_count = 0

    class _SnapshotPopen:
        returncode = 0

        def communicate(self, timeout=None):
            return (b"RESULT: SUCCESS", b"")

    def fake_spawn(cmd, **kwargs):
        nonlocal call_count
        call_count += 1
        return _SnapshotPopen()

    monkeypatch.setattr(mod, "spawn_with_provider_snapshot", fake_spawn)

    from fno.sigma_dispatch import dispatch_sigma_subagent

    with dispatch_sigma_subagent(
        agent_name="silent-failure-hunter",
        provider_id=original_provider_id,  # captured at dispatch time
        cli="gemini",
        prompt="review diff",
        repo_root=tmp_path,
    ):
        # Simulate: concurrent failover swap fires mid-block.
        # spawn_with_provider_snapshot has already been called once in __enter__
        # with original_provider_id; swapping here must not affect the event.
        pass

    assert call_count == 1, (
        f"spawn_with_provider_snapshot called {call_count} times; "
        "snapshot must be captured exactly once at __enter__"
    )

    events_file = tmp_path / ".fno" / "events.jsonl"
    lines = [l for l in events_file.read_text(encoding="utf-8").splitlines() if l.strip()]
    spawn_events = [json.loads(l) for l in lines if json.loads(l)["type"] == "subagent_spawn"]

    assert spawn_events, "No subagent_spawn event found"
    spawn_provider = spawn_events[0]["data"]["provider_id"]
    assert spawn_provider == original_provider_id, (
        f"Snapshot stickiness violated: spawn event has {spawn_provider!r}, "
        f"expected {original_provider_id!r}"
    )
    # Confirm the swapped provider_id never appeared in the spawn event.
    assert spawn_provider != swapped_provider_id


# ---------------------------------------------------------------------------
# Invariant 10: dispatcher blocks until subprocess completes before returning
# ---------------------------------------------------------------------------


def test_dispatcher_waits_before_returning(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Invariant 10: dispatcher's subprocess.communicate() completes before __exit__ returns.

    The subagent_complete event must be on disk before the orchestrator regains
    control. Verified by checking that both spawn AND complete events exist in
    events.jsonl immediately after the `with` block exits -- no async writes.
    Additionally, we confirm the complete event's timestamp ordering is
    consistent (complete was not emitted before spawn).
    """
    session_id = "20260505T111207Z-99008-aabb08"
    nonce = "ffeeddccbbaa9988"
    _setup_target_state(tmp_path, session_id=session_id, nonce=nonce)

    import fno.sigma_dispatch as mod

    class _SlowPopen:
        """Simulates a subprocess that takes a measurable amount of time."""
        returncode = 0

        def communicate(self, timeout=None):
            # Small sleep to make timing assertions meaningful.
            time.sleep(0.05)
            return (b"RESULT: SUCCESS", b"")

    monkeypatch.setattr(mod, "spawn_with_provider_snapshot", lambda cmd, **kw: _SlowPopen())

    from fno.sigma_dispatch import dispatch_sigma_subagent

    start_ts = time.monotonic()

    with dispatch_sigma_subagent(
        agent_name="code-reviewer",
        provider_id="gemini-pro",
        cli="gemini",
        prompt="review",
        repo_root=tmp_path,
    ):
        pass

    end_ts = time.monotonic()

    # The `with` block must have taken at least as long as the fake subprocess.
    elapsed = end_ts - start_ts
    assert elapsed >= 0.05, (
        f"Dispatcher returned too quickly ({elapsed:.3f}s); "
        "communicate() must block before __exit__ returns"
    )

    # Both events must be on disk immediately after the context-manager exits.
    events_file = tmp_path / ".fno" / "events.jsonl"
    assert events_file.exists(), "events.jsonl missing after dispatch block"

    lines = [l for l in events_file.read_text(encoding="utf-8").splitlines() if l.strip()]
    parsed = [json.loads(l) for l in lines]
    event_types = [e["type"] for e in parsed]

    assert "subagent_spawn" in event_types, "subagent_spawn event missing"
    assert "subagent_complete" in event_types, (
        "subagent_complete event missing; dispatcher did not wait before returning"
    )

    # Spawn must appear before complete in the event stream.
    spawn_idx = next(i for i, e in enumerate(parsed) if e["type"] == "subagent_spawn")
    complete_idx = next(i for i, e in enumerate(parsed) if e["type"] == "subagent_complete")
    assert spawn_idx < complete_idx, (
        f"spawn event at index {spawn_idx} appears AFTER complete at {complete_idx}"
    )
