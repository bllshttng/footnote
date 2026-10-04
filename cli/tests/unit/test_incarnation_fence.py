"""Incarnation fence (x-eea5 1.3): a losing incarnation refuses outward actions."""
import os
import socket
import subprocess
import sys
from pathlib import Path

from fno.claims.incarnation import incarnation_fence_blocks, resolve_fence_session_uuid
from fno.claims.io import claim_path, serialize_claim
from fno.claims.types import Claim, now_ms


def _wire(monkeypatch, status, *, own_pid=None):
    monkeypatch.setattr("fno.claims.core.claim_status", lambda key, root=None: status)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: own_pid
    )
    # Ownership compares the claim's machine_id, not a raw gethostname().
    # Fixtures below carry machine_id="h" to match.
    monkeypatch.setattr("fno.claims.hostid.machine_id", lambda: "h")


def test_no_uuid_is_invisible():
    assert incarnation_fence_blocks(None) == (False, "")
    assert incarnation_fence_blocks("") == (False, "")


def test_free_claim_proceeds(monkeypatch):
    _wire(monkeypatch, {"state": "free"})
    assert incarnation_fence_blocks("uuid1") == (False, "")


def test_ours_proceeds(monkeypatch):
    # AC5-EDGE: the sole incarnation holding its own claim is never fenced.
    _wire(
        monkeypatch,
        {"state": "live", "holder": "me", "pid": 123, "host": "h", "machine_id": "h"},
        own_pid=123,
    )
    assert incarnation_fence_blocks("uuid1") == (False, "")


def test_other_live_blocks(monkeypatch):
    # AC3-ERR: another live incarnation holds the lineage claim -> refuse.
    _wire(
        monkeypatch,
        {"state": "live", "holder": "target-session:other", "pid": 999, "host": "h"},
        own_pid=123,
    )
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "session:uuid1" in reason
    assert "other" in reason


def test_unreadable_claims_fails_closed(monkeypatch):
    # AC4-FR: an unreadable claims dir refuses outward actions.
    def boom(key, root=None):
        raise RuntimeError("unreadable")

    monkeypatch.setattr("fno.claims.core.claim_status", boom)
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked and "unreadable" in reason


def test_stale_holder_proceeds(monkeypatch):
    # A dead/stale contender is no contention -> proceed.
    _wire(
        monkeypatch,
        {"state": "stale", "holder": "dead", "pid": 1, "host": "h"},
        own_pid=123,
    )
    assert incarnation_fence_blocks("uuid1") == (False, "")


def test_corrupted_claim_fails_closed(monkeypatch):
    # F3: claim_status returns state="corrupted" (no raise) for a malformed claim
    # file. An unverifiable single-writer state must fail closed, not read clear.
    _wire(monkeypatch, {"state": "corrupted", "error": "bad json"})
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "corrupt" in reason.lower()


def test_resolve_uuid_from_env(monkeypatch):
    # F1: the fence keys on the TRANSCRIPT uuid (CLAUDE_CODE_SESSION_ID), not the
    # target run id (TARGET_SESSION_ID); the single-writer claim is held under the
    # transcript uuid, so the run id would read a nonexistent key as clear.
    monkeypatch.setenv("TARGET_SESSION_ID", "run-id")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "transcript-uuid")
    assert resolve_fence_session_uuid() == "transcript-uuid"


def test_target_session_id_is_not_the_fence_key(monkeypatch, tmp_path):
    # F1: TARGET_SESSION_ID is the target run id, not the claim key. With no
    # transcript uuid resolvable the fence is invisible (None), never the run id.
    monkeypatch.setenv("TARGET_SESSION_ID", "run-id")
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    assert resolve_fence_session_uuid(tmp_path) is None


def test_resolve_uuid_from_manifest(tmp_path, monkeypatch):
    # F1: the manifest's transcript uuid (claude_session_id / harness_session_id)
    # is the claim key, not the run-id `session_id` field.
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        'session_id: "run-uuid"\n'
        'claude_session_id: "transcript-uuid"\n'
        'harness_session_id: "transcript-uuid"\n'
    )
    assert resolve_fence_session_uuid(tmp_path) == "transcript-uuid"


def test_resolve_manifest_run_id_only_is_none(tmp_path, monkeypatch):
    # F1: a manifest carrying only the run-id session_id yields None; the run id
    # is never the fence key.
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text('session_id: "run-uuid"\n')
    assert resolve_fence_session_uuid(tmp_path) is None


