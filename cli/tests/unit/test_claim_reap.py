"""Unit tests for claim GC (reap) and the list reader's honesty.

Two things had no test before this: nothing pruned a claim whose holder died
without releasing (so a leaked lockfile stayed forever), and `fno agents claim list`
rendered a store that was 99 percent stale as an empty store. Every test here
proves a real defect would have been caught - especially the load-bearing
case (test_AC1_HP_kill_without_release_is_reaped), which spawns a real
subprocess and kills it. A test that only exercises a clean release proves
nothing about the leak that was measured.
"""
from __future__ import annotations

import os
import socket
import subprocess
import sys
import threading
import time
from types import SimpleNamespace

import psutil
import pytest
from typer.testing import CliRunner

from fno.claims.cli import cli
from fno.claims.core import (
    ClaimGoneAway,
    ClaimHeldByOther,
    acquire_claim,
    claim_status,
    list_claims_with_counts,
    reap_dead_claims,
    refresh_claim,
)
from fno.claims.io import archive_claim, claim_path, claims_dir, read_claim_file, serialize_claim
from fno.claims.staleness import classify_for_sweep, is_provably_dead, now_ms
from fno.claims.types import Claim
from fno.mutex import acquire_dir_mutex, release_dir_mutex


HOLDER_A = "target-session:sid-a"
runner = CliRunner()


def _dead_pid() -> int:
    dead = 999_999
    while psutil.pid_exists(dead):
        dead += 1
    return dead


# ---------------------------------------------------------------------------
# is_provably_dead: the predicate itself (staleness.py)
# ---------------------------------------------------------------------------


class TestIsProvablyDead:
    def test_dead_pid_same_machine_is_provably_dead(self):
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms() - 60_000, expires_at=None,
            pid=_dead_pid(), host=socket.gethostname(),
        )
        assert is_provably_dead(claim) is True

    def test_off_machine_is_never_provably_dead(self):
        """AC3: even a locally-dead-looking pid cannot be proven dead off-host."""
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms() - 60_000, expires_at=None,
            pid=_dead_pid(), host="some-other-host", machine_id="not-this-machine",
        )
        assert is_provably_dead(claim) is False

    def test_ttl_protected_suspect_is_never_provably_dead(self):
        """AC4: dead pid but still inside the TTL window -> SUSPECT, kept."""
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms(), expires_at=now_ms() + 60_000,
            pid=_dead_pid(), host=socket.gethostname(),
        )
        assert is_provably_dead(claim) is False

    def test_live_claim_is_never_provably_dead(self):
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms(), expires_at=None,
            pid=os.getpid(), host=socket.gethostname(),
        )
        assert is_provably_dead(claim) is False


class TestExpiredTTLIsHostIndependent:
    """x-cd1e: an expired TTL is provably dead from any host, for the rows that
    cannot be identified at all.

    The measured leak: this machine wrote claims as ``BB16s-MBP``,
    ``BB16s-MacBook-Pro.local`` and a tailnet name within one hour. Rows
    predating the ``machine_id`` field carry only that moving name, so a
    host-gated sweep could never satisfy its same-machine proof and kept them
    forever. Expiry is a clock reading, not a local measurement, so for THOSE
    rows it needs no such proof.

    A row that names a real other machine keeps the gate, and the boundary is
    load-bearing: ``classify``'s corroborated hybrid arm reads an expired
    claim as LIVE when its pid is live and prover-proven, and that pid means
    something only on the machine that
    wrote it. Reaping one from here would archive a claim its owner is still
    refreshing, and the next reader would staff a second worker onto the node.
    """

    def test_expired_ttl_on_an_unidentifiable_row_is_reapable(self):
        """The row this arm exists for: no machine_id, and a hostname that has
        already moved, so no same-machine proof is ever possible for it."""
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms() - 120_000,
            expires_at=now_ms() - 60_000, pid=_dead_pid(),
            host="bb16s-macbook-pro.bigeye-truck.ts.net",
            machine_id=None,
        )
        assert is_provably_dead(claim) is True

    def test_expired_ttl_on_an_unidentifiable_row_ignores_a_live_local_pid(self):
        """The pid arm cannot rescue it: that pid belongs to another machine's
        namespace, so a locally-live number proves nothing about the holder.
        """
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms() - 120_000,
            expires_at=now_ms() - 60_000, pid=os.getpid(),
            host="some-other-host", machine_id=None,
        )
        assert is_provably_dead(claim) is True

    def test_expired_ttl_from_a_named_other_machine_is_kept(self):
        """The boundary. That machine's `classify` reads this same claim as
        LIVE whenever its pid is live, so archiving it here publishes as free a
        node its owner is still working."""
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms() - 120_000,
            expires_at=now_ms() - 60_000, pid=_dead_pid(),
            host="some-other-host", machine_id="not-this-machine",
        )
        provably_dead, bucket = classify_for_sweep(claim)
        assert (provably_dead, bucket) == (False, "offhost")

    def test_off_host_pid_liveness_claim_is_still_kept(self):
        """The pid arm keeps its same-machine proof. Only expiry travels."""
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms() - 60_000, expires_at=None,
            pid=_dead_pid(), host="some-other-host", machine_id="not-this-machine",
        )
        provably_dead, bucket = classify_for_sweep(claim)
        assert (provably_dead, bucket) == (False, "offhost")

    def test_off_host_unexpired_ttl_is_still_kept(self):
        claim = Claim(
            key="k", holder="h", acquired_at=now_ms(),
            expires_at=now_ms() + 60_000, pid=_dead_pid(),
            host="some-other-host", machine_id="not-this-machine",
        )
        provably_dead, bucket = classify_for_sweep(claim)
        assert (provably_dead, bucket) == (False, "offhost")

    def test_expired_ttl_on_this_machine_with_a_live_pid_is_still_live(self):
        """The corroborated hybrid arm survives: a suspended local session
        keeps its slot when its pid was prover-proven at write time.

        ``acquired_at`` has to postdate this process's own create_time, or
        ``is_live`` reads the pid as reused and the claim is dead for an
        unrelated reason - which would pass this assertion for the wrong one.
        """
        started = int(psutil.Process(os.getpid()).create_time() * 1000)
        claim = Claim(
            key="k", holder="h", acquired_at=started + 1,
            expires_at=now_ms() - 1, pid=os.getpid(),
            host=socket.gethostname(), pid_provenance="session-prover",
        )
        provably_dead, bucket = classify_for_sweep(claim)
        assert (provably_dead, bucket) == (False, "live")

    def test_expired_ttl_on_this_machine_with_a_live_unproven_pid_reaps(self):
        """The specimen flip side: the same live local pid WITHOUT provenance
        is reapable at expiry. A claim that cannot prove its pid cannot
        outrank its own TTL."""
        started = int(psutil.Process(os.getpid()).create_time() * 1000)
        claim = Claim(
            key="k", holder="h", acquired_at=started + 1,
            expires_at=now_ms() - 1, pid=os.getpid(),
            host=socket.gethostname(),
        )
        assert is_provably_dead(claim) is True

    def test_hostname_drift_between_two_claims_does_not_change_the_verdict(self):
        """The two spellings one box wrote in one hour reap identically."""
        verdicts = {
            host: is_provably_dead(
                Claim(
                    key="k", holder="h", acquired_at=now_ms() - 120_000,
                    expires_at=now_ms() - 60_000, pid=_dead_pid(), host=host,
                )
            )
            for host in ("BB16s-MBP", "BB16s-MacBook-Pro.local")
        }
        assert verdicts == {"BB16s-MBP": True, "BB16s-MacBook-Pro.local": True}


