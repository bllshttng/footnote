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
from unittest.mock import patch

import psutil
import pytest

from fno.claims.core import (
    ClaimContended,
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
from fno.claims.io import claim_path, claims_dir, read_claim_file, serialize_claim
from fno.claims.types import Claim, ClaimState, now_ms


HOLDER_A = "target-session:sid-a"
HOLDER_B = "target-session:sid-b"


# ---------------------------------------------------------------------------
# acquire
# ---------------------------------------------------------------------------


class TestAcquire:
    def test_AC1_HP_fresh_key(self, tmp_path):
        claim = acquire_claim("node:ab-1", HOLDER_A, root=tmp_path)
        assert claim.holder == HOLDER_A
        assert claim_path("node:ab-1", root=tmp_path).exists()

    def test_AC1_FR_pid_liveness_omits_expires_at(self, tmp_path):
        claim = acquire_claim("k", HOLDER_A, root=tmp_path)
        assert claim.expires_at is None
        text = claim_path("k", root=tmp_path).read_text()
        assert "expires_at" not in text

    def test_AC1_FR_ttl_sets_expires_at(self, tmp_path):
        claim = acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        assert claim.expires_at is not None
        assert claim.expires_at > claim.acquired_at

    def test_AC3_HP_ttl_pid_unavailable_is_explicit(self, tmp_path):
        claim = acquire_claim(
            "k", HOLDER_A, ttl_ms=60_000, pid_unavailable=True, root=tmp_path
        )
        assert claim.pid is None
        assert claim.pid_unavailable is True
        assert claim.schema_version == 2
        payload = claim_path("k", root=tmp_path).read_text()
        assert "pid: null" in payload
        assert "pid_unavailable: true" in payload
        assert claim_status("k", root=tmp_path)["pid_unavailable"] is True

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

    def test_acquire_contention_recursion_is_bounded(self, tmp_path):
        """Perpetual recovery-mutex contention must raise, not recurse forever.

        acquire_claim's own holder (idempotent) path always takes the
        recovery mutex, so patching acquire_dir_mutex to always report
        "busy" drives a deterministic, PID-liveness-independent path through
        _retry() every call - proving the ACQUIRE_MAX_ATTEMPTS cap fires
        rather than growing the Python call stack unbounded.
        """
        import fno.claims.core as claims_core

        acquire_claim("k", HOLDER_A, root=tmp_path)
        with patch.object(claims_core, "acquire_dir_mutex", return_value=None):
            with pytest.raises(ClaimContended, match="gave up after"):
                acquire_claim("k", HOLDER_A, root=tmp_path)

    def test_idempotent_reverify_releases_lock_before_recursing(self, tmp_path):
        """The idempotent branch's locked re-verify must release the
        recovery mutex BEFORE recursing (_release_and_retry, not a bare
        _retry) when the fresh read shows a different holder.

        Python evaluates a `return <expr>` expression before the enclosing
        `finally` runs, so recursing while `acquired_lock` is still True
        would have the recursive call poll for the SAME per-key mutex this
        frame is still sitting on, if the recursion lands back in the
        stale-reclaim branch (which it does here: the "other" holder found
        on re-verify has a dead pid). Proven by asserting the mutex is
        acquired exactly twice with a release between them, never
        acquire-acquire, and that this resolves without hitting
        ACQUIRE_MAX_ATTEMPTS.
        """
        import fno.claims.core as claims_core

        acquire_claim("k", HOLDER_A, root=tmp_path)  # real live claim, holder=HOLDER_A

        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        stale_other = Claim(
            key="k", holder=HOLDER_B, acquired_at=0, pid=dead_pid,
            host=socket.gethostname(),
        )

        real_read = claims_core.read_claim_file
        call_count = {"n": 0}

        def _read_side_effect(path):
            call_count["n"] += 1
            # 1st call: acquire_claim's own top-level unlocked read (real).
            # 2nd call: the idempotent branch's locked re-verify - simulate
            # a race where an unlocked writer (e.g. force_release_claim +
            # a third party's top-level create) swapped in a stale claim
            # for a different holder between the two reads.
            if call_count["n"] == 2:
                return stale_other
            return real_read(path)

        order = []
        real_acquire = claims_core.acquire_dir_mutex
        real_release = claims_core.release_dir_mutex

        def _acquire_side_effect(lock_dir, timeout_s, **kw):
            order.append("acquire")
            return real_acquire(lock_dir, timeout_s, **kw)

        def _release_side_effect(lock_dir, token):
            order.append("release")
            return real_release(lock_dir, token)

        with patch.object(claims_core, "read_claim_file", side_effect=_read_side_effect), \
                patch.object(claims_core, "acquire_dir_mutex", side_effect=_acquire_side_effect), \
                patch.object(claims_core, "release_dir_mutex", side_effect=_release_side_effect):
            claim = acquire_claim("k", HOLDER_A, root=tmp_path)

        assert claim.holder == HOLDER_A
        assert order.count("acquire") == 2, order
        # The release between the two acquires proves the recursive call's
        # own acquire never contended against this frame's still-held lock.
        first_acquire = order.index("acquire")
        second_acquire = order.index("acquire", first_acquire + 1)
        release_idx = order.index("release")
        assert first_acquire < release_idx < second_acquire, order

    def test_AC4_EDGE_stale_pid_recovered(self, tmp_path):
        """A claim whose holder process is dead is reclaimable by another holder."""
        # Pick a definitely-dead PID and hand-write a claim for it.
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        from fno.claims.types import now_ms
        stale = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 100_000,
            expires_at=None,
            pid=dead_pid,
            host=socket.gethostname(),
        )
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(stale))

        # New holder can take over.
        new = acquire_claim("k", HOLDER_B, root=tmp_path)
        assert new.holder == HOLDER_B
        # The old claim is archived.
        archive_dir = claims_dir(tmp_path) / ".expired"
        assert archive_dir.exists()
        assert any(archive_dir.iterdir())

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

    def test_hybrid_expired_live_pid_not_reclaimable(self, tmp_path):
        """CORROBORATED HYBRID (codex P1): an expired TTL claim whose recorded
        pid is a live PROVER-PROVEN process is NOT reclaimable - acquire must
        honor the same hybrid liveness as classify(), so a peer parks instead
        of stealing the node from a suspended-but-alive session (AC1-ERR)."""
        # Anchor acquired_at AFTER this process's create_time so is_live's
        # pid-reuse guard (create_time < acquired_at) passes; both timestamps
        # are now-relative and in the past so the TTL is expired on any runner
        # speed (a create-relative expiry is still future on a fast one).
        proc_create_ms = int(psutil.Process(os.getpid()).create_time() * 1000)
        assert now_ms() - 100 > proc_create_ms
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        expired_live = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 100,   # after this proc started -> live
            expires_at=now_ms() - 50,     # in the past -> TTL lapsed
            pid=os.getpid(),               # alive on this host
            host=socket.gethostname(),
            pid_provenance="session-prover",
        )
        path.write_text(serialize_claim(expired_live))

        with pytest.raises(ClaimHeldByOther) as exc:
            acquire_claim("k", HOLDER_B, root=tmp_path)
        assert exc.value.holder == HOLDER_A
        # The live claim was NOT archived.
        archive_dir = claims_dir(tmp_path) / ".expired"
        assert not (archive_dir.exists() and any(archive_dir.iterdir()))

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

    def test_prover_resolved_pid_on_a_ttl_claim_stamps_session_prover(self, tmp_path, monkeypatch):
        """The target-init shape: the caller resolved its pid through the
        process-tree prover (here: the prover answers our own pid), so the
        long live session keeps its hybrid-arm protection past TTL expiry."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        # The harness is passed, not walked: the stamp now gates on the value
        # the writer stores, so an unpinned test would assert session-prover
        # under claude and ambient under codex - a flake keyed to who ran it.
        claim = acquire_claim(
            "node:x-1", HOLDER_A, ttl_ms=60_000, pid=os.getpid(),
            harness="claude", root=tmp_path
        )
        assert claim.pid_provenance == "session-prover"

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

    def test_rebind_earns_provenance_for_the_new_pid(self, tmp_path, monkeypatch):
        """A rebind rewrites the pid, so the prior record's provenance must
        not survive it: the new pid earns its own stamp (the handover path
        passes a prover-resolved pid, so the rebond claim reads session-prover)."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        # A dead-prior handover claim the worker's init takes over.
        handover = Claim(
            key="node:x-5", holder="spawn-handover:bp-x5",
            acquired_at=now_ms(), expires_at=now_ms() + 900_000,
            pid=999_999_999, host=socket.gethostname(), pid_provenance="ambient",
        )
        path = claim_path("node:x-5", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(handover))

        # The harness is named, not walked: the rebind gates the stamp on the
        # harness it STORES, so leaving it ambient makes this assert
        # session-prover under claude and ambient under codex.
        claim, mode = compare_and_rebind(
            "node:x-5", "spawn-handover:bp-x5",
            new_holder=HOLDER_A, new_pid=os.getpid(), ttl_ms=3_600_000,
            new_harness="claude", root=tmp_path,
        )
        assert mode == "handover"
        assert claim.pid == os.getpid()
        assert claim.pid_provenance == "session-prover"
        assert claim.harness == "claude"

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

    def test_rebind_keeping_a_shared_host_harness_still_stamps_ambient(
        self, tmp_path, monkeypatch
    ):
        """The branch the first fix missed. A non-handover rebind writes NO new
        harness, so the record KEEPS its own - here codex - while the caller
        resolved its provenance against the rebinding process's harness before
        the mutex. Passing the expected harness at the call site cannot cover
        this, because the two part company after that point. _rebound_claim is
        the single branch that picks the written harness, so the narrowing
        lives there and every caller inherits it."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        existing = Claim(
            key="node:x-8", holder=HOLDER_A,
            acquired_at=now_ms(), expires_at=now_ms() + 900_000,
            pid=os.getpid(), host=socket.gethostname(),
            harness="codex", pid_provenance="ambient",
        )
        import fno.claims.core as claims_core

        rebound = claims_core._rebound_claim(
            existing, os.getpid(), 60_000,
            # What a claude caller resolves for itself before the mutex.
            new_pid_provenance="session-prover",
        )
        assert rebound.harness == "codex"
        assert rebound.pid_provenance == "ambient"

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

    def test_per_session_harness_still_earns_the_prover_stamp(self, tmp_path, monkeypatch):
        """AC1's twin: the identical acquire under a harness that forks per
        session still earns the stamp. The deny-list must not cost claude the
        hybrid protection it was built for."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        claim = acquire_claim(
            "node:x-forked", HOLDER_A, ttl_ms=60_000, pid=os.getpid(),
            harness="claude", root=tmp_path
        )
        assert claim.pid_provenance == "session-prover"

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

    def test_refresh_reanchor_stamps_session_prover(self, tmp_path, monkeypatch):
        """The renewal re-anchor's pid IS the prover's answer by construction,
        so the refreshed claim keeps hybrid protection; a bare TTL extension
        (no anchor) leaves the written provenance untouched."""
        monkeypatch.setattr(
            "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: os.getpid()
        )
        first = acquire_claim(
            "node:x-6", HOLDER_A, ttl_ms=60_000, pid=os.getpid(),
            harness="claude", root=tmp_path
        )
        assert first.pid_provenance == "session-prover"
        refreshed = refresh_claim("node:x-6", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        assert refreshed is not None
        assert refreshed.pid_provenance == "session-prover"


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

    def test_AC2_FR_release_silently_skips_other_holder(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        # Wrong holder: silent success (no exception).
        release_claim("k", HOLDER_B, root=tmp_path)
        assert claim_path("k", root=tmp_path).exists()

    def test_AC2_ERR_release_strict_raises_on_mismatch(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        with pytest.raises(HolderMismatch):
            release_claim("k", HOLDER_B, strict=True, root=tmp_path)

    def test_AC2_ERR_empty_key_rejected(self, tmp_path):
        with pytest.raises(ClaimValidationError):
            release_claim("", HOLDER_A, root=tmp_path)

    def test_AC2_FR_release_emits_duration(self, tmp_path):
        # Just verify the call doesn't raise; duration is best-effort visible in events.jsonl
        acquire_claim("k", HOLDER_A, root=tmp_path)
        release_claim("k", HOLDER_A, root=tmp_path)

    def test_strict_release_compares_and_unlinks_under_per_key_mutex(self, tmp_path):
        import fno.claims.core as claims_core

        acquire_claim("node:x-abcd", HOLDER_A, root=tmp_path)
        real_read = claims_core.read_claim_file
        observed = []

        def locked_read(path):
            mutex = path.with_name(path.name + ".recovery.d")
            observed.append(mutex.is_dir())
            return real_read(path)

        with patch.object(claims_core, "read_claim_file", side_effect=locked_read):
            release_claim(
                "node:x-abcd", HOLDER_A, strict=True, root=tmp_path
            )

        assert observed == [True]
        assert not claim_path("node:x-abcd", root=tmp_path).exists()


# ---------------------------------------------------------------------------
# refresh
# ---------------------------------------------------------------------------


class TestRefresh:
    def test_AC3_HP_refresh_extends_expires_at(self, tmp_path):
        first = acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        # Sleep to ensure now_ms() advances.
        import time
        time.sleep(0.01)
        refreshed = refresh_claim("k", HOLDER_A, ttl_ms=120_000, root=tmp_path)
        assert refreshed is not None
        assert refreshed.expires_at > first.expires_at

    def test_refresh_contention_recursion_is_bounded(self, tmp_path):
        """Perpetual recovery-mutex contention must raise, not recurse forever."""
        import fno.claims.core as claims_core

        acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        with patch.object(claims_core, "acquire_dir_mutex", return_value=None):
            with pytest.raises(ClaimContended, match="gave up after"):
                refresh_claim("k", HOLDER_A, root=tmp_path)

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

    def test_refresh_refuses_claim_that_expires_while_waiting_for_recovery_mutex(
        self, tmp_path, monkeypatch
    ):
        """The under-mutex reread is the authority: expiry in the wait window
        cannot be rewritten into a new lease by the old holder."""
        import fno.claims.core as claims_core

        path = claim_path("k", root=tmp_path)
        acquire_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)
        real_acquire = claims_core.acquire_dir_mutex
        expired_deadline = now_ms() - 1

        def acquire_then_expire(lock_path, timeout_s, **kwargs):
            token = real_acquire(lock_path, timeout_s, **kwargs)
            existing = read_claim_file(path)
            path.write_text(
                serialize_claim(existing.model_copy(update={"expires_at": expired_deadline}))
            )
            return token

        monkeypatch.setattr(claims_core, "acquire_dir_mutex", acquire_then_expire)

        with pytest.raises(ClaimValidationError, match="expired"):
            refresh_claim("k", HOLDER_A, ttl_ms=60_000, root=tmp_path)

        assert read_claim_file(path).expires_at == expired_deadline


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

    def test_AC4_HP_status_corrupted(self, tmp_path):
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("not yaml at all: ::::")
        result = claim_status("k", root=tmp_path)
        assert result["state"] == ClaimState.CORRUPTED.value
        assert "error" in result

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


# ---------------------------------------------------------------------------
# list
# ---------------------------------------------------------------------------


class TestList:
    def test_AC5_HP_list_empty_when_no_claims(self, tmp_path):
        assert list_claims(root=tmp_path) == []

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

    def test_AC5_FR_list_excludes_stale_by_default(self, tmp_path):
        from fno.claims.types import now_ms
        # Write an expired TTL claim whose holder is DEAD. Under hybrid liveness
        # (ab-cc5553f2) an expired TTL claim with a still-LIVE on-host pid stays
        # LIVE, so pinning pid=os.getpid() here made the claim flip to LIVE once
        # this pytest process had run longer than the TTL window (create_time
        # then precedes acquired_at and is_live's pid-reuse guard passes) - a
        # latent flake that only fired late in a full-suite run. A definitely-
        # dead pid keeps the claim unambiguously STALE (mirrors the dead-pid
        # pattern the sibling stale-claim tests already use).
        dead_pid = 999_999
        while psutil.pid_exists(dead_pid):
            dead_pid += 1
        path = claim_path("expired", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(Claim(
            key="expired",
            holder=HOLDER_A,
            acquired_at=now_ms() - 200_000,
            expires_at=now_ms() - 100_000,
            pid=dead_pid,
            host=socket.gethostname(),
        )))
        # Default: stale excluded
        assert list_claims(root=tmp_path) == []
        # include_stale=True surfaces it
        all_results = list_claims(include_stale=True, root=tmp_path)
        assert any(r["key"] == "expired" for r in all_results)


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

    def test_AC6_FR_archives_to_expired_dir(self, tmp_path):
        acquire_claim("k", HOLDER_A, root=tmp_path)
        force_release_claim("k", reason="cleanup", root=tmp_path)
        archive = claims_dir(tmp_path) / ".expired"
        assert archive.exists()
        assert any(archive.iterdir())

    def test_force_release_takes_and_releases_recovery_mutex(self, tmp_path):
        """force_release_claim must take the SAME per-key recovery mutex
        acquire_claim/refresh_claim/reap_dead_claims take, closing the
        resurrection race where a concurrent idempotent re-acquire reads the
        still-present claim under its own lock and writes it back right after
        this call's archive_claim moves the file away."""
        import fno.claims.core as claims_core

        acquire_claim("k", HOLDER_A, root=tmp_path)

        calls: list[str] = []
        real_acquire = claims_core.acquire_dir_mutex
        real_release = claims_core.release_dir_mutex

        def _acquire_spy(*args, **kwargs):
            calls.append("acquire")
            return real_acquire(*args, **kwargs)

        def _release_spy(*args, **kwargs):
            calls.append("release")
            return real_release(*args, **kwargs)

        with patch.object(claims_core, "acquire_dir_mutex", _acquire_spy), \
             patch.object(claims_core, "release_dir_mutex", _release_spy):
            force_release_claim("k", reason="operator override", root=tmp_path)

        assert calls == ["acquire", "release"]
        assert not claim_path("k", root=tmp_path).exists()

    def test_force_release_still_succeeds_when_mutex_acquire_times_out(self, tmp_path):
        """A contended recovery mutex must not turn force-release's
        'always succeeds' administrative-override contract into a raise -
        it proceeds without the lock on timeout instead."""
        import fno.claims.core as claims_core

        acquire_claim("k", HOLDER_A, root=tmp_path)

        with patch.object(claims_core, "acquire_dir_mutex", return_value=None):
            force_release_claim("k", reason="operator override", root=tmp_path)

        assert not claim_path("k", root=tmp_path).exists()


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

    def test_acquire_stamps_session_id(self, tmp_path, monkeypatch):
        """AC1: a claim acquired under a resolvable session identity reads it
        back, resolved beside the harness from ONE identity answer."""
        import types

        import fno.claims.core as claims_core

        identity = types.SimpleNamespace(
            harness="claude", session_id="abc123", disposition="canonical"
        )
        monkeypatch.setattr(claims_core, "resolve_self_identity", lambda: identity)
        claim = acquire_claim(
            "node:x-sid", HOLDER_A, ttl_ms=60_000, pid=os.getpid(), root=tmp_path
        )
        assert claim.session_id == "abc123"

    def test_acquire_pinned_session_id_wins(self, tmp_path, monkeypatch):
        """An explicit harness_session_id (the init-hook pin) beats ambient."""
        import types

        import fno.claims.core as claims_core

        identity = types.SimpleNamespace(
            harness="claude", session_id="ambient", disposition="canonical"
        )
        monkeypatch.setattr(claims_core, "resolve_self_identity", lambda: identity)
        claim = acquire_claim(
            "node:x-sid",
            HOLDER_A,
            ttl_ms=60_000,
            pid=os.getpid(),
            harness_session_id="pinned-sid",
            root=tmp_path,
        )
        assert claim.session_id == "pinned-sid"

    def test_registry_session_pid_reads_the_row_binding(self, tmp_path, monkeypatch):
        """The row keyed by harness_session_id names the live pid; a dead or
        missing row answers None and the caller falls through."""
        import json as _json

        from fno.claims.core import _registry_session_pid

        registry = tmp_path / "registry.json"
        registry.write_text(
            _json.dumps(
                {
                    "schema_version": 24,
                    "agents": [
                        {
                            "name": "w",
                            "harness": "claude",
                            "harness_session_id": "ses_r",
                            "pid": os.getpid(),
                        },
                        {
                            "name": "dead",
                            "harness": "claude",
                            "harness_session_id": "ses_dead",
                            "pid": 999_999_999,
                        },
                    ],
                }
            )
        )
        monkeypatch.setattr("fno.paths.agents_registry_path", lambda: registry)
        assert _registry_session_pid("ses_r") == os.getpid()
        assert _registry_session_pid("ses_dead") is None
        assert _registry_session_pid("ses_absent") is None

    def test_resume_rebind_backfills_session_id(self, tmp_path, monkeypatch):
        """A pre-change claim (no session_id) rebound on resume takes the
        rebinding session's id."""
        import types

        import fno.claims.core as claims_core

        monkeypatch.setattr(
            claims_core,
            "resolve_self_identity",
            lambda: types.SimpleNamespace(harness="claude", session_id="new-sid"),
        )
        dead_pid = _dead_pid_context()
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        legacy = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 100,
            expires_at=now_ms() + 600_000,
            pid=dead_pid,
            host=socket.gethostname(),
        )
        path.write_text(serialize_claim(legacy))

        rebound, mode = compare_and_rebind("k", HOLDER_A, ttl_ms=600_000, root=tmp_path)
        assert mode == "rebound"
        assert rebound.session_id == "new-sid"

    def test_handover_restamps_session_id_to_the_successor(self, tmp_path, monkeypatch):
        """A handover changes WHO owns the claim, so the spawner's session id
        must not survive it - same rule as the harness tag."""
        import types

        import fno.claims.core as claims_core

        monkeypatch.setattr(
            claims_core,
            "resolve_self_identity",
            lambda: types.SimpleNamespace(harness="claude", session_id="successor-sid"),
        )
        dead_pid = _dead_pid_context()
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        handed = Claim(
            key="k",
            holder="spawn-handover:w1",
            acquired_at=now_ms() - 100,
            expires_at=now_ms() + 600_000,
            pid=dead_pid,
            host=socket.gethostname(),
            session_id="spawner-sid",
        )
        path.write_text(serialize_claim(handed))

        rebound, mode = compare_and_rebind(
            "k", "spawn-handover:w1", new_holder=HOLDER_B, ttl_ms=600_000, root=tmp_path
        )
        assert mode == "handover"
        assert rebound.session_id == "successor-sid"


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
        assert status["state"] == "live"
        assert status["basis"] == "registry-session-live"
        assert status["session_basis"] == "registry-session-live"

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
        assert status["state"] == "suspect"
        assert status["basis"] == "ttl-expired-unresolved"
        assert status["session_basis"] == "unresolved"

    def test_status_without_session_id_keeps_legacy_stale(self, tmp_path, monkeypatch):
        """AC4: a pre-change claim (no session id) reads Stale on expiry - the
        reaper behavior the 1511 revert restored, unchanged."""
        self._pin_native_binary(monkeypatch)
        dead_pid = _dead_pid_context()
        self._write_claim(tmp_path, "k", None, dead_pid, -60_000)
        status = claim_status("k", root=tmp_path)
        assert status["state"] == "stale"
        assert "session_basis" not in status

    def test_refresh_never_reanchors_a_live_recorded_pid(self, tmp_path, monkeypatch):
        """A healthy claim (recorded pid alive) is never re-anchored, even when
        its registry row names a DIFFERENT live process (the keeper-pid shape:
        rows record the keeper, not the child). Rewriting a running session's
        anchor is the takeover the first None case exists to prevent."""
        import fno.claims.core as claims_core

        foreign = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        try:
            # Let the child's create time fall clearly BEFORE acquired_at, so
            # the recorded pid reads as the same live process, not a reuse.
            time.sleep(0.3)
            rec = self._write_claim(tmp_path, "k", "ses_live", foreign.pid, 600_000)
            monkeypatch.setattr(
                claims_core, "_registry_session_pid", lambda sid: os.getpid()
            )
            refreshed = refresh_claim("k", HOLDER_A, ttl_ms=600_000, root=tmp_path)
            assert refreshed.pid == foreign.pid, "a live anchor must stay put"
            assert refreshed.acquired_at == rec.acquired_at
        finally:
            foreign.terminate()
            foreign.wait()

    def test_refresh_reanchors_to_registry_row_pid_under_resume(self, tmp_path, monkeypatch):
        """AC6: a claim whose recorded pid is dead and whose session row names
        a live pid re-anchors to THAT pid on renewal - created after
        acquired_at and all. Positive marker: the pid MOVES."""
        import fno.claims.core as claims_core

        dead_pid = _dead_pid_context()
        rec = self._write_claim(tmp_path, "k", "ses_move", dead_pid, 600_000)
        monkeypatch.setattr(claims_core, "_registry_session_pid", lambda sid: os.getpid())
        refreshed = refresh_claim("k", HOLDER_A, ttl_ms=600_000, root=tmp_path)
        assert refreshed.pid == os.getpid(), "the anchor must MOVE to the row pid"
        assert refreshed.acquired_at == rec.acquired_at

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