def test_run_merge_blocked_by_fence(monkeypatch, tmp_path):
    # The merge outward action refuses when the fence blocks (before any merge work).
    # run_merge's hold-check runs BEFORE the fence check; an empty graph_json
    # keeps it a no-op so this test exercises the fence, not leftover graph
    # state from another test in the same xdist worker (round-12 review fix).
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(
        "fno.pr._review_hold.resolve_pr_worktree", lambda _pr, repo: repo
    )
    monkeypatch.setattr(
        "fno.claims.incarnation.resolve_fence_session_uuid", lambda cwd=None: "uuid1"
    )
    monkeypatch.setattr(
        "fno.claims.incarnation.incarnation_fence_blocks",
        lambda u, **k: (True, "session:uuid1 held by other"),
    )
    from fno.pr import _merge

    rc = _merge.run_merge(["123"], cwd=str(tmp_path))
    assert rc == 2


# ---------------------------------------------------------------------------
# the 2026-08-21 specimen, replayed end to end against a real claim file
# ---------------------------------------------------------------------------


def _write_specimen_claim(root: Path, *, pid: int, provenance=None) -> None:
    """The PR 1031 shape: a 120-second session-writer claim, long expired."""
    started = now_ms() - 100
    claim = Claim(
        key="session:119e3c52-specimen",
        holder="119e3c52-specimen-holder",
        acquired_at=started,
        expires_at=now_ms() - 50,  # the 120s TTL lapsed ~700 minutes ago
        pid=pid,
        host=socket.gethostname(),
        pid_provenance=provenance,
    )
    path = claim_path(claim.key, root=root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(claim))


def test_specimen_expired_foreign_pid_no_longer_blocks(tmp_path):
    """THE SPECIMEN, end to end: a session-writer claim whose TTL expired
    ~700 minutes ago while a live foreign process (a chat app's app-server)
    answered for the recorded pid. Before the corroborated hybrid arm the
    claim read live forever and the fence blocked every merge on the
    lineage; now the unproven pid cannot outrank the TTL, the claim reads
    stale, and the fence clears - the merge path proceeds."""
    foreign = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    try:
        _write_specimen_claim(tmp_path, pid=foreign.pid, provenance="ambient")
        blocked, reason = incarnation_fence_blocks(
            "119e3c52-specimen", claims_root=tmp_path
        )
        assert blocked is False
        assert reason == ""
    finally:
        foreign.terminate()
        foreign.wait()


def test_specimen_inverted_prover_pid_still_blocks(tmp_path, monkeypatch):
    """The gate is not a blanket expiry-clears-all: the same expired claim
    with a pid the prover could have answered for (a genuinely suspended
    holder session) still reads live and still blocks."""
    monkeypatch.setattr(
        "fno.claims.incarnation._own_session_pid", lambda: None
    )  # we are not the holder
    _write_specimen_claim(
        tmp_path, pid=os.getpid(), provenance="session-prover"
    )
    blocked, reason = incarnation_fence_blocks(
        "119e3c52-specimen", claims_root=tmp_path
    )
    assert blocked is True
    assert "session:119e3c52-specimen" in reason


# ---------------------------------------------------------------------------
# the refusal names the process it found
# ---------------------------------------------------------------------------


def test_blocked_refusal_names_cmdline_and_uptime(monkeypatch):
    # Both specimen operators ran ps by hand to learn the blocker was a chat
    # app. The refusal now says so itself: cmdline and uptime of the pid.
    _wire(
        monkeypatch,
        {"state": "live", "holder": "target-session:other", "pid": os.getpid(),
         "host": "h", "machine_id": "h"},
        own_pid=123,
    )
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "python" in reason  # our own cmdline: the interpreter running pytest
    assert ", up " in reason


def test_blocked_refusal_names_an_unreadable_pid(monkeypatch):
    # A pid psutil cannot inspect renders <uninspectable>, not a crash and
    # not a cleared fence: the state still says a contender exists.
    _wire(
        monkeypatch,
        {"state": "live", "holder": "target-session:other", "pid": "garbage",
         "host": "h", "machine_id": "h"},
        own_pid=123,
    )
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "<uninspectable>" in reason


def test_blocked_refusal_names_a_dead_pid_honestly(monkeypatch):
    # A live-or-suspect verdict over a pid that no longer exists (the suspect
    # arm's normal shape) still blocks, and says there is no such process.
    _wire(
        monkeypatch,
        {"state": "suspect", "holder": "target-session:other", "pid": 999_999_999,
         "host": "h", "machine_id": "h"},
        own_pid=123,
    )
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "no such process" in reason