class TestClassifyForSweepMatchesIsProvablyDead:
    """is_provably_dead is a thin bool-only view of classify_for_sweep
    (staleness.py) - both share one implementation rather than being kept
    in sync by convention. This regression test pins the invariant
    directly rather than trusting that never drifts back apart.
    """

    @pytest.mark.parametrize(
        "claim",
        [
            pytest.param(
                Claim(
                    key="k", holder="h", acquired_at=now_ms() - 60_000, expires_at=None,
                    pid=_dead_pid(), host=socket.gethostname(),
                ),
                id="dead_pid_same_machine",
            ),
            pytest.param(
                Claim(
                    key="k", holder="h", acquired_at=now_ms() - 60_000, expires_at=None,
                    pid=_dead_pid(), host="some-other-host", machine_id="not-this-machine",
                ),
                id="off_machine",
            ),
            pytest.param(
                Claim(
                    key="k", holder="h", acquired_at=now_ms(), expires_at=now_ms() + 60_000,
                    pid=_dead_pid(), host=socket.gethostname(),
                ),
                id="ttl_protected_suspect",
            ),
            pytest.param(
                Claim(
                    key="k", holder="h", acquired_at=now_ms(), expires_at=None,
                    pid=os.getpid(), host=socket.gethostname(),
                ),
                id="live",
            ),
        ],
    )
    def test_provably_dead_verdict_matches(self, claim):
        ts = now_ms()
        provably_dead, _bucket = classify_for_sweep(claim, ts)
        assert provably_dead is is_provably_dead(claim, now=ts)


# ---------------------------------------------------------------------------
# reap_dead_claims: the reaper (core.py)
# ---------------------------------------------------------------------------


