from __future__ import annotations

import threading
import subprocess
import json
from dataclasses import asdict, replace
from datetime import datetime, timezone
from pathlib import Path

import pytest

from fno.agents.outage_handoff import (
    ArchiveReceipt,
    EvidenceProof,
    HandoffDependencies,
    HandoffRequest,
    HandoffSnapshot,
    SourceRow,
    SpawnReceipt,
    StopProof,
    SuccessorProof,
    run_outage_handoff,
    production_handoff_dependencies,
    spawn_successor_exact,
    stop_source_exact,
    registry_entry_executable,
)
from fno.state.outage_handoff import (
    ManifestAuthority,
    ManifestArchiveCollision,
    ManifestPrepareError,
    archive_target_manifest,
    prepare_target_handoff,
)
from fno.claims.core import HolderMismatch
from fno.agents.provider_outage import OutageEvidence, OutagePolicy


def _request() -> HandoffRequest:
    return HandoffRequest(
        node="x-abcd",
        outage_epoch="epoch-1",
        source_row_id="source-row",
        source_provider="anthropic",
        source_account="claude-work",
        evidence_fingerprints=("fp-source", "fp-peer"),
        destination_harness="codex",
        destination_provider="openai",
        destination_model="gpt-5.6-sol",
        destination_account="work",
        destination_account_env={"CODEX_HOME": "/private/account"},
        quorum_evidence_count=2,
    )


def _snapshot(tmp_path: Path) -> HandoffSnapshot:
    return HandoffSnapshot(
        node="x-abcd",
        outage_epoch="epoch-1",
        source=SourceRow(
            row_id="source-row",
            name="worker-a",
            harness="claude",
            cwd=str(tmp_path),
            harness_session_id="session-a",
            mux={"session": "footnote", "pane_id": "7"},
        ),
        claim_holder="target-session:session-a",
        node_status="ready",
        plan_path=str(tmp_path / "plan.md"),
        owner_cwd=str(tmp_path),
        branch="feature/test",
        head="abc123",
        worktree_id="gitdir:/repo/.git/worktrees/test",
        manifest_hash="manifest-before",
    )


def _deps(tmp_path: Path, calls: list[str], *, stopped: bool = True) -> HandoffDependencies:
    lease_lock = threading.Lock()
    lease_holder: list[str] = []

    def acquire(_key: str, holder: str) -> bool:
        with lease_lock:
            if lease_holder:
                return False
            lease_holder.append(holder)
            calls.append(f"lease:{holder}")
            return True

    def release(_key: str, holder: str) -> None:
        with lease_lock:
            assert lease_holder == [holder]
            lease_holder.clear()
            calls.append("lease-release")

    return HandoffDependencies(
        acquire_dispatch=acquire,
        read_dispatch_holder=lambda _key: lease_holder[0] if lease_holder else None,
        refresh_dispatch=lambda _key, holder: lease_holder == [holder],
        release_dispatch=release,
        read_snapshot=lambda _request: _snapshot(tmp_path),
        revalidate_evidence=lambda _request, _snapshot: EvidenceProof(
            verified=True,
            evidence_count=2,
            reason="exact persisted and raw evidence match",
        ),
        stop_source=lambda _source: (
            calls.append("stop")
            or StopProof(
                confirmed_dead=stopped,
                kind="mux-pane",
                evidence_count=2 if stopped else 0,
                reason="pane absent" if stopped else "pane state unknown",
            )
        ),
        prepare_handoff=lambda _snapshot, attempt: (
            calls.append("archive")
            or calls.append("claim-release")
            or ArchiveReceipt(
                path=str(tmp_path / f"target-state-{attempt}.md"),
                content_hash="manifest-before",
            )
        ),
        spawn_successor=lambda _snapshot, _request: (
            calls.append("spawn")
            or SpawnReceipt(row_id="successor-row", name="worker-b")
        ),
        verify_successor=lambda _snapshot, _spawn: SuccessorProof(
            executable=True,
            same_cwd=True,
            same_branch=True,
            exact_claim=True,
            fresh_manifest=True,
            unique=True,
            evidence_count=6,
        ),
        stop_partial_successor=lambda _spawn: calls.append("stop-partial") or True,
    )