# ---------------------------------------------------------------------------
# pool-machinery carve-out (the bg-spare hosts the worker's own lineage)
# ---------------------------------------------------------------------------


def _fake_psutil_process(monkeypatch, argv):
    """Replace psutil.Process with one whose cmdline() answers ARGV (or raises
    the class set on .raise_)."""
    import psutil

    class _FakeProc:
        raise_ = None

        def __init__(self, pid):
            self.pid = pid

        def cmdline(self):
            if self.raise_ is not None:
                raise self.raise_(self.pid)
            return list(argv)

    monkeypatch.setattr(psutil, "Process", _FakeProc)
    return _FakeProc


def test_helper_matches_the_measured_pool_shapes(monkeypatch):
    # The three measured argv shapes from spawn_context.rs pool_machinery
    # tests: a bg-spare, its pty host, and the daemon after the claude binary.
    from fno.claims.incarnation import _process_is_pool_machinery

    _fake_psutil_process(
        monkeypatch,
        ["claude", "bg-spare", "--bg-spare", "/tmp/cc/spare/6cd.sock"],
    )
    assert _process_is_pool_machinery(82264) is True
    _fake_psutil_process(
        monkeypatch,
        [
            "claude", "bg-pty-host", "--bg-pty-host", "/tmp/cc/spare/1ec.pty.sock",
            "200", "50", "--", "--bg-spare", "/tmp/cc/spare",
        ],
    )
    assert _process_is_pool_machinery(82265) is True
    _fake_psutil_process(
        monkeypatch,
        ["/Users/bb16/.local/bin/claude", "daemon", "run", "--origin", "transient"],
    )
    assert _process_is_pool_machinery(82266) is True


def test_helper_rejects_real_sessions_and_non_pids(monkeypatch):
    # A real session's argv carries none of the tokens; `daemon` counts only
    # after a claude binary; every unprovable input fails closed so the fence
    # keeps blocking.
    from fno.claims.incarnation import _process_is_pool_machinery

    import psutil

    _fake_psutil_process(
        monkeypatch,
        ["/versions/2.1.0", "--resume", "/tmp/s/abc123.jsonl", "--name", "w"],
    )
    assert _process_is_pool_machinery(900) is False
    _fake_psutil_process(monkeypatch, ["fno", "daemon", "run"])
    assert _process_is_pool_machinery(901) is False
    _fake_psutil_process(monkeypatch, [])
    assert _process_is_pool_machinery(902) is False
    fake = _fake_psutil_process(monkeypatch, ["claude", "bg-spare", "--bg-spare", "x"])
    fake.raise_ = psutil.NoSuchProcess
    assert _process_is_pool_machinery(903) is False
    for junk in ("garbage", None, True):
        assert _process_is_pool_machinery(junk) is False


def test_machinery_holder_on_own_machine_proceeds(monkeypatch):
    # THE BUG: the session claim is pinned to the bg-spare that hosts this
    # very worker. Pool machinery is not a rival incarnation; the fence
    # proceeds (the claim itself still gates real adopters).
    _wire(
        monkeypatch,
        {"state": "live", "holder": "revive:64225", "pid": 999_999,
         "host": "h", "machine_id": "h"},
        own_pid=None,  # a thread worker: the walk refuses spares, so no own pid
    )
    monkeypatch.setattr(
        "fno.claims.incarnation._process_is_pool_machinery", lambda pid: True
    )
    assert incarnation_fence_blocks("uuid1") == (False, "")


def test_real_holder_still_blocks_and_names_the_release_verb(monkeypatch):
    # A same-machine holder that is NOT machinery still blocks, and the
    # refusal now carries the administrative release verb so an operator can
    # act without reading source.
    _wire(
        monkeypatch,
        {"state": "live", "holder": "revive:64225", "pid": 999_999,
         "host": "h", "machine_id": "h"},
        own_pid=123,
    )
    monkeypatch.setattr(
        "fno.claims.incarnation._process_is_pool_machinery", lambda pid: False
    )
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "fno agents claim release session:uuid1 --force --reason" in reason


def test_remote_machinery_holder_still_blocks(monkeypatch):
    # A spare-shaped holder on ANOTHER machine cannot be inspected, so the
    # same-machine guard short-circuits before the helper and the fence
    # blocks (fail closed).
    _wire(
        monkeypatch,
        {"state": "live", "holder": "revive:other", "pid": 999_999,
         "host": "remote", "machine_id": "other"},
        own_pid=123,
    )
    monkeypatch.setattr(
        "fno.claims.incarnation._process_is_pool_machinery", lambda pid: True
    )
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "no such process" in reason