class TestReapDeadClaims:
    def test_AC1_HP_kill_without_release_is_reaped(self, tmp_path):
        """The load-bearing case. A real process, really killed, never released."""
        proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        try:
            acquire_claim("node:x-killed", HOLDER_A, pid=proc.pid, root=tmp_path)
            proc.kill()
            proc.wait(timeout=10)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=10)

        path = claim_path("node:x-killed", root=tmp_path)
        assert path.exists()

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 1
        assert summary["reap_failed"] == []
        assert not path.exists(), "lockfile must be gone from the active store"
        expired = list((claims_dir(tmp_path) / ".expired").glob("*.lock"))
        assert len(expired) == 1, "the file must be present under .expired/"
        assert expired[0].name.startswith("node%3Ax-killed."), expired[0].name

    def test_AC2_FR_both_roots_swept_in_one_run(self, tmp_path):
        root_a = tmp_path / "a"
        root_b = tmp_path / "b"
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=root_a)
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=root_b)

        summary = reap_dead_claims(roots=[root_a, root_b], apply=True)

        assert summary["reaped"] == 2
        assert len(summary["roots"]) == 2
        assert list((claims_dir(root_a) / ".expired").glob("*.lock"))
        assert list((claims_dir(root_b) / ".expired").glob("*.lock"))

    def test_roots_deduped_by_resolve(self, tmp_path):
        """A root passed twice (e.g. the cwd-local root already being the
        global one) must be swept once, not twice."""
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        summary = reap_dead_claims(roots=[tmp_path, tmp_path], apply=True)

        assert summary["reaped"] == 1
        assert len(summary["roots"]) == 1

    def test_AC3_FR_off_machine_claim_never_reaped(self, tmp_path):
        claim = Claim(
            key="node:x-remote", holder=HOLDER_A, acquired_at=now_ms() - 100_000,
            expires_at=None, pid=os.getpid(), host="some-other-host",
            machine_id="not-this-machine",
        )
        path = claim_path("node:x-remote", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(claim))

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 0
        assert summary["kept_offhost"] == 1
        assert path.exists(), "an off-host claim must never be archived"

    def test_AC4_FR_ttl_protected_suspect_never_reaped(self, tmp_path):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), ttl_ms=60_000, root=tmp_path)

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 0
        assert summary["kept_suspect"] == 1
        assert claim_path("k", root=tmp_path).exists()

    def test_live_claim_kept_live(self, tmp_path):
        acquire_claim("k", HOLDER_A, pid=os.getpid(), root=tmp_path)

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 0
        assert summary["kept_live"] == 1

    def test_AC5_EDGE_failed_move_reported_not_reaped(self, tmp_path, monkeypatch):
        """The positive-marker rule: a no-op archive_claim must not count as reaped."""
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        monkeypatch.setattr("fno.claims.core.archive_claim", lambda path, ts_ms: path)

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 0
        assert len(summary["reap_failed"]) == 1
        assert summary["reap_failed"][0][0] == str(claim_path("k", root=tmp_path))
        assert claim_path("k", root=tmp_path).exists()

    def test_archive_claim_oserror_reported_not_raised_and_sweep_continues(
        self, tmp_path, monkeypatch
    ):
        """A permission error, full disk, or other rename failure inside
        archive_claim must not abort the whole sweep - every other dead
        claim is still reapable."""
        from fno.claims.io import archive_claim as real_archive_claim

        acquire_claim("bad", HOLDER_A, pid=_dead_pid(), root=tmp_path)
        acquire_claim("good", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        def _archive_or_raise(path, ts_ms):
            if "bad" in str(path):
                raise OSError("simulated permission denied")
            return real_archive_claim(path, ts_ms=ts_ms)

        monkeypatch.setattr("fno.claims.core.archive_claim", _archive_or_raise)

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 1
        assert len(summary["reap_failed"]) == 1
        assert "bad" in summary["reap_failed"][0][0]
        assert "simulated permission denied" in summary["reap_failed"][0][1]
        assert claim_path("bad", root=tmp_path).exists()
        assert not claim_path("good", root=tmp_path).exists()

    def test_AC5_EDGE_reap_credited_even_if_key_recreated_right_after_archive(
        self, tmp_path, monkeypatch
    ):
        """A fresh, unrelated acquire_claim() recreating the same key the
        instant after the archive completes must not read as a failed move:
        entry.exists() flips back to True with no bearing on whether THIS
        call's own rename worked."""
        from fno.claims.io import archive_claim as real_archive_claim

        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        def _archive_then_race(path, ts_ms):
            result = real_archive_claim(path, ts_ms=ts_ms)
            acquire_claim("k", "other-holder", pid=os.getpid(), root=tmp_path)
            return result

        monkeypatch.setattr("fno.claims.core.archive_claim", _archive_then_race)

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 1
        assert summary["reap_failed"] == []
        status = claim_status("k", root=tmp_path)
        assert status["state"] == "live"
        assert status["holder"] == "other-holder"

    def test_AC6_EDGE_corrupted_lockfile_counted_never_reaped(self, tmp_path):
        cdir = claims_dir(tmp_path)
        cdir.mkdir(parents=True, exist_ok=True)
        bad = cdir / "node%3Ax-bad.lock"
        bad.write_text("this is not a claim\n")

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 0
        assert summary["corrupted"] == 1
        assert bad.exists()

    def test_dry_run_reports_would_reap_and_writes_nothing(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        # resolve_repo_root() is @cache'd process-wide; some earlier fixture's
        # first-use import can warm it against the ORIGINAL cwd before this
        # chdir runs (order-dependent - harmless everywhere except a test
        # that, like this one, needs append_event's cwd-derived events.jsonl
        # path to follow the chdir). Clear it post-chdir so the write (or
        # non-write, which is what this test asserts) lands under tmp_path.
        from fno.paths import resolve_repo_root
        resolve_repo_root.cache_clear()
        # The journal is pinned as well as the root. The hermetic sandbox sets
        # FNO_EVENTS_PATH for the whole pytest process and it is checked ahead
        # of the root, so a test reading the cwd-derived journal back has to
        # name that same file.
        monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        summary = reap_dead_claims(roots=[tmp_path], apply=False)

        assert summary["apply"] is False
        assert summary["would_reap"] == 1
        assert summary["reaped"] == 0
        assert claim_path("k", root=tmp_path).exists()

        import json

        events_path = tmp_path / ".fno" / "events.jsonl"
        # events.jsonl already exists from acquire_claim's own claim_acquired
        # emission above; what a dry run must NOT add is a claim_reap_swept
        # entry - that write would break `fno backlog reconcile --dry-run`'s
        # own preview contract.
        lines = [json.loads(line) for line in events_path.read_text().splitlines()]
        swept = [e for e in lines if e["type"] == "claim_reap_swept"]
        assert swept == []

    def test_second_apply_run_is_idempotent(self, tmp_path):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        first = reap_dead_claims(roots=[tmp_path], apply=True)
        second = reap_dead_claims(roots=[tmp_path], apply=True)

        assert first["reaped"] == 1
        assert second["reaped"] == 0
        assert second["scanned"] == 0, ".expired/ must never be rescanned"

    def test_AC8_swept_event_fires_on_a_zero_reap_run(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        # See the comment in test_dry_run_reports_would_reap_and_writes_nothing:
        # resolve_repo_root()'s process-wide cache can be warmed against the
        # pre-chdir cwd by an unrelated fixture's first-use import, order-
        # dependent on what ran earlier in the session. Order-dependent here
        # means this test passes as part of the suite but fails run alone.
        from fno.paths import resolve_repo_root
        resolve_repo_root.cache_clear()
        # The journal is pinned as well as the root. The hermetic sandbox sets
        # FNO_EVENTS_PATH for the whole pytest process and it is checked ahead
        # of the root, so a test reading the cwd-derived journal back has to
        # name that same file.
        monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
        import json

        reap_dead_claims(roots=[tmp_path], apply=True)  # empty root, nothing to reap

        events_path = tmp_path / ".fno" / "events.jsonl"
        assert events_path.exists(), "a silent sweep must still leave a trace"
        lines = [json.loads(line) for line in events_path.read_text().splitlines()]
        swept = [e for e in lines if e["type"] == "claim_reap_swept"]
        assert len(swept) == 1
        assert swept[0]["data"]["scanned"] == 0
        assert swept[0]["data"]["reaped"] == 0
        assert swept[0]["data"]["apply"] is True


# ---------------------------------------------------------------------------
# acquire_claim's idempotent re-acquire vs reap's recovery mutex: a
# respawned worker refreshing the same holder string must not have its live
# write clobbered by a concurrent reap sweep that proved the OLD pid dead.
# ---------------------------------------------------------------------------


class TestIdempotentReacquireVsReapRecoveryMutex:
    def test_idempotent_reacquire_waits_for_reaps_recovery_mutex(self, tmp_path):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)
        path = claim_path("k", root=tmp_path)
        recovery_lock = path.with_name(path.name + ".recovery.d")

        # Simulate reap holding this key's recovery mutex mid-archive (the
        # exact window between its re-verify and its archive_claim() call).
        token = acquire_dir_mutex(recovery_lock, 0)
        assert token is not None

        result: dict[str, object] = {}

        def _racer() -> None:
            result["claim"] = acquire_claim("k", HOLDER_A, pid=os.getpid(), root=tmp_path)

        racer = threading.Thread(target=_racer)
        racer.start()
        time.sleep(0.2)
        try:
            assert racer.is_alive(), "idempotent re-acquire must block on the held mutex"
            still_dead = read_claim_file(path)
            assert still_dead.pid == _dead_pid(), (
                "the on-disk claim must not have been rewritten while reap's "
                "recovery mutex was held - that write would race a concurrent "
                "archive"
            )
        finally:
            release_dir_mutex(recovery_lock, token)

        racer.join(timeout=5)
        assert not racer.is_alive()
        assert result["claim"].pid == os.getpid()

    def test_idempotent_reacquire_revalidates_stale_read_before_lock_acquired(
        self, tmp_path, monkeypatch
    ):
        """A different holder can win the key in the gap between acquire_claim's
        initial UNLOCKED read and its own (uncontended) recovery_lock.mkdir() -
        the winner's own mutex cycle has already finished and released by then,
        so our mkdir() succeeds immediately with no contention to wait out. The
        idempotent path must re-read under its own lock rather than trust the
        now-stale ``existing`` it read before ever touching the mutex."""
        from fno.claims import core as claims_core

        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)
        real_read = claims_core.read_claim_file
        calls = {"n": 0}

        def _read_then_race(p):
            calls["n"] += 1
            rec = real_read(p)
            if calls["n"] == 1 and rec.holder == HOLDER_A:
                # A different holder's stale-reclaim completes (and releases
                # its own recovery mutex) entirely between this unlocked read
                # and our own mkdir() attempt below.
                archive_claim(p, ts_ms=now_ms())
                acquire_claim("k", "other-holder", pid=os.getpid(), root=tmp_path)
            return rec

        monkeypatch.setattr(claims_core, "read_claim_file", _read_then_race)

        with pytest.raises(ClaimHeldByOther) as exc_info:
            acquire_claim("k", HOLDER_A, pid=os.getpid(), root=tmp_path)
        assert exc_info.value.holder == "other-holder"

        status = claim_status("k", root=tmp_path)
        assert status["holder"] == "other-holder", (
            "must not have overwritten the new holder's live claim with a "
            "stale-read idempotent rewrite"
        )


# ---------------------------------------------------------------------------
# refresh_claim vs reap's recovery mutex: a TTL claim reap has proven dead
# and is archiving must not be resurrected by a concurrent refresh.
# ---------------------------------------------------------------------------


class TestRefreshClaimVsReapRecoveryMutex:
    def test_refresh_waits_for_held_mutex_and_does_not_resurrect_archived_claim(
        self, tmp_path
    ):
        path = claim_path("k", root=tmp_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        expired = Claim(
            key="k",
            holder=HOLDER_A,
            acquired_at=now_ms() - 120_000,
            expires_at=now_ms() - 1_000,
            pid=_dead_pid(),
            host=socket.gethostname(),
        )
        path.write_text(serialize_claim(expired), encoding="utf-8")

        recovery_lock = path.with_name(path.name + ".recovery.d")
        token = acquire_dir_mutex(recovery_lock, 0)
        assert token is not None

        result: dict[str, object] = {}

        def _racer() -> None:
            try:
                result["claim"] = refresh_claim("k", HOLDER_A, root=tmp_path)
            except ClaimGoneAway as exc:
                result["error"] = exc

        racer = threading.Thread(target=_racer)
        racer.start()
        time.sleep(0.2)
        assert racer.is_alive(), "refresh must block on the held recovery mutex"

        # Simulate reap archiving this exact claim while refresh waits.
        archive_claim(path, ts_ms=now_ms())
        release_dir_mutex(recovery_lock, token)

        racer.join(timeout=5)
        assert not racer.is_alive()
        assert "error" in result, "must not resurrect a claim reap just archived"
        assert not path.exists()


# ---------------------------------------------------------------------------
# list_claims_with_counts / `fno agents claim list`: the reader must not lie
# ---------------------------------------------------------------------------


class TestReaderReportsFilteredCount:
    def test_AC7_counts_reconcile_with_lockfile_count(self, tmp_path):
        for i in range(5):
            acquire_claim(f"k{i}", HOLDER_A, pid=_dead_pid(), root=tmp_path)
        acquire_claim("k-live", HOLDER_A, pid=os.getpid(), root=tmp_path)

        rows, counts, states_by_key = list_claims_with_counts(root=tmp_path)

        on_disk = len(list(claims_dir(tmp_path).glob("*.lock")))
        assert on_disk == 6
        assert counts["total"] == 6
        assert counts["stale"] == 5
        assert counts["live"] == 1
        assert len(rows) == 1, "default include_stale=False shows only the live row"
        assert len(states_by_key) == 6, "states_by_key covers every entry, not just rows"
        assert states_by_key["k-live"] == "live"
        assert states_by_key["k0"] == "stale"

    def test_cli_list_does_not_print_bare_no_claims_string(self, cwd_tmp):
        for i in range(3):
            acquire_claim(f"k{i}", HOLDER_A, pid=_dead_pid(), root=cwd_tmp)

        result = runner.invoke(cli, ["list"])

        assert result.exit_code == 0
        assert result.output.strip() != "no claims"
        assert "no live claims (" in result.output
        assert "3 stale" in result.output

    def test_cli_list_labels_rows_with_root(self, cwd_tmp):
        acquire_claim("k-live", HOLDER_A, pid=os.getpid(), root=cwd_tmp)

        result = runner.invoke(cli, ["list"])

        assert result.exit_code == 0
        assert "root=" in result.output


# ---------------------------------------------------------------------------
# `fno agents claim reap` CLI verb
# ---------------------------------------------------------------------------


class TestReapCliVerb:
    def test_dry_run_is_the_default(self, cwd_tmp):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=cwd_tmp)

        result = runner.invoke(cli, ["reap"])

        assert result.exit_code == 0
        assert "would reap 1" in result.output
        assert claim_path("k", root=cwd_tmp).exists()

    def test_apply_archives_and_exits_zero(self, cwd_tmp):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=cwd_tmp)

        result = runner.invoke(cli, ["reap", "--apply"])

        assert result.exit_code == 0
        assert "reaped 1" in result.output
        assert not claim_path("k", root=cwd_tmp).exists()

    def test_apply_exits_nonzero_when_a_move_is_not_confirmed(self, cwd_tmp, monkeypatch):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=cwd_tmp)
        monkeypatch.setattr("fno.claims.core.archive_claim", lambda path, ts_ms: path)

        result = runner.invoke(cli, ["reap", "--apply"])

        assert result.exit_code == 1

    def test_json_output_is_parseable(self, cwd_tmp):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=cwd_tmp)
        import json

        result = runner.invoke(cli, ["reap", "--json"])

        assert result.exit_code == 0
        payload = json.loads(result.output)
        assert payload["would_reap"] == 1