def test_ac6_con_lease_contention_names_live_holder_and_is_retryable(
    tmp_path: Path,
):
    calls: list[str] = []
    deps = _deps(tmp_path, calls)
    deps = replace(
        deps,
        acquire_dispatch=lambda _key, _holder: False,
        read_dispatch_holder=lambda _key: "outage-handoff:winner",
        read_snapshot=lambda _request: pytest.fail("contention read source snapshot"),
        stop_source=lambda _source: pytest.fail("contention stopped source"),
        prepare_handoff=lambda _snapshot, _attempt: pytest.fail("contention prepared"),
        spawn_successor=lambda _snapshot, _request: pytest.fail("contention spawned"),
    )

    result = run_outage_handoff(
        _request(), deps=deps, journal_root=tmp_path / "journal"
    )

    assert result.phase == "refused"
    assert result.failed_phase == "observed"
    assert "outage-handoff:winner" in result.reason
    assert result.counts == {"lease_contention": 1}
    assert not (tmp_path / "journal").exists()


def test_ac6_con_double_tick_has_one_successor_and_terminal_replay_is_inert(tmp_path: Path):
    calls: list[str] = []
    deps = _deps(tmp_path, calls)
    barrier = threading.Barrier(3)
    results = []

    def tick() -> None:
        barrier.wait()
        results.append(
            run_outage_handoff(_request(), deps=deps, journal_root=tmp_path / "journal")
        )

    threads = [threading.Thread(target=tick) for _ in range(2)]
    for thread in threads:
        thread.start()
    barrier.wait()
    for thread in threads:
        thread.join()

    assert [result.phase for result in results].count("committed") == 1
    assert [result.phase for result in results].count("refused") == 1
    assert calls.count("spawn") == 1
    assert len([call for call in calls if call.startswith("lease:")]) == 1

    before = list(calls)
    replay = run_outage_handoff(_request(), deps=deps, journal_root=tmp_path / "journal")
    assert replay.phase == "committed"
    assert replay.replayed is True
    assert calls == before


def test_ac7_err_unknown_source_stop_refuses_without_terminal_receipt(tmp_path: Path):
    calls: list[str] = []
    result = run_outage_handoff(
        _request(), deps=_deps(tmp_path, calls, stopped=False), journal_root=tmp_path / "journal"
    )

    assert result.phase == "refused"
    assert result.failed_phase == "source_stopped"
    assert result.counts["source_stop_evidence"] == 0
    assert "archive" not in calls
    assert "claim-release" not in calls
    assert "spawn" not in calls
    receipts = list((tmp_path / "journal").glob("*.json"))
    assert len(receipts) == 1
    assert json.loads(receipts[0].read_text())["phase"] != "parked"


def test_ac7_con_evidence_drift_under_lease_refuses_before_stop(tmp_path: Path):
    calls: list[str] = []
    deps = replace(
        _deps(tmp_path, calls),
        revalidate_evidence=lambda _request, _snapshot: EvidenceProof(
            verified=False,
            evidence_count=1,
            reason="breaker fingerprint drift",
        ),
    )

    result = run_outage_handoff(
        _request(), deps=deps, journal_root=tmp_path / "journal"
    )

    assert result.phase == "refused"
    assert result.failed_phase == "source_stopped"
    assert result.reason == "breaker fingerprint drift"
    assert result.counts["source_evidence"] == 1
    assert "stop" not in calls
    assert "archive" not in calls
    assert "claim-release" not in calls
    assert "spawn" not in calls


