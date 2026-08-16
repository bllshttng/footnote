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
from pathlib import Path

import psutil
import pytest
from typer.testing import CliRunner

from fno.claims.cli import cli
from fno.claims.core import (
    _classify_for_sweep,
    acquire_claim,
    list_claims_with_counts,
    reap_dead_claims,
)
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.staleness import is_provably_dead, now_ms
from fno.claims.types import Claim


HOLDER_A = "target-session:sid-a"
runner = CliRunner()


def _dead_pid() -> int:
    dead = 999_999
    while psutil.pid_exists(dead):
        dead += 1
    return dead


@pytest.fixture
def cwd_tmp(tmp_path: Path, monkeypatch):
    """Collapse both claims roots (global + cwd-local) onto one tmp dir.

    Mirrors the fixture in test_claims_cli.py: HOME=cwd means
    global_claims_root() and the canonical-repo-root claims_dir(None) are
    the SAME directory, so a test can write with plain acquire_claim(root=
    tmp_path) and know both of `list`/`reap`'s default roots see it.
    """
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    yield tmp_path


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
    """_classify_for_sweep (core.py) inlines is_provably_dead's own
    composition (same-machine and classify() is STALE) instead of calling
    it, so a sweep can share one classify() call across the outer scan and
    the mutex re-verify. A parity test pins the two
    together: if a future change to either composition diverges, this
    fails instead of reap silently disagreeing with its documented single
    liveness authority.
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
        provably_dead, _bucket = _classify_for_sweep(claim, ts)
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

    def test_AC6_EDGE_corrupted_lockfile_counted_never_reaped(self, tmp_path):
        cdir = claims_dir(tmp_path)
        cdir.mkdir(parents=True, exist_ok=True)
        bad = cdir / "node%3Ax-bad.lock"
        bad.write_text("this is not a claim\n")

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 0
        assert summary["corrupted"] == 1
        assert bad.exists()

    def test_dry_run_reports_would_reap_and_writes_nothing(self, tmp_path):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        summary = reap_dead_claims(roots=[tmp_path], apply=False)

        assert summary["apply"] is False
        assert summary["would_reap"] == 1
        assert summary["reaped"] == 0
        assert claim_path("k", root=tmp_path).exists()

    def test_second_apply_run_is_idempotent(self, tmp_path):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=tmp_path)

        first = reap_dead_claims(roots=[tmp_path], apply=True)
        second = reap_dead_claims(roots=[tmp_path], apply=True)

        assert first["reaped"] == 1
        assert second["reaped"] == 0
        assert second["scanned"] == 0, ".expired/ must never be rescanned"

    def test_AC8_swept_event_fires_on_a_zero_reap_run(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
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
# list_claims_with_counts / `fno claim list`: the reader must not lie
# ---------------------------------------------------------------------------


class TestReaderReportsFilteredCount:
    def test_AC7_counts_reconcile_with_lockfile_count(self, tmp_path):
        for i in range(5):
            acquire_claim(f"k{i}", HOLDER_A, pid=_dead_pid(), root=tmp_path)
        acquire_claim("k-live", HOLDER_A, pid=os.getpid(), root=tmp_path)

        rows, counts = list_claims_with_counts(root=tmp_path)

        on_disk = len(list(claims_dir(tmp_path).glob("*.lock")))
        assert on_disk == 6
        assert counts["total"] == 6
        assert counts["stale"] == 5
        assert counts["live"] == 1
        assert len(rows) == 1, "default include_stale=False shows only the live row"

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
        "reap_failed": [], "apply": True, "roots": ["/canned/root"],
    }
    monkeypatch.setattr(claims_core, "reap_dead_claims", lambda **kw: dict(canned))

    result = runner.invoke(graph_cli.cli, ["reconcile", "--json"])

    assert result.exit_code == 0, result.output
    payload = _json.loads(result.output)
    assert payload["claim_reap"]["outcome"] == "ok"
    assert payload["claim_reap"]["reaped"] == 3
    assert payload["claim_reap"]["scanned"] == 9