# ---------------------------------------------------------------------------
# `fno backlog reconcile` calls the reaper (Change 6)
# ---------------------------------------------------------------------------


def test_reconcile_folds_the_reap_summary_into_its_json_payload(tmp_path, monkeypatch):
    """Wiring test only: cmd_reconcile must call reap_dead_claims and surface
    its summary, not re-verify the reaper's own behavior (covered above).

    The autouse `_hermetic_claim_reap` fixture (conftest.py) no-ops this leg
    for every OTHER reconcile test so none of them touch a real claims
    store; this test explicitly re-patches it with a distinguishable canned
    summary to prove the call site actually reaches fno.claims.core.
    """
    import json as _json

    import fno.graph._constants as gc
    import fno.graph.cli as graph_cli
    import fno.graph.store as gs
    import fno.claims.core as claims_core

    graph_path = tmp_path / "graph.json"
    graph_path.write_text(_json.dumps({"entries": []}) + "\n")
    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", tmp_path / "ledger.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)
    monkeypatch.setattr("fno.paths.retro_pending_dir", lambda: tmp_path / "retro")

    canned = {
        "scanned": 9, "reaped": 3, "would_reap": 0, "kept_live": 2,
        "kept_suspect": 1, "kept_offhost": 0, "corrupted": 0, "vanished": 0,
        "contended": 0, "reap_failed": [], "apply": True, "roots": ["/canned/root"],
    }
    monkeypatch.setattr(claims_core, "reap_dead_claims", lambda **kw: dict(canned))

    result = runner.invoke(graph_cli.cli, ["reconcile", "--json"])

    assert result.exit_code == 0, result.output
    payload = _json.loads(result.output)
    assert payload["claim_reap"]["outcome"] == "ok"
    assert payload["claim_reap"]["reaped"] == 3
    assert payload["claim_reap"]["scanned"] == 9