def test_ac7_con_production_revalidator_rereads_raw_and_persisted_evidence(
    tmp_path: Path,
):
    now = 1_787_100_000.0
    transcript = tmp_path / "source.jsonl"
    content = "API Error: Request rejected (429) fair usage policy"
    transcript.write_text(json.dumps({
        "type": "assistant",
        "timestamp": datetime.fromtimestamp(now, tz=timezone.utc).isoformat(),
        "isApiErrorMessage": True,
        "apiErrorStatus": 429,
        "message": {"role": "assistant", "content": content},
    }) + "\n", encoding="utf-8")
    evidence = OutageEvidence(
        source="transcript",
        observed_at=now,
        row_id="source-row",
        harness="claude",
        provider="anthropic",
        account="claude-work",
        role="assistant",
        raw_status=429,
        raw_kind="api_error",
        content=content,
    )
    journal = tmp_path / "provider-outages.json"
    journal.write_text(json.dumps({
        "evidence": [asdict(evidence)],
        "breakers": [{
            "provider": "anthropic",
            "account": "claude-work",
            "outage_epoch": "epoch-1",
            "fingerprints": [evidence.fingerprint],
        }],
    }), encoding="utf-8")
    request = replace(_request(), evidence_fingerprints=(evidence.fingerprint,))
    deps = production_handoff_dependencies(
        outage_evidence_path=journal,
        transcript_path_for=lambda _identity: transcript,
        now_s=lambda: now,
    )

    proof = deps.revalidate_evidence(request, _snapshot(tmp_path))

    assert proof.verified is True
    assert proof.evidence_count == 1


def test_ac7_con_production_revalidator_rereads_and_persists_exact_pane(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
):
    now = 1_787_100_000.0
    content = "API Error: Request rejected (429) fair usage policy"
    evidence = OutageEvidence(
        source="pane", observed_at=now, row_id="source-row", harness="agy",
        provider="anthropic", account="claude-work", role="assistant",
        raw_status=429, raw_kind="api_error", content=content, pane_id="19",
        persisted=True, snapshot_at=now,
    )
    journal = tmp_path / "provider-outages.json"
    journal.write_text(json.dumps({
        "evidence": [asdict(evidence)],
        "breakers": [{
            "provider": "anthropic", "account": "claude-work",
            "outage_epoch": "epoch-1", "fingerprints": [evidence.fingerprint],
        }],
    }), encoding="utf-8")
    request = replace(_request(), evidence_fingerprints=(evidence.fingerprint,))
    snapshot = replace(
        _snapshot(tmp_path),
        source=replace(
            _snapshot(tmp_path).source,
            harness="agy",
            harness_session_id=None,
            mux={"session": "stable", "pane_id": "19"},
        ),
    )
    from fno import paths

    state_root = tmp_path / "state"
    monkeypatch.setattr(paths, "state_dir", lambda: state_root)
    snapshot_dir = state_root / "recovery" / "provider-pane-revalidations"

    class PaneRead:
        returncode = 0
        stdout = content
        stderr = ""

    deps = production_handoff_dependencies(
        outage_evidence_path=journal,
        pane_runner=lambda cmd, **_kwargs: (
            PaneRead() if "stable" in cmd and "19" in cmd else pytest.fail(cmd)
        ),
        now_s=lambda: now,
    )

    proof = deps.revalidate_evidence(request, snapshot)

    assert proof.verified is True
    assert proof.sources == ("pane",)
    persisted = json.loads((snapshot_dir / f"{evidence.fingerprint}.json").read_text())
    assert persisted["row_id"] == "source-row"
    assert persisted["pane_id"] == "19"


