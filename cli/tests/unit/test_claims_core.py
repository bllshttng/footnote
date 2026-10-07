"""Unit tests for fno.claims.core: the six verbs.

Tests are organized by verb. Each verb exercises the design-doc ACs:
HP (happy path), ERR (error path), EDGE (edge case), FR (functional req).

Filesystem isolation: every test uses a tmp_path root via the ``root``
argument supported by every verb. Events emission goes to .fno/events.jsonl
which the typed-builders write best-effort; tests focus on lock-file state
and exceptions, not on event log content (the event types are covered by
the parity corpus and test_validator_parity.py).
"""
from __future__ import annotations

import os
import socket
import subprocess
import sys
import time
from pathlib import Path

import psutil
import pytest

from fno.rust_binary import find_dev_binary

from fno.claims.core import (
    ClaimGoneAway,
    ClaimHeldByOther,
    ClaimValidationError,
    HolderMismatch,
    acquire_claim,
    claim_status,
    compare_and_rebind,
    force_release_claim,
    list_claims,
    refresh_claim,
    release_claim,
)
from fno.claims.io import claim_path, serialize_claim
from fno.claims.types import Claim, ClaimState, now_ms
from tests._table_seed import read_claim_row, update_claim


HOLDER_A = "target-session:sid-a"
HOLDER_B = "target-session:sid-b"


# ---------------------------------------------------------------------------
# acquire
# ---------------------------------------------------------------------------


class TestAcquire:
    def test_AC1_FR_ttl_sets_expires_at(self, tmp_path):
        claim = acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        assert claim.expires_at is not None
        assert claim.expires_at > claim.acquired_at

    def test_AC3_ERR_pid_unavailable_requires_ttl(self, tmp_path):
        with pytest.raises(ClaimValidationError, match="TTL"):
            acquire_claim("k", HOLDER_A, pid_unavailable=True, root=tmp_path)

    def test_AC1_HP_release_then_reacquire_mints_new_holder(self, tmp_path):
        first = acquire_claim(
            "node:handoff", HOLDER_A, ttl_ms=60_000, pid_unavailable=True, root=tmp_path
        )
        assert release_claim("node:handoff", HOLDER_A, root=tmp_path) is not None
        second = acquire_claim(
            "node:handoff", HOLDER_B, ttl_ms=60_000, pid_unavailable=True, root=tmp_path
        )
        assert first.holder != second.holder
        assert second.holder == HOLDER_B

    def test_AC1_ERR_key_too_long_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("x" * 300, HOLDER_A, root=tmp_path)

    def test_AC1_ERR_ttl_below_min_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("k", HOLDER_A, ttl_ms=100, root=tmp_path)

    def test_AC1_ERR_ttl_above_max_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("k", HOLDER_A, ttl_ms=86_400_001, root=tmp_path)

    def test_AC1_ERR_empty_holder_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            acquire_claim("k", "", root=tmp_path)

    def test_AC1_EDGE_live_other_raises(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        with pytest.raises(ClaimHeldByOther) as exc:
            acquire_claim("k", HOLDER_B, root=tmp_path)
        assert exc.value.holder == HOLDER_A
        assert exc.value.key == "k"

    def test_AC1_FR_idempotent_reacquire_same_holder(self, tmp_path):
        first = acquire_claim("k", HOLDER_A, root=tmp_path)
        # Same holder, second call must succeed (not raise).
        second = acquire_claim("k", HOLDER_A, root=tmp_path)
        assert second.holder == HOLDER_A
        # acquired_at is refreshed
        assert second.acquired_at >= first.acquired_at

    def test_AC1_FR_ttl_expired_recovered(self, tmp_path):
        """A TTL claim past expires_at whose pid is dead is reclaimable.

        The recorded pid must be dead: under the hybrid liveness arm an
        expired TTL claim whose pid is still ALIVE on this host stays LIVE
        and is NOT reclaimable (see test_hybrid_expired_live_pid_not_reclaimable)."""
        from fno.claims.types import now_ms
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        expired = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 200_000,
            expires_at=now_ms() - 100_000,
            pid=dead_pid,
            host=socket.gethostname(),
        )
        path.write_text(serialize_claim(expired))

        new = acquire_claim("k", HOLDER_B, root=tmp_path)
        assert new.holder == HOLDER_B

    def test_expired_live_ambient_pid_is_reclaimable(self, tmp_path):
        """THE SPECIMEN'S ACQUIRE SIDE: the same expired claim WITHOUT
        provenance (a foreign process merely answers for the pid) IS
        reclaimable. A live pid that was never proven to be the holder
        session's own process cannot outrank the TTL, or the lease is not a
        lease - this is what unblocks a fenced merge peer."""
        proc_create_ms = int(psutil.Process(os.getpid()).create_time() * 1000)
        # now-relative, not create-relative: acquired 100ms ago (after proc
        # start, so the pid-reuse guard passes) and expired 50ms ago. A
        # create-relative expiry is in the future on a fast runner and the
        # claim lands in the unexpired arm for an unrelated reason.
        assert now_ms() - 100 > proc_create_ms
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        expired_foreign = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 100,
            expires_at=now_ms() - 50,
            pid=os.getpid(),
            host=socket.gethostname(),
            pid_provenance="ambient",
        )
        path.write_text(serialize_claim(expired_foreign))

        new = acquire_claim("k", HOLDER_B, root=tmp_path)
        assert new.holder == HOLDER_B