# ---------------------------------------------------------------------------
# a dead one-shot holder blocks nothing (x-05be change 3)
# ---------------------------------------------------------------------------


class TestTheSweepNeverReapsAnUnexpiredDispatchReservation:
    """A dead-pid `dispatch:` claim inside its TTL LOOKS like a pure wedge, and
    the sweep must still keep it.

    That TTL is the boot window. It outlives its spawner on purpose, so a second
    dispatcher does not launch onto a node whose worker has not yet reached `fno
    target init`. A background sweep reaping it collapses the dedup window, and
    it also voids the `dispatch:think:<node>:<reason>` tokens, which have no
    node claim behind them at all. The wedge is cleared at the spawn guard
    instead, where the caller is the next dispatcher rather than a sweep.
    """

    def _suspect(self, key):
        return Claim(
            key=key, holder="spawn-cli:1", acquired_at=now_ms(),
            expires_at=now_ms() + 180_000, pid=_dead_pid(),
            host=socket.gethostname(),
        )

    def test_a_dead_dispatch_reservation_is_kept_inside_its_ttl(self):
        provably_dead, bucket = classify_for_sweep(self._suspect("dispatch:x-05be"))
        assert (provably_dead, bucket) == (False, "suspect")

    def test_a_nested_dispatch_token_is_kept_too(self):
        """`dispatch:think:<node>:<reason>` is a dedup token with no node claim
        behind it, so reaping it early re-runs the work it deduplicated."""
        provably_dead, bucket = classify_for_sweep(
            self._suspect("dispatch:think:x-18ac:birth")
        )
        assert (provably_dead, bucket) == (False, "suspect")

    def test_an_expired_dispatch_reservation_is_still_reapable(self):
        """Expiry frees them on schedule, which is the whole recovery path the
        sweep is allowed to take."""
        claim = Claim(
            key="dispatch:x-old", holder="spawn-cli:1",
            acquired_at=now_ms() - 240_000, expires_at=now_ms() - 60_000,
            pid=_dead_pid(), host=socket.gethostname(),
        )
        assert is_provably_dead(claim) is True

    def test_a_node_key_is_kept_too(self):
        provably_dead, bucket = classify_for_sweep(self._suspect("node:x-05be"))
        assert (provably_dead, bucket) == (False, "suspect")

    def test_an_off_host_dispatch_reservation_is_still_opaque(self):
        claim = Claim(
            key="dispatch:x-far", holder="spawn-cli:1", acquired_at=now_ms(),
            expires_at=now_ms() + 180_000, pid=_dead_pid(),
            host="some-other-host", machine_id="not-this-machine",
        )
        provably_dead, bucket = classify_for_sweep(claim)
        assert (provably_dead, bucket) == (False, "offhost")


# ---------------------------------------------------------------------------
# the abandonment probe: reap only on a positive finding (x-05be change 2)
# ---------------------------------------------------------------------------