def test_ac7_con_policy_controls_under_lease_pane_freshness(tmp_path: Path):
    now = 1000.0
    evidence = OutageEvidence(
        source="pane", observed_at=now - 2, row_id="source-row", harness="agy",
        provider="anthropic", account="claude-work", role="assistant",
        raw_status=429, raw_kind="api_error", content="API Error 429",
        pane_id="19", persisted=True, snapshot_at=now - 2,
    )
    journal = tmp_path / "provider-outages.json"
    journal.write_text(json.dumps({"evidence": [asdict(evidence)], "breakers": [{
        "provider": "anthropic", "account": "claude-work", "outage_epoch": "epoch-1",
        "fingerprints": [evidence.fingerprint],
    }]}), encoding="utf-8")
    request = replace(_request(), evidence_fingerprints=(evidence.fingerprint,))
    snapshot = replace(_snapshot(tmp_path), source=replace(
        _snapshot(tmp_path).source, harness="agy", harness_session_id=None,
        mux={"session": "stable", "pane_id": "19"},
    ))
    deps = production_handoff_dependencies(
        outage_evidence_path=journal, pane_read_fn=lambda *_args: "API Error 429",
        pane_snapshot_dir=tmp_path / "snapshots", now_s=lambda: now,
        policy=OutagePolicy(pane_freshness_s=1, evidence_freshness_s=1),
    )

    proof = deps.revalidate_evidence(request, snapshot)

    assert proof.verified is False
    assert "stale" in proof.reason


def test_ac7_con_exact_holder_change_after_stop_parks_before_archive(tmp_path: Path):
    calls: list[str] = []
    deps = _deps(tmp_path, calls)
    snapshots = iter([
        _snapshot(tmp_path),
        _snapshot(tmp_path),
        replace(_snapshot(tmp_path), claim_holder="target-session:someone-else"),
    ])
    deps = replace(deps, read_snapshot=lambda _request: next(snapshots))

    result = run_outage_handoff(_request(), deps=deps, journal_root=tmp_path / "journal")

    assert result.phase == "parked"
    assert result.failed_phase == "prepared"
    assert calls.index("stop") < len(calls)
    assert "archive" not in calls
    assert "claim-release" not in calls


def test_ac10_err_partial_successor_is_stopped_and_attempt_stays_unclaimed(tmp_path: Path):
    calls: list[str] = []
    deps = _deps(tmp_path, calls)
    deps = replace(
        deps,
        verify_successor=lambda _snapshot, _spawn: SuccessorProof(
            executable=True,
            same_cwd=True,
            same_branch=True,
            exact_claim=False,
            fresh_manifest=False,
            unique=True,
            evidence_count=3,
        ),
    )

    result = run_outage_handoff(_request(), deps=deps, journal_root=tmp_path / "journal")

    assert result.phase == "parked"
    assert result.failed_phase == "committed"
    assert calls.index("stop") < calls.index("archive") < calls.index("claim-release")
    assert calls.index("claim-release") < calls.index("spawn") < calls.index("stop-partial")
    assert calls.count("claim-release") == 1
    assert calls.count("spawn") == 1


def test_ac9_hp_recovery_outage_entrypoint_bypasses_legacy_redispatch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    from fno import recovery

    calls: list[str] = []
    monkeypatch.setattr(
        recovery,
        "_redispatch",
        lambda *_args, **_kwargs: pytest.fail("provider outage used legacy redispatch"),
    )

    result = recovery.recover_provider_outage(
        _request(), deps=_deps(tmp_path, calls), journal_root=tmp_path / "journal"
    )

    assert result.phase == "committed"
    assert calls.count("spawn") == 1


def _git(cwd: Path, *args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=cwd, text=True).strip()