# ---------------------------------------------------------------------------
# pid provenance stamping: every writer earns its field or says ambient
# ---------------------------------------------------------------------------


class TestPidProvenanceStamping:
    """The corroborated hybrid arm is only as honest as the stamp, so the
    stamp is earned centrally at write time against the process-tree prover -
    never asserted by a writer that merely had a pid lying around."""


    def test_foreign_live_pid_stamps_ambient(self, tmp_path):
        """THE SPECIMEN WRITER SHAPE: a reattach resolved its incarnation
        through an ambient codex process tree and recorded a foreign live pid
        (a chat app's app-server). The pid is real and alive, but it is not
        the prover's answer for this session, so the stamp must say ambient -
        the claim then expires on its TTL instead of reading live forever."""
        foreign = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        try:
            assert psutil.pid_exists(foreign.pid)
            claim = acquire_claim(
                "session:uuid-reattach", HOLDER_A, ttl_ms=120_000,
                pid=foreign.pid, root=tmp_path,
            )
            assert claim.pid == foreign.pid
            assert claim.pid_provenance == "ambient"
        finally:
            foreign.terminate()
            foreign.wait()

    def test_specimen_shape_ambient_codex_marker_does_not_launder_provenance(
        self, tmp_path, monkeypatch
    ):
        """The specimen's enabling condition: an inherited CODEX marker in the
        environment of a process that is not codex. Even with the marker set,
        provenance is decided by the process tree, not the ambient id, so a
        foreign pid still stamps ambient and the TTL stays a lease."""
        monkeypatch.setenv("CODEX_THREAD_ID", "01a02125-ambient-foreign")
        foreign = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        try:
            claim = acquire_claim(
                "session:uuid-reattach2", HOLDER_A, ttl_ms=120_000,
                pid=foreign.pid, root=tmp_path,
            )
            assert claim.pid_provenance == "ambient"
        finally:
            foreign.terminate()
            foreign.wait()

    def test_no_pid_ttl_claim_stamps_ambient_without_walking(self, tmp_path, monkeypatch):
        """A defaulted pid is the transient acquiring subprocess: ambient, and
        no process walk is paid for it (provenance is only read on TTL claims,
        but a transient pid can never earn session-proven anyway)."""
        def _boom(**_kw):
            raise AssertionError("no walk should run for a defaulted pid")

        monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid", _boom)
        claim = acquire_claim("node:x-2", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        assert claim.pid_provenance == "ambient"

    def test_pid_liveness_claim_skips_the_walk(self, tmp_path, monkeypatch):
        """PID-liveness claims never reach the expired-TTL arm, so provenance
        is never consulted; the walk is skipped entirely."""
        def _boom(**_kw):
            raise AssertionError("no walk should run for a PID-liveness claim")

        monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid", _boom)
        claim = acquire_claim(
            "node:x-3", HOLDER_A, pid=os.getpid(), root=tmp_path
        )
        assert claim.expires_at is None
        assert claim.pid_provenance == "ambient"

    def test_explicit_provenance_from_a_caller_that_did_its_own_proving(self, tmp_path):
        """The escape hatch: a caller that holds its own positive proof stamps
        the field itself; the central resolution defers to it verbatim."""
        claim = acquire_claim(
            "node:x-4", HOLDER_A, ttl_ms=60_000, pid=424242,
            pid_provenance="session-prover", root=tmp_path,
        )
        assert claim.pid_provenance == "session-prover"

    def test_rebind_stamp_and_stored_harness_never_disagree(self, tmp_path, monkeypatch):
        """A rebind that hands a claim to a SHARED-HOST harness must not stamp
        session-prover, even when the rebinding process's own harness forks per
        session. The stamp is earned against the harness the record will carry,
        never against the walker's - otherwise a record contradicts itself
        about which harness wrote it, which is the state the Rust make_claim
        hoists one resolve_harness() call to make impossible."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        handover = Claim(
            key="node:x-7", holder="spawn-handover:bp-x7",
            acquired_at=now_ms(), expires_at=now_ms() + 900_000,
            pid=999_999_999, host=socket.gethostname(), pid_provenance="ambient",
        )
        path = claim_path("node:x-7", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(handover))

        claim, mode = compare_and_rebind(
            "node:x-7", "spawn-handover:bp-x7",
            new_holder=HOLDER_A, new_pid=os.getpid(), ttl_ms=3_600_000,
            new_harness="codex", root=tmp_path,
        )
        assert mode == "handover"
        assert claim.harness == "codex"
        assert claim.pid_provenance == "ambient"

    def test_shared_host_harness_never_earns_the_prover_stamp(self, tmp_path, monkeypatch):
        """AC1 - the writer side of the specimen. The prover walk succeeds:
        the pid IS resolve_session_pid's answer, and both sides of that
        equality hold. Under codex they hold for the WRONG reason, because the
        answer is a shared `codex app-server` that hosts every session on the
        machine. Proving which process the pid is never proves that process
        dies with the session, so the stamp is refused and the TTL stays the
        lease it claims to be."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        claim = acquire_claim(
            "node:x-shared", HOLDER_A, ttl_ms=60_000, pid=os.getpid(),
            harness="codex", root=tmp_path
        )
        assert claim.pid_provenance == "ambient"
        # The stamp is gated on the harness the record STORES, so the two can
        # never disagree about which harness wrote it.
        assert claim.harness == "codex"


    def test_refresh_under_shared_host_harness_does_not_repoison(self, tmp_path, monkeypatch):
        """AC5-EDGE - refresh_claim re-derives the stamp instead of asserting
        it. Its old hardcode rested on 'the anchor IS the prover's answer',
        which is true and insufficient: under codex that answer is the
        multiplexer. Left hardcoded, every renewal would re-poison the record
        acquire had just stopped poisoning, and a refreshed lease would be
        permanent again."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        first = acquire_claim(
            "node:x-refresh-shared", HOLDER_A, ttl_ms=60_000, pid=os.getpid(),
            harness="codex", root=tmp_path
        )
        assert first.pid_provenance == "ambient"
        refreshed = refresh_claim(
            "node:x-refresh-shared", HOLDER_A, ttl_ms=60_000, root=tmp_path
        )
        assert refreshed is not None
        assert refreshed.pid_provenance == "ambient"


# ---------------------------------------------------------------------------
# release
# ---------------------------------------------------------------------------


class TestRelease:
    def test_AC2_HP_release_removes_lock_file(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        release_claim("k", HOLDER_A, root=tmp_path)
        assert not claim_path("k", root=tmp_path).exists()

    def test_AC2_HP_release_missing_is_idempotent(self, tmp_path):
        # No claim filed; release must succeed.
        release_claim("k", HOLDER_A, root=tmp_path)

    def test_AC2_ERR_release_strict_raises_on_mismatch(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        with pytest.raises(HolderMismatch):
            release_claim("k", HOLDER_B, strict=True, root=tmp_path)

    def test_AC2_ERR_empty_key_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            release_claim("", HOLDER_A, root=tmp_path)

# ---------------------------------------------------------------------------
# refresh
# ---------------------------------------------------------------------------


class TestRefresh:
    def test_AC3_HP_refresh_extends_expires_at(self, tmp_path):
        first = acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        # Sleep to ensure now_ms() advances.
        time.sleep(0.01)
        refreshed = refresh_claim("k", HOLDER_A, ttl_ms=120_000, root=tmp_path)
        assert refreshed is not None
        assert refreshed.expires_at > first.expires_at

    def test_AC3_FR_refresh_pid_liveness_returns_none(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)  # no TTL
        result = refresh_claim("k", HOLDER_A, root=tmp_path)
        assert result is None

    def test_AC3_ERR_refresh_missing_raises_gone_away(self, tmp_path):
        with pytest.raises(ClaimGoneAway):
            refresh_claim("k", HOLDER_A, root=tmp_path)

    def test_AC3_ERR_refresh_wrong_holder_raises(self, tmp_path):
        acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        with pytest.raises(HolderMismatch):
            refresh_claim("k", HOLDER_B, root=tmp_path)

    def test_AC3_ERR_refresh_ttl_out_of_range(self, tmp_path):
        acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        with pytest.raises(ClaimValidationError):
            refresh_claim("k", HOLDER_A, ttl_ms=10, root=tmp_path)

    @staticmethod
    def _dead_pid() -> int:
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        return dead_pid


# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------


class TestStatus:
    def test_AC4_HP_status_free(self, tmp_path):
        result = claim_status("k", root=tmp_path)
        assert result["state"] == ClaimState.FREE.value
        assert result["key"] == "k"

    def test_AC4_HP_status_live(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        result = claim_status("k", root=tmp_path)
        assert result["state"] == ClaimState.LIVE.value
        assert result["holder"] == HOLDER_A
        assert result["pid"] == os.getpid()

    def test_status_carries_basis_and_offhost_differs_from_pid_reuse(
        self, tmp_path
    ):
        """Both claims read stale, but the basis tells the causes apart
        without a source read - the whole point of verdict-beside-basis."""
        offhost = Claim(
            key="k-offhost",
            holder=HOLDER_A,
            acquired_at=now_ms(),
            pid=os.getpid(),
            host="somewhere-else.invalid",
            machine_id="some-other-machine-0000",
        )
        path = claim_path("k-offhost", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(offhost))

        reused = Claim(
            key="k-reuse",
            holder=HOLDER_A,
            acquired_at=0,
            pid=os.getpid(),
            host=socket.gethostname(),
        )
        path = claim_path("k-reuse", root=tmp_path)
        path.write_text(serialize_claim(reused))

        a = claim_status("k-offhost", root=tmp_path)
        b = claim_status("k-reuse", root=tmp_path)
        assert a["state"] == ClaimState.STALE.value
        assert b["state"] == ClaimState.STALE.value
        assert a["basis"] == "offhost"
        assert b["basis"] == "pid-reuse"
        assert a["basis"] != b["basis"]

    def test_status_live_basis_and_free_have_no_basis(self, tmp_path):
        acquire_claim("k-live", HOLDER_A, root=tmp_path)
        result = claim_status("k-live", root=tmp_path)
        assert result["basis"] == "live"
        free = claim_status("never-existed", root=tmp_path)
        assert "basis" not in free

    def test_rootless_node_key_routes_to_global_root(self, tmp_path, monkeypatch):
        """A rootless read of node:<id> must answer the global root, not the
        repo space (x-74aa): the repo space holds no node locks, so the
        pre-fix default read every live node claim as free."""
        monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
        monkeypatch.setattr("fno.paths.space_dir", lambda: tmp_path / "space")
        acquire_claim("node:ab-1234", HOLDER_A, root=None)
        # Control: the repo space really is the wrong tree for this key.
        assert claim_status("node:ab-1234", root=tmp_path / "space")["state"] == (
            ClaimState.FREE.value
        )
        result = claim_status("node:ab-1234")
        assert result["state"] == ClaimState.LIVE.value
        assert result["holder"] == HOLDER_A

    def test_explicit_root_still_wins_over_key_routing(self, tmp_path, monkeypatch):
        monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
        acquire_claim("node:ab-1234", HOLDER_A, root=tmp_path / "explicit")
        result = claim_status("node:ab-1234", root=tmp_path / "explicit")
        assert result["state"] == ClaimState.LIVE.value
        assert result["holder"] == HOLDER_A

    def test_node_shaped_colonless_key_is_unknown_not_free(self):
        result = claim_status("ab-1234")
        assert result["state"] == "unknown"
        assert result["basis"] == "key-unrouted"
        assert "node:ab-1234" in result["detail"]
        assert result["state"] != ClaimState.FREE.value

    def test_non_node_colonless_key_keeps_repo_space_default(self, tmp_path):
        free = claim_status("some-repo-token", root=tmp_path)
        assert free["state"] == ClaimState.FREE.value
        walker = claim_status("walker:/tmp/repo", root=tmp_path)
        assert walker["state"] == ClaimState.FREE.value


# ---------------------------------------------------------------------------
# list
# ---------------------------------------------------------------------------


class TestList:
    def test_AC5_HP_list_returns_live_claims(self, tmp_path):
        acquire_claim("node:ab-1", HOLDER_A, root=tmp_path)
        acquire_claim("node:ab-2", HOLDER_A, root=tmp_path)
        results = list_claims(root=tmp_path)
        keys = sorted(r["key"] for r in results)
        assert keys == ["node:ab-1", "node:ab-2"]

    def test_AC5_FR_list_filters_by_prefix(self, tmp_path):
        acquire_claim("node:ab-1", HOLDER_A, root=tmp_path)
        acquire_claim("fleet:m1", HOLDER_A, root=tmp_path)
        results = list_claims(prefix="node:", root=tmp_path)
        assert [r["key"] for r in results] == ["node:ab-1"]

# ---------------------------------------------------------------------------
# force-release
# ---------------------------------------------------------------------------


class TestForceRelease:
    def test_AC6_HP_force_release_removes_live_claim(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        force_release_claim("k", reason="operator override", root=tmp_path)
        assert not claim_path("k", root=tmp_path).exists()

    def test_AC6_HP_force_release_missing_succeeds(self, tmp_path):
        force_release_claim("k", reason="cleanup", root=tmp_path)

    def test_AC6_ERR_empty_reason_rejected(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        with pytest.raises(ClaimValidationError):
            force_release_claim("k", reason="", root=tmp_path)

# ---------------------------------------------------------------------------
# session-keyed liveness (x-a613): the session id is the witness
# ---------------------------------------------------------------------------


def _dev_native_binary() -> str:
    """The worktree-built fno-agents: the session witness exists only here,
    and the installed binary the default resolver finds predates it."""
    return str(
        Path(__file__).resolve().parents[3] / "crates/fno-agents/target/debug/fno-agents"
    )


def _dead_pid_context():
    """A pid that is genuinely absent, plus proof it was alive once."""
    dead = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    pid = dead.pid
    dead.terminate()
    dead.wait()
    return pid


class TestSessionIdStamping:
    """The session id rides the claim record so classification and renewal can
    resolve the holder through the registry row keyed by it, never by parsing
    the published holder string."""

RUST_BIN = find_dev_binary()
requires_rust = pytest.mark.skipif(
    RUST_BIN is None,
    reason="compiled fno-agents binary not present (the smoke pytest shard deletes it)",
)


class TestSessionWitnessVerdicts:
    """The witness heals a live session's verdict and bounds the unknown.
    These shell the NATIVE classifier, pinned to the worktree build."""

    def _write_claim(self, tmp_path, key, session_id, pid, expires_delta):
        path = claim_path(key, root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        rec = Claim(
            key=key,
            holder=HOLDER_A,
            acquired_at=now_ms() - 100,
            expires_at=now_ms() + expires_delta,
            pid=pid,
            host=socket.gethostname(),
            pid_provenance="session-prover",
            session_id=session_id,
        )
        path.write_text(serialize_claim(rec))
        return rec

    def _pin_native_binary(self, monkeypatch):
        monkeypatch.setenv("FNO_AGENTS_BIN", _dev_native_binary())

    def _registry_home_with_live_row(self, tmp_path, session_id):
        """A registry the NATIVE binary reads: FNO_AGENTS_HOME tmp + row pid +
        start time in the convention daemon.process_start_time compares."""
        import json as _json

        home = tmp_path / "agents-home"
        home.mkdir(parents=True, exist_ok=True)
        if sys.platform == "darwin":
            start = int(psutil.Process(os.getpid()).create_time() * 1_000_000)
        else:
            with open(f"/proc/{os.getpid()}/stat") as fh:
                stat = fh.read().rsplit(")", 1)[1].split()
            start = int(stat[19])
        row = {
            "name": "w",
            "cwd": str(tmp_path),
            "harness": "claude",
            "harness_session_id": session_id,
            "pid": os.getpid(),
            "pid_start_time": start,
            "status": "busy",
            "created_at": "2026-09-06T00:00:00Z",
        }
        (home / "registry.json").write_text(
            _json.dumps({"schema_version": 24, "agents": [row]})
        )
        return home

    @requires_rust
    def test_status_live_session_never_reads_stale(self, tmp_path, monkeypatch):
        """AC3 + x-0c29: an EXPIRED claim whose session's registry row is live
        reads LIVE with the registry basis - a session that wrote seconds ago
        must never be provably dead."""
        self._pin_native_binary(monkeypatch)
        dead_pid = _dead_pid_context()
        self._write_claim(tmp_path, "k", "ses_live", dead_pid, -60_000)
        monkeypatch.setenv(
            "FNO_AGENTS_HOME", str(self._registry_home_with_live_row(tmp_path, "ses_live"))
        )
        status = claim_status("k", root=tmp_path)
        assert status["state"] == "live", status
        assert status["basis"] == "registry-session-live"
        assert status["session_basis"] == "registry-session-live"

    @requires_rust
    def test_status_unresolved_names_the_witness(self, tmp_path, monkeypatch):
        """AC5: expired, no registry row, no transcript -> the verdict is
        bounded (Suspect inside the grace) and the payload names the session
        witness unresolved, so a reader can see WHICH leg failed."""
        self._pin_native_binary(monkeypatch)
        dead_pid = _dead_pid_context()
        self._write_claim(tmp_path, "k", "ses_ghost", dead_pid, -60_000)
        home = tmp_path / "empty-agents-home"
        home.mkdir(parents=True, exist_ok=True)
        monkeypatch.setenv("FNO_AGENTS_HOME", str(home))
        status = claim_status("k", root=tmp_path)
        assert status["state"] == "suspect", status
        assert status["basis"] == "ttl-expired-unresolved"
        assert status["session_basis"] == "unresolved"

    @requires_rust
    def test_status_without_session_id_keeps_legacy_stale(self, tmp_path, monkeypatch):
        """AC4: a pre-change claim (no session id) reads Stale on expiry - the
        reaper behavior the 1511 revert restored, unchanged."""
        self._pin_native_binary(monkeypatch)
        dead_pid = _dead_pid_context()
        self._write_claim(tmp_path, "k", None, dead_pid, -60_000)
        status = claim_status("k", root=tmp_path)
        assert status["state"] == "stale", status
        assert "session_basis" not in status

    def test_refresh_without_session_id_leaves_the_anchor_alone(self, tmp_path, monkeypatch):
        """AC7: no session id -> the legacy create-time filter still governs,
        and with no resolvable ancestor the pid is left exactly as found."""
        dead_pid = _dead_pid_context()
        rec = self._write_claim(tmp_path, "k", None, dead_pid, 600_000)
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: None
        )
        refreshed = refresh_claim("k", HOLDER_A, ttl_ms=600_000, root=tmp_path)
        assert refreshed.pid == dead_pid
        assert refreshed.acquired_at == rec.acquired_at


# ---------------------------------------------------------------------------
# Contracts ported from the lockfile era: each seeds through the public verbs
# and ages the row in the claims table instead of hand-writing a lockfile.
# ---------------------------------------------------------------------------


def _gone_pid() -> int:
    pid = 999_999
    while psutil.pid_exists(pid):
        pid += 1
    return pid


def test_AC1_HP_fresh_key(tmp_path):
    claim = acquire_claim("node:ab-1", HOLDER_A, root=tmp_path)
    assert claim.holder == HOLDER_A
    assert claim_status("node:ab-1", root=tmp_path)["holder"] == HOLDER_A


def test_AC1_FR_pid_liveness_omits_expires_at(tmp_path):
    claim = acquire_claim("k", HOLDER_A, root=tmp_path)
    assert claim.expires_at is None
    assert read_claim_row("k", root=tmp_path)["expires_at"] is None


def test_AC3_HP_ttl_pid_unavailable_is_explicit(tmp_path):
    claim = acquire_claim("k", HOLDER_A, ttl_ms=60_000, pid_unavailable=True, root=tmp_path)
    assert claim.pid is None
    assert claim.pid_unavailable is True
    assert claim.schema_version == 2
    row = read_claim_row("k", root=tmp_path)
    assert row["pid"] is None
    assert row["pid_unavailable"]
    assert claim_status("k", root=tmp_path)["pid_unavailable"] is True


def test_AC4_EDGE_stale_pid_recovered(tmp_path):
    """A claim whose holder process is dead is reclaimable by another holder."""
    acquire_claim("k", HOLDER_A, root=tmp_path)
    update_claim("k", root=tmp_path, pid=_gone_pid(), acquired_at=now_ms() - 100_000)
    new = acquire_claim("k", HOLDER_B, root=tmp_path)
    assert new.holder == HOLDER_B
    assert claim_status("k", root=tmp_path)["holder"] == HOLDER_B


def test_hybrid_expired_live_pid_not_reclaimable(tmp_path):
    """An expired TTL claim whose pid is a live prover-proven process stays
    held: acquire honors the same hybrid liveness as status."""
    proc_create_ms = int(psutil.Process(os.getpid()).create_time() * 1000)
    assert now_ms() - 100 > proc_create_ms
    acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
    update_claim(
        "k", root=tmp_path, acquired_at=now_ms() - 100, expires_at=now_ms() - 50,
        pid=os.getpid(), pid_provenance="session-prover", session_id=None,
    )
    with pytest.raises(ClaimHeldByOther) as exc:
        acquire_claim("k", HOLDER_B, root=tmp_path)
    assert exc.value.holder == HOLDER_A
    assert claim_status("k", root=tmp_path)["holder"] == HOLDER_A


def test_AC2_FR_release_silently_skips_other_holder(tmp_path):
    acquire_claim("k", HOLDER_A, root=tmp_path)
    release_claim("k", HOLDER_B, root=tmp_path)
    assert claim_status("k", root=tmp_path)["holder"] == HOLDER_A


def test_strict_release_by_the_owner_frees_the_key(tmp_path):
    acquire_claim("node:x-abcd", HOLDER_A, root=tmp_path)
    release_claim("node:x-abcd", HOLDER_A, strict=True, root=tmp_path)
    assert claim_status("node:x-abcd", root=tmp_path)["state"] == "free"


def test_AC2_HP_refresh_extends_expired_claim_whose_holder_reads_live(tmp_path):
    """TTL expiry alone never refuses: an expired claim whose verdict reads
    live extends, exactly as `claim status` reports."""
    acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
    update_claim(
        "k", root=tmp_path, expires_at=now_ms() - 1, pid=os.getpid(),
        pid_provenance="session-prover", session_id=None,
    )
    time.sleep(0.01)
    refreshed = refresh_claim("k", HOLDER_A, ttl_ms=7_200_000, root=tmp_path)
    assert refreshed is not None
    assert abs(refreshed.expires_at - (now_ms() + 7_200_000)) < 2_000


def test_AC2_ERR_refresh_refuses_verdict_stale_and_leaves_the_row_alone(tmp_path):
    """A stale verdict (dead holder, no live witness) refuses, and the
    refusal leaves the row unchanged."""
    acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
    update_claim(
        "k", root=tmp_path, expires_at=now_ms() - 1, pid=_gone_pid(),
        pid_provenance="session-prover", session_id=None,
    )
    before = read_claim_row("k", root=tmp_path)
    with pytest.raises(ClaimValidationError, match="holder reads dead"):
        refresh_claim("k", HOLDER_A, ttl_ms=7_200_000, root=tmp_path)
    assert read_claim_row("k", root=tmp_path) == before


def test_AC5_HP_list_empty_when_no_claims(tmp_path, monkeypatch):
    # The list reads the global root too; pin it so other tests' claims stay out.
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    assert list_claims(root=tmp_path) == []


def test_AC5_FR_list_excludes_stale_by_default(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    acquire_claim("expired", HOLDER_A, ttl_ms=60_000, root=tmp_path)
    update_claim(
        "expired", root=tmp_path, acquired_at=now_ms() - 200_000,
        expires_at=now_ms() - 100_000, pid=_gone_pid(), session_id=None,
    )
    assert list_claims(root=tmp_path) == []
    assert any(r["key"] == "expired" for r in list_claims(include_stale=True, root=tmp_path))


def test_AC6_FR_force_release_archives_the_row(tmp_path):
    acquire_claim("k", HOLDER_A, root=tmp_path)
    outcome = force_release_claim("k", reason="cleanup", root=tmp_path)
    assert outcome.archived is True
    assert outcome.previous_holder == HOLDER_A
    assert claim_status("k", root=tmp_path)["state"] == "free"


def test_acquire_pinned_session_id_wins(tmp_path):
    """An explicit harness_session_id (the init-hook pin) beats ambient."""
    claim = acquire_claim(
        "node:x-sid", HOLDER_A, ttl_ms=60_000, pid=os.getpid(),
        harness_session_id="pinned-sid", root=tmp_path,
    )
    assert claim.session_id == "pinned-sid"