class TestAbandonmentProbe:
    def _suspect_node(self, tmp_path, key="node:x-gone"):
        """A SUSPECT node claim on disk: dead pid, TTL still open."""
        acquire_claim(
            key=key, holder="target-session:s", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )

    def test_without_a_probe_nothing_changes(self, tmp_path):
        """The parameter defaults to None so every existing caller is
        byte-for-byte unaffected."""
        self._suspect_node(tmp_path)
        summary = reap_dead_claims(roots=[tmp_path], apply=False)
        assert summary["would_reap"] == 0
        assert summary["kept_suspect"] == 1
        assert summary["kept_suspect_alive"] == 0
        assert summary["kept_suspect_unprobed"] == 0

    def test_a_proven_abandoned_node_claim_is_reaped(self, tmp_path):
        self._suspect_node(tmp_path)
        summary = reap_dead_claims(
            roots=[tmp_path], apply=False, abandonment_probe=lambda _c: True
        )
        assert summary["would_reap"] == 1
        assert summary["kept_suspect_alive"] == 0

    def test_a_live_worker_keeps_its_claim(self, tmp_path):
        """The x-ba4b regression guard. Archiving this claim is two live
        sessions in one worktree and a duplicate PR."""
        self._suspect_node(tmp_path)
        summary = reap_dead_claims(
            roots=[tmp_path], apply=True, abandonment_probe=lambda _c: False
        )
        assert summary["reaped"] == 0
        assert summary["kept_suspect_alive"] == 1
        assert claim_status("node:x-gone", root=tmp_path)["state"] == "suspect"

    def test_a_probe_that_could_not_run_keeps_the_claim(self, tmp_path):
        """unknown KEEPS. Reaping because a probe returned nothing is the exact
        inversion of this fix."""
        self._suspect_node(tmp_path)
        summary = reap_dead_claims(
            roots=[tmp_path], apply=True, abandonment_probe=lambda _c: None
        )
        assert summary["reaped"] == 0
        assert summary["kept_suspect_unprobed"] == 1
        assert claim_status("node:x-gone", root=tmp_path)["state"] == "suspect"

    def test_the_probe_is_never_asked_about_a_non_node_key(self, tmp_path):
        """No other key family has a roster to consult. The reservation is kept
        because its TTL is the boot window, not because the probe said so."""
        def _boom(_claim):
            raise AssertionError("probe asked about a non-node key")

        acquire_claim(
            key="dispatch:x-one", holder="spawn-cli:1", ttl_ms=180_000,
            pid=_dead_pid(), root=tmp_path,
        )
        summary = reap_dead_claims(
            roots=[tmp_path], apply=False, abandonment_probe=_boom
        )
        assert summary["would_reap"] == 0
        assert summary["kept_suspect"] == 1

    def test_the_probe_is_never_asked_about_a_live_claim(self, tmp_path):
        def _boom(_claim):
            raise AssertionError("probe asked about a live claim")

        acquire_claim(
            key="node:x-busy", holder="target-session:s", ttl_ms=3_600_000,
            pid=os.getpid(), root=tmp_path,
        )
        summary = reap_dead_claims(
            roots=[tmp_path], apply=False, abandonment_probe=_boom
        )
        assert summary["kept_live"] == 1