def _linked_worktree(tmp_path: Path) -> tuple[Path, Path, str]:
    repo = tmp_path / "repo"
    worktree = tmp_path / "worktree"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.com"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=repo, check=True)
    (repo / "tracked.txt").write_text("base\n", encoding="utf-8")
    subprocess.run(["git", "add", "tracked.txt"], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-qm", "base"], cwd=repo, check=True)
    subprocess.run(["git", "worktree", "add", "-qb", "feature/test", str(worktree)], cwd=repo, check=True)
    head = _git(worktree, "rev-parse", "HEAD")
    return repo, worktree, head


def _write_manifest(worktree: Path, plan: Path, head: str) -> ManifestAuthority:
    state = worktree / ".fno" / "target-state.md"
    state.parent.mkdir()
    gitdir = str(Path(_git(worktree, "rev-parse", "--git-dir")).resolve())
    state.write_text(
        "---\n"
        f'plan_path: "{plan}"\n'
        f'owner_cwd: "{worktree}"\n'
        f'initial_head: "{head}"\n'
        "harness: claude\n"
        "harness_session_id: source-session\n"
        "---\n"
        "# Target Session State\n"
        'target_claim_key: "node:x-abcd"\n'
        'target_claim_holder: "target-session:source-session"\n'
        "graph_node_id: x-abcd\n",
        encoding="utf-8",
    )
    return ManifestAuthority(
        node="x-abcd",
        claim_holder="target-session:source-session",
        owner_cwd=str(worktree),
        plan_path=str(plan),
        branch="feature/test",
        head=head,
        worktree_id=gitdir,
        harness_session_id="source-session",
    )


def test_ac7_con_archive_validates_authority_and_preserves_linked_worktree(tmp_path: Path):
    _repo, worktree, head = _linked_worktree(tmp_path)
    plan = tmp_path / "plan.md"
    plan.write_text("plan\n", encoding="utf-8")
    authority = _write_manifest(worktree, plan, head)
    (worktree / "tracked.txt").write_text("dirty\n", encoding="utf-8")
    before_dirty = _git(worktree, "diff", "--", "tracked.txt")

    receipt = archive_target_manifest(worktree, "attempt-a", authority)

    assert receipt.path == str(plan) + ".artifacts/target-state-attempt-a.md"
    assert len(receipt.content_hash) == 64
    assert not (worktree / ".fno" / "target-state.md").exists()
    assert _git(worktree, "rev-parse", "HEAD") == head
    assert _git(worktree, "diff", "--", "tracked.txt") == before_dirty


def test_ac10_err_archive_collision_with_different_content_refuses(tmp_path: Path):
    _repo, worktree, head = _linked_worktree(tmp_path)
    plan = tmp_path / "plan.md"
    plan.write_text("plan\n", encoding="utf-8")
    authority = _write_manifest(worktree, plan, head)
    archive = Path(str(plan) + ".artifacts") / "target-state-attempt-a.md"
    archive.parent.mkdir()
    archive.write_text("different\n", encoding="utf-8")

    with pytest.raises(ManifestArchiveCollision):
        archive_target_manifest(worktree, "attempt-a", authority)

    assert (worktree / ".fno" / "target-state.md").exists()
    assert archive.read_text(encoding="utf-8") == "different\n"


def test_ac10_err_prepare_release_failure_restores_identical_manifest(tmp_path: Path):
    _repo, worktree, head = _linked_worktree(tmp_path)
    plan = tmp_path / "plan.md"
    plan.write_text("plan\n", encoding="utf-8")
    authority = _write_manifest(worktree, plan, head)
    state = worktree / ".fno" / "target-state.md"
    before = state.read_bytes()

    with pytest.raises(ManifestPrepareError, match="release failed") as exc_info:
        prepare_target_handoff(
            worktree,
            "attempt-a",
            authority,
            release_exact=lambda _key, _holder: (_ for _ in ()).throw(
                RuntimeError("release failed")
            ),
        )

    archive = Path(exc_info.value.archive_path)
    assert exc_info.value.restored is True
    assert state.read_bytes() == before
    assert archive.read_bytes() == before


def test_ac10_err_prepare_foreign_holder_restores_and_names_holder(tmp_path: Path):
    _repo, worktree, head = _linked_worktree(tmp_path)
    plan = tmp_path / "plan.md"
    plan.write_text("plan\n", encoding="utf-8")
    authority = _write_manifest(worktree, plan, head)

    with pytest.raises(ManifestPrepareError) as exc_info:
        prepare_target_handoff(
            worktree,
            "attempt-a",
            authority,
            release_exact=lambda key, holder: (_ for _ in ()).throw(
                HolderMismatch(expected=holder, actual="target-session:other", key=key)
            ),
        )

    assert exc_info.value.live_holder == "target-session:other"
    assert exc_info.value.restored is True
    assert (worktree / ".fno" / "target-state.md").exists()


def test_ac8_con_shell_delegates_prepare_to_python_owner():
    script = (
        Path(__file__).parents[2] / "skills" / "target" / "scripts" / "handoff.sh"
    ).read_text(encoding="utf-8")

    assert "-m fno.state.outage_handoff prepare" in script
    assert 'mv "$STATE_FILE" "$ARCHIVED_STATE"' not in script
    assert 'fno claim release "node:$NODE_ID"' not in script


def test_ac8_con_mux_stop_requires_exact_pane_absence():
    calls = []

    class Result:
        returncode = 0
        stdout = ""
        stderr = ""

    row = SourceRow(
        row_id="source-row",
        name="not-used-as-address",
        harness="codex",
        cwd="/worktree",
        mux={"session": "stable-session", "pane_id": "19"},
    )
    proof = stop_source_exact(
        row,
        runner=lambda cmd, **kwargs: calls.append(cmd) or Result(),
        pane_probe=lambda mux: None,
    )

    assert proof.confirmed_dead is False
    assert proof.evidence_count == 0
    assert calls[0][-6:] == [
        "mux", "pane", "kill", "--session", "stable-session", "19"
    ]


def test_ac8_con_process_stop_requires_pid_start_token_and_positive_dead_probe():
    signals = []
    row = SourceRow(
        row_id="source-row",
        name="worker",
        harness="opencode",
        cwd="/worktree",
        pid=4242,
        pid_start_time=111,
    )
    states = iter([True, True, False])

    proof = stop_source_exact(
        row,
        pid_probe=lambda pid, started: next(states),
        signal_process=lambda pid, sig: signals.append((pid, sig)),
        sleep=lambda _seconds: None,
    )

    assert proof.confirmed_dead is True
    assert proof.evidence_count >= 2
    assert signals and {pid for pid, _sig in signals} == {4242}


def test_ac9_hp_successor_spawn_uses_canonical_axes_and_no_merge(tmp_path: Path):
    commands = []

    class Result:
        returncode = 0
        stdout = '{"name":"successor","short_id":"","session_id":"child-row"}\n'
        stderr = ""

    receipt = spawn_successor_exact(
        _snapshot(tmp_path),
        _request(),
        runner=lambda cmd, **kwargs: commands.append((cmd, kwargs)) or Result(),
    )

    cmd, kwargs = commands[0]
    assert cmd[-1] == "/fno:target --no-merge x-abcd"
    assert cmd[cmd.index("--harness") + 1] == "codex"
    assert cmd[cmd.index("--substrate") + 1] == "pane"
    assert "--bounded-placement" in cmd
    assert cmd[cmd.index("--cwd") + 1] == str(tmp_path)
    assert cmd[cmd.index("--node") + 1] == "x-abcd"
    assert "--provider" not in cmd
    assert "--recorded-provider=openai" in cmd
    assert cmd[cmd.index("--model") + 1] == "gpt-5.6-sol"
    assert cmd[cmd.index("--dispatch-account") + 1] == "work"
    assert kwargs["env"]["TARGET_NO_MERGE"] == "1"
    assert receipt == SpawnReceipt(row_id="child-row", name="successor")


def test_ac11_ui_live_registry_status_is_not_executability_proof():
    entry = type("Entry", (), {
        "status": "live",
        "mux": {"session": "stable", "pane_id": "19"},
        "pid": None,
        "pid_start_time": None,
    })()

    assert registry_entry_executable(entry, pane_probe=lambda _mux: None) is False
    assert registry_entry_executable(entry, pane_probe=lambda _mux: False) is False
    assert registry_entry_executable(entry, pane_probe=lambda _mux: True) is True
