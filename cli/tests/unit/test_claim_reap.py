"""Unit tests for claim GC (reap) and the list reader's honesty.

Two things had no test before this: nothing pruned a claim whose holder died
without releasing (so a leaked lockfile stayed forever), and `fno claim list`
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
# list_claims_with_counts / `fno claim list`: the reader must not lie
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
# `fno claim reap` CLI verb
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