class TestCliProbeWiring:
    """The CLI is what injects the roster join, so the wiring needs its own
    coverage: a probe that exists but is never passed is a decorative guard."""

    def _fake_roster(self, monkeypatch, rows, warnings=()):
        def _fake(*_a, **_kw):
            return list(rows), list(warnings)

        monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _fake)

    def test_a_blind_roster_never_reaps_and_says_so(self, tmp_path, monkeypatch):
        acquire_claim(
            key="node:x-blind", holder="target-session:s", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        self._fake_roster(monkeypatch, rows=[], warnings=["claude not on PATH"])
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output
        assert "suspect (roster not consulted)" in r.output

    def test_an_empty_scan_never_reaps(self, tmp_path, monkeypatch):
        """Zero rows scanned is not a finding, even with no read error: there is
        nothing to have found the worker in."""
        acquire_claim(
            key="node:x-empty", holder="target-session:s", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        self._fake_roster(monkeypatch, rows=[])
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output
        assert "suspect (roster not consulted)" in r.output

    def _transcript_says(self, monkeypatch, finished):
        monkeypatch.setattr(
            "fno.claims.cli._transcript_says_finished", lambda *_a, **_kw: finished
        )

    def test_the_holder_found_and_finished_reaps(self, tmp_path, monkeypatch):
        """Abandonment is proven by FINDING the holder and seeing it stopped."""
        from fno.agents.watchdog import Row

        acquire_claim(
            key="node:x-abandoned", holder="target-session:sid-gone", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        self._fake_roster(
            monkeypatch,
            rows=[Row(row_id="sid-gone", name="t-gone", state="done", node="x-abandoned", cwd="")],
        )
        self._transcript_says(monkeypatch, True)
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 1" in r.output

    def test_a_terminal_row_whose_transcript_is_alive_is_never_reaped(
        self, tmp_path, monkeypatch
    ):
        """The row state alone cannot authorize a reap. `_TERMINAL_STATES`
        carries its own warning that the roster called a WORKING session done
        on 2026-08-15, and archiving on it hands a live worker's node to the
        next dispatcher - the `reaped_a_live_worker` kill criterion."""
        from fno.agents.watchdog import Row

        acquire_claim(
            key="node:x-lying", holder="target-session:sid-busy", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        self._fake_roster(
            monkeypatch,
            rows=[Row(row_id="sid-busy", name="t-busy", state="done", node="x-lying", cwd="")],
        )
        self._transcript_says(monkeypatch, False)
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output

    def test_the_holder_found_and_working_keeps(self, tmp_path, monkeypatch):
        from fno.agents.watchdog import Row

        acquire_claim(
            key="node:x-busy2", holder="target-session:sid-busy", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        self._fake_roster(
            monkeypatch,
            rows=[Row(row_id="sid-busy", name="t-busy2", state="working", node="x-busy2", cwd="")],
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output
        assert "suspect (worker alive)" in r.output

    def test_a_holder_absent_from_the_roster_is_never_reaped(self, tmp_path, monkeypatch):
        """THE P1 REGRESSION GUARD. fleet_rows enumerates claude rows only and
        drops interactive ones, so a codex worker, an opencode worker and any
        hand-started session are invisible to it BY CONSTRUCTION. Reading that
        absence as abandonment archives a live worker's claim. A row count
        validates the instrument, never the target."""
        from fno.agents.watchdog import Row

        acquire_claim(
            key="node:x-codex", holder="target-session:sid-codex", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        # Forty rows scanned, none of them able to represent this holder.
        self._fake_roster(
            monkeypatch,
            rows=[
                Row(row_id=f"other-{i}", name=f"t-{i}", state="working", node=f"x-{i}", cwd="")
                for i in range(40)
            ],
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output
        assert "suspect (roster not consulted)" in r.output

    def test_an_unparseable_holder_is_never_reaped(self, tmp_path, monkeypatch):
        from fno.agents.watchdog import Row

        acquire_claim(
            key="node:x-odd", holder="some-foreign-holder-shape", ttl_ms=3_600_000,
            pid=_dead_pid(), root=tmp_path,
        )
        self._fake_roster(
            monkeypatch,
            rows=[Row(row_id="a", name="t-a", state="done", node="x-odd", cwd="")],
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output


class TestExternalDeathEvidence:
    """The king's two 2026-08-23 specimens, replayed. Both held a claim whose
    holder was dead to every outside instrument - process table, registry row,
    mux pane - yet the reaper answered "I cannot tell" (suspect) and a manual
    --force release was the only way through. The fix feeds the reaper those
    instruments as positive findings; a claim that still cannot be proven
    dead keeps, exactly as before."""

    def _fake_roster(self, monkeypatch, rows, warnings=()):
        def _fake(*_a, **_kw):
            return list(rows), list(warnings)

        monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _fake)

    def _handover(self, tmp_path, *, pid=None, key="node:x-c272"):
        acquire_claim(
            key=key,
            holder="spawn-handover:bp-xc272-daemondrift",
            ttl_ms=900_000,  # inside the launch window: the old probe's blind spot
            pid=pid if pid is not None else _dead_pid(),
            root=tmp_path,
        )

    def _pane(self, monkeypatch, absent):
        monkeypatch.setattr(
            "fno.claims.cli._mux_pane_absent_for",
            lambda worker, node_id="", runner=None: absent,
        )

    def test_specimen_2_absent_pane_and_dead_pid_reaps(self, tmp_path, monkeypatch):
        """x-c272: the king killed the holder's pane and removed its registry
        row; the spawn-handover claim survived both and the next dispatch
        refused. Pane positively absent from the mux listing AND the recorded
        spawner pid dead is the launch window OVER - a positive finding."""
        self._pane(monkeypatch, absent=True)
        self._handover(tmp_path)
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 1" in r.output

    def test_handover_whose_pane_is_still_live_stays(self, tmp_path, monkeypatch):
        """The negative that keeps the fix honest: a pane still hosting the
        worker is the launch window OPEN, and the claim keeps."""
        self._pane(monkeypatch, absent=False)
        self._handover(tmp_path)
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output

    def test_handover_with_absent_pane_but_live_spawner_pid_stays(
        self, tmp_path, monkeypatch
    ):
        """Pane gone but the spawner process itself still runs: it may be
        mid-relaunch onto a new pane, so neither absence alone may reap."""
        self._pane(monkeypatch, absent=True)
        self._handover(tmp_path, pid=os.getpid())
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output

    def test_handover_when_the_mux_cannot_answer_stays(self, tmp_path, monkeypatch):
        """An unverifiable pane listing (None) is not absence: unknown keeps."""
        monkeypatch.setattr(
            "fno.claims.cli._mux_pane_absent_for",
            lambda worker, node_id="", runner=None: None,
        )
        self._handover(tmp_path)
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output

    def test_handover_in_place_policy_stays_unknown(self, tmp_path, monkeypatch):
        """An in-place worker's pane has no worker/worktree identity, so a
        missing match must not prove that its live pane is gone."""
        self._pane(monkeypatch, absent=True)
        monkeypatch.setattr(
            "fno.worktree_paths.resolve_worktree_policy",
            lambda *_a, **_kw: SimpleNamespace(policy="never"),
        )
        self._handover(tmp_path)
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output

    def test_specimen_1_degraded_then_recovered_roster_reaps(
        self, tmp_path, monkeypatch
    ):
        """x-3f84: the king killed the holder's whole tree and verified five
        pids gone; `claim reap` still reported `kept: 1 suspect (roster not
        consulted)` because one degraded roster read answered None for the
        whole pass. One retry on a degraded reading resolves it here."""
        from fno.agents.watchdog import Row

        calls = {"n": 0}

        def _flaky(*_a, **_kw):
            calls["n"] += 1
            if calls["n"] == 1:
                return [], ["claude not on PATH"]
            return (
                [
                    Row(
                        row_id="sid-3f84", name="t-3f84", state="done",
                        node="x-3f84", cwd="",
                    )
                ],
                [],
            )

        monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _flaky)
        monkeypatch.setattr(
            "fno.claims.cli._transcript_says_finished", lambda *_a, **_kw: True
        )
        acquire_claim(
            key="node:x-3f84", holder="target-session:sid-3f84",
            ttl_ms=3_600_000, pid=_dead_pid(), root=tmp_path,
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 1" in r.output
        assert calls["n"] == 2  # degraded once, retried once, settled

    def test_a_still_degraded_roster_after_retry_keeps(self, tmp_path, monkeypatch):
        """One retry, never a loop: a roster that fails twice is really
        unavailable, and the sweep reports it rather than papering over it."""
        calls = {"n": 0}

        def _always_degraded(*_a, **_kw):
            calls["n"] += 1
            return [], ["claude not on PATH"]

        monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _always_degraded)
        acquire_claim(
            key="node:x-dead-roster", holder="target-session:sid-1",
            ttl_ms=3_600_000, pid=_dead_pid(), root=tmp_path,
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output
        assert "roster not consulted" in r.output
        assert calls["n"] == 2

    def _claimed_no_row(self, tmp_path, *, metadata=None):
        acquire_claim(
            key="node:x-no-row", holder="target-session:sid-no-row",
            ttl_ms=3_600_000, pid=_dead_pid(), metadata=metadata, root=tmp_path,
        )

    def _roster_without_holder(self, monkeypatch):
        from fno.agents.watchdog import Row

        self._fake_roster(
            monkeypatch,
            rows=[
                Row(row_id=f"other-{i}", name=f"t-{i}", state="working",
                    node=f"x-{i}", cwd="")
                for i in range(3)
            ],
        )

    def test_dead_pid_with_worktree_metadata_and_finished_transcript_reaps(
        self, tmp_path, monkeypatch
    ):
        """The transcript fallback: the roster ran and has no row for the
        holder (the codex/hand-started coverage gap), the recorded pid is
        dead, and the claim itself carries the worktree the session's tree
        lives under. A finished tree is abandonment PROVEN, never inferred
        from the absent row."""
        self._roster_without_holder(monkeypatch)
        self._claimed_no_row(tmp_path, metadata={"worktree": "/tmp/wt-x"})
        monkeypatch.setattr(
            "fno.claims.cli._transcript_says_finished", lambda *_a, **_kw: True
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 1" in r.output

    def test_dead_pid_without_worktree_metadata_stays(self, tmp_path, monkeypatch):
        """No cwd to find the tree with: the probe still answers None. The
        fallback is only as live as the worktree stamp init now writes."""
        self._roster_without_holder(monkeypatch)
        self._claimed_no_row(tmp_path)
        monkeypatch.setattr(
            "fno.claims.cli._transcript_says_finished",
            lambda *_a, **_kw: (_ for _ in ()).throw(
                AssertionError("no transcript lookup without a worktree")
            ),
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output

    def test_dead_pid_with_live_transcript_stays(self, tmp_path, monkeypatch):
        """An unfinished transcript overrules the fallback: still working."""
        self._roster_without_holder(monkeypatch)
        self._claimed_no_row(tmp_path, metadata={"worktree": "/tmp/wt-x"})
        monkeypatch.setattr(
            "fno.claims.cli._transcript_says_finished", lambda *_a, **_kw: False
        )
        r = runner.invoke(cli, ["reap", "--root", str(tmp_path)])
        assert "would reap 0" in r.output


class TestMuxPaneAbsenceHelper:
    """_mux_pane_absent_for's own parsing rules, with a fake runner."""

    class _Proc:
        def __init__(self, rc, out):
            self.returncode = rc
            self.stdout = out

    def _runner(self, replies):
        calls = {"n": 0}

        def _run(argv, **_kw):
            idx = calls["n"]
            calls["n"] += 1
            return replies[idx]

        return _run

    def test_match_by_fno_id_or_title_means_present(self):
        from fno.claims.cli import _mux_pane_absent_for

        runner = self._runner(
            [
                self._Proc(0, '[{"session":"main","state":"live","panes":2}]'),
                self._Proc(
                    0,
                    '[{"pane_id":2,"fno_id":"other"},'
                    '{"pane_id":3,"fno_id":null,"title":"bp-x-worker"}]',
                ),
            ]
        )
        assert _mux_pane_absent_for("bp-x-worker", runner=runner) is False

    def test_match_by_worktree_cwd_basename_means_present(self):
        """The NORMAL live-launch marker: the pane's fno_id is the session
        UUID (not the worker name) and the title is whatever the shell set,
        but dispatch names the worker's worktree after the worker, so the
        pane's cwd basename is the reliable join. A miss here is the
        reaped-a-live-worker disaster."""
        from fno.claims.cli import _mux_pane_absent_for

        runner = self._runner(
            [
                self._Proc(0, '[{"session":"main","state":"live","panes":1}]'),
                self._Proc(
                    0,
                    '[{"pane_id":4,"fno_id":"01a0-fresh-uuid","title":null,'
                    '"cwd":"/Users/x/.fno/worktrees/footnote/bp-x-worker"}]',
                ),
            ]
        )
        assert _mux_pane_absent_for("bp-x-worker", runner=runner) is False

    def test_match_by_node_id_worktree_name_means_present(self):
        """The `target start` naming: a worktree named after the node id."""
        from fno.claims.cli import _mux_pane_absent_for

        runner = self._runner(
            [
                self._Proc(0, '[{"session":"main","state":"live","panes":1}]'),
                self._Proc(
                    0,
                    '[{"pane_id":5,"fno_id":null,"title":null,'
                    '"cwd":"/Users/x/.fno/worktrees/footnote/x-c272"}]',
                ),
            ]
        )
        assert _mux_pane_absent_for("bp-x-worker", node_id="x-c272", runner=runner) is False

    def test_nonempty_listing_without_the_worker_is_positive_absence(self):
        from fno.claims.cli import _mux_pane_absent_for

        runner = self._runner(
            [
                self._Proc(0, '[{"session":"main","state":"live","panes":1}]'),
                self._Proc(
                    0,
                    '[{"pane_id":2,"fno_id":"someone-else","title":null,'
                    '"cwd":"/Users/x/code/other"}]',
                ),
            ]
        )
        assert _mux_pane_absent_for("bp-x-worker", runner=runner) is True

    def test_empty_listing_is_unknown_not_absent(self):
        """`pane ls` prints [] both for no panes and for an unreachable
        session socket, so an empty listing proves nothing about absence."""
        from fno.claims.cli import _mux_pane_absent_for

        runner = self._runner(
            [
                self._Proc(0, '[{"session":"main","state":"live","panes":2}]'),
                self._Proc(0, "[]"),
            ]
        )
        assert _mux_pane_absent_for("bp-x-worker", runner=runner) is None

    def test_uninspectable_live_session_keeps_mixed_listing_unknown(self):
        """One unreadable live session invalidates absence from another
        session's unrelated pane listing."""
        from fno.claims.cli import _mux_pane_absent_for

        runner = self._runner(
            [
                self._Proc(
                    0,
                    '[{"session":"main","state":"live","panes":1},'
                    '{"session":"other","state":"live","panes":1}]',
                ),
                self._Proc(
                    0,
                    '[{"pane_id":2,"fno_id":"someone-else",'
                    '"title":null,"cwd":"/Users/x/code/other"}]',
                ),
                self._Proc(1, "pane socket unavailable"),
            ]
        )
        assert _mux_pane_absent_for("bp-x-worker", runner=runner) is None

    def test_no_live_sessions_is_unknown(self):
        from fno.claims.cli import _mux_pane_absent_for

        runner = self._runner([self._Proc(0, '[{"session":"main","state":"stale"}]')])
        assert _mux_pane_absent_for("bp-x-worker", runner=runner) is None
