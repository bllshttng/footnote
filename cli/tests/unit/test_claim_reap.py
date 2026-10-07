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

import json
import os
import socket
from pathlib import Path
from types import SimpleNamespace

import psutil
import pytest
from typer.testing import CliRunner

from fno.claims.cli import cli
from fno.claims.core import (
    acquire_claim,
    claim_status,
    reap_dead_claims,
    release_claim,
)
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.types import Claim, now_ms
from fno.claims.verdict import claim_verdicts


HOLDER_A = "target-session:sid-a"
runner = CliRunner()


@pytest.fixture(autouse=True)
def _real_reap_for_verb_tests(monkeypatch):
    """These tests drive the REAL reap against tmp roots, so the conftest's
    hermetic reap stub must not reach them.

    It never could while the CLI verbs captured core callables at import; the
    CLI now resolves them at call time, which is what makes the stub visible
    here. Restore the function from this file's own collection-time binding -
    the same reference the conftest fixture already documents as unaffected.
    """
    import fno.claims.core as claims_core

    monkeypatch.setattr(claims_core, "reap_dead_claims", reap_dead_claims)


def _dead_pid() -> int:
    dead = 999_999
    while psutil.pid_exists(dead):
        dead += 1
    return dead


def _native_sweep_verdict(claim: Claim) -> tuple[bool, str]:
    """Exercise the Rust decision door for a synthetic claim fixture."""
    from tempfile import TemporaryDirectory

    with TemporaryDirectory() as raw_root:
        root = Path(raw_root)
        path = claim_path(claim.key, root=root)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(claim), encoding="utf-8")
        row = claim_verdicts([claim.key], root=root).get(claim.key)
        assert row is not None, f"native door omitted {claim.key}"
        return bool(row["provably_dead"]), str(row["bucket"] or "")


def classify_for_sweep(claim: Claim, _now: int | None = None) -> tuple[bool, str]:
    return _native_sweep_verdict(claim)


def is_provably_dead(claim: Claim, now: int | None = None) -> bool:
    return _native_sweep_verdict(claim)[0]


# ---------------------------------------------------------------------------
# is_provably_dead: the native verdict door
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# reap_dead_claims: the reaper (core.py)
# ---------------------------------------------------------------------------


class TestReapDeadClaims:
    def test_node_release_does_not_open_or_write_the_graph(self, tmp_path, monkeypatch):
        node_id = "x-release-lockfile-only"
        holder = "target-session:release-lockfile-only"
        graph_path = tmp_path / "configured-graph.json"
        graph_path.write_text(json.dumps({"entries": [{"id": node_id, "locked_by": "old-copy"}]}) + "\n")
        before = graph_path.read_text()
        monkeypatch.setattr(
            "fno.paths.graph_json",
            lambda: pytest.fail("claim release must not open the graph"),
        )

        claim = acquire_claim(f"node:{node_id}", holder, pid=os.getpid(), root=tmp_path)
        released = release_claim(claim.key, holder, root=tmp_path)

        assert isinstance(released, Claim)
        assert claim_status(claim.key, root=tmp_path)["state"] == "free"
        assert graph_path.read_text() == before

    def test_live_claim_kept_live(self, tmp_path):
        acquire_claim("k", HOLDER_A, pid=os.getpid(), root=tmp_path)

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 0
        assert summary["kept_live"] == 1

    def test_reap_row_names_the_native_verdict_basis(self, tmp_path, monkeypatch):
        import fno.claims.core as claims_core
        from fno.claims import events as claim_events
        from fno.events import validate

        acquire_claim("node:x-absent", HOLDER_A, pid=_dead_pid(), root=tmp_path)
        verdict = {
            "state": "stale",
            "basis": "session-absent",
            "bucket": "",
            "provably_dead": True,
        }
        monkeypatch.setattr(
            claims_core, "claim_verdicts", lambda *a, **k: {"node:x-absent": verdict}
        )
        emitted: list[dict] = []
        monkeypatch.setattr(claim_events, "_emit", emitted.append)

        summary = reap_dead_claims(roots=[tmp_path], apply=True)

        assert summary["reaped"] == 1
        reaped = [e for e in emitted if e["type"] == "claim_reaped"]
        assert len(reaped) == 1
        assert reaped[0]["data"]["basis"] == "session-absent"
        validate(reaped[0])

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
        # The journal is pinned as well as the root. The hermetic sandbox sets
        # FNO_EVENTS_PATH for the whole pytest process and it is checked ahead
        # of the root, so a test reading the cwd-derived journal back has to
        # name that same file.
        monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
        reap_dead_claims(roots=[tmp_path], apply=True)  # empty root, nothing to reap

        from fno.events.store_client import store_db_path

        events_path = tmp_path / ".fno" / "events.jsonl"
        assert store_db_path(events_path).exists(), "a silent sweep must still leave a trace"
        from tests._event_rows import event_rows

        swept = [e for e in event_rows(events_path) if e["type"] == "claim_reap_swept"]
        assert len(swept) == 1
        assert swept[0]["data"]["scanned"] == 0
        assert swept[0]["data"]["reaped"] == 0
        assert swept[0]["data"]["apply"] is True


# ---------------------------------------------------------------------------
# acquire_claim's idempotent re-acquire vs reap's recovery mutex: a
# respawned worker refreshing the same holder string must not have its live
# write clobbered by a concurrent reap sweep that proved the OLD pid dead.
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# refresh_claim vs reap's recovery mutex: a TTL claim reap has proven dead
# and is archiving must not be resurrected by a concurrent refresh.
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# list_claims_with_counts / `fno agents claim list`: the reader must not lie
# ---------------------------------------------------------------------------


class TestReaderReportsFilteredCount:
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
    def test_json_output_is_parseable(self, cwd_tmp):
        acquire_claim("k", HOLDER_A, pid=_dead_pid(), root=cwd_tmp)
        import json

        result = runner.invoke(cli, ["reap", "--json"])

        assert result.exit_code == 0
        payload = json.loads(result.output)
        assert payload["would_reap"] == 1


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
            roots=[tmp_path], apply=False, abandonment_probe=lambda _c, **_: True
        )
        assert summary["would_reap"] == 1
        assert summary["kept_suspect_alive"] == 0

    def test_a_live_worker_keeps_its_claim(self, tmp_path):
        """The x-ba4b regression guard. Archiving this claim is two live
        sessions in one worktree and a duplicate PR."""
        self._suspect_node(tmp_path)
        summary = reap_dead_claims(
            roots=[tmp_path], apply=True, abandonment_probe=lambda _c, **_: False
        )
        assert summary["reaped"] == 0
        assert summary["kept_suspect_alive"] == 1
        assert claim_status("node:x-gone", root=tmp_path)["state"] == "suspect"

    def test_a_probe_that_could_not_run_keeps_the_claim(self, tmp_path):
        """unknown KEEPS. Reaping because a probe returned nothing is the exact
        inversion of this fix."""
        self._suspect_node(tmp_path)
        summary = reap_dead_claims(
            roots=[tmp_path], apply=True, abandonment_probe=lambda _c, **_: None
        )
        assert summary["reaped"] == 0
        assert summary["kept_suspect_unprobed"] == 1
        assert claim_status("node:x-gone", root=tmp_path)["state"] == "suspect"

    def test_the_probe_is_never_asked_about_a_non_node_key(self, tmp_path):
        """No other key family has a roster to consult. The reservation is kept
        because its TTL is the boot window, not because the probe said so."""
        def _boom(_claim, **_):
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
        def _boom(_claim, **_):
            raise AssertionError("probe asked about a live claim")

        acquire_claim(
            key="node:x-busy", holder="target-session:s", ttl_ms=3_600_000,
            pid=os.getpid(), root=tmp_path,
        )
        summary = reap_dead_claims(
            roots=[tmp_path], apply=False, abandonment_probe=_boom
        )
        assert summary["kept_live"] == 1


class TestSharedPidExclusivity:
    """The 2026-08-29 specimen: seven expired claims from distinct sessions,
    one live daemon pid, every one prover-proven, so the corroborated hybrid
    arm kept them all LIVE forever and the reaper reported 0 of 14. Provenance
    was honest - the prover truthfully resolves every daemon-hosted session to
    the daemon - so the missing predicate is EXCLUSIVITY: a pid that answers
    for more than one distinct holder is not session evidence. The sweep
    derives that property from the claim records it is already reading. No
    harness name appears anywhere in it: a daemon-hosted session is a substrate
    property, not any one harness's (the doctor_footprint lesson, PR 1295).
    """

    @staticmethod
    def _expired_prover_on_disk(root, key, holder, pid=os.getpid()):
        """An expired TTL claim on disk whose live pid is prover-proven.

        ``acquired_at`` must postdate this process's create_time or is_live
        reads the pid as reused and the claim dies for an unrelated reason.
        acquire_claim cannot write this shape (its TTL is always future), so
        the claim is serialized directly.
        """
        started = int(psutil.Process(os.getpid()).create_time() * 1000)
        claim = Claim(
            key=key, holder=holder, acquired_at=started + 1,
            expires_at=now_ms() - 60_000, pid=pid,
            host=socket.gethostname(), pid_provenance="session-prover",
        )
        path = claim_path(key, root=root)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialize_claim(claim), encoding="utf-8")
        return path

    def test_two_expired_claims_sharing_one_live_pid_are_not_immortal(self, tmp_path):
        """Distinct holders, one live pid: the pid alone cannot keep either
        LIVE. Both fall to the suspect route where the secondary evidence
        decides - here it proves abandonment, so both are reapable."""
        self._expired_prover_on_disk(tmp_path, "node:x-one", "target-session:s1")
        self._expired_prover_on_disk(tmp_path, "node:x-two", "target-session:s2")
        summary = reap_dead_claims(
            roots=[tmp_path], apply=False, abandonment_probe=lambda _c, **_: True
        )
        assert summary["would_reap"] == 2
        assert summary["kept_live"] == 0

    def test_same_holder_claims_sharing_a_pid_stay_live(self, tmp_path):
        """Exclusivity counts DISTINCT holders. One session's own claims on
        its one prover pid are the corroborated hybrid arm's legitimate case
        and must not be demoted by counting claim files."""
        self._expired_prover_on_disk(tmp_path, "node:x-a", "target-session:s1")
        self._expired_prover_on_disk(tmp_path, "node:x-b", "target-session:s1")
        summary = reap_dead_claims(roots=[tmp_path], apply=False)
        assert summary["kept_live"] == 2
        assert summary["kept_suspect"] == 0

    def test_without_a_probe_the_shared_shape_keeps(self, tmp_path):
        """No secondary evidence supplied: unknown keeps, the pre-existing
        doctrine, now visible as suspect instead of a false live."""
        self._expired_prover_on_disk(tmp_path, "node:x-one", "target-session:s1")
        self._expired_prover_on_disk(tmp_path, "node:x-two", "target-session:s2")
        summary = reap_dead_claims(roots=[tmp_path], apply=False)
        assert summary["would_reap"] == 0
        assert summary["kept_suspect"] == 2
        assert summary["kept_live"] == 0

class TestCliProbeWiring:
    """The CLI is what injects the roster join, so the wiring needs its own
    coverage: a probe that exists but is never passed is a decorative guard."""

    def _fake_roster(self, monkeypatch, rows, warnings=()):
        def _fake(*_a, **_kw):
            return list(rows), list(warnings)

        monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _fake)

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
        assert "probe unanswered: row-absent-no-cwd 1" in r.output

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
    """The lead's two 2026-08-23 specimens, replayed. Both held a claim whose
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
        """x-c272: the lead killed the holder's pane and removed its registry
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
        """x-3f84: the lead killed the holder's whole tree and verified five
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


class TestSweepReadsWalkedDir:
    """AC1/AC2 (x-9c91): the sweep asks the native door about the directory it
    walks, and a claim with no verdict there is unclassified, never unprobed.

    The old root=cdir.parent.parent round-trip re-resolved the space claims
    dir one level down, so all 19 space-root claims got no verdict and fell
    to unknown-keeps labelled "roster not consulted" - a cause nobody
    measured."""

    def test_AC1_HP_space_root_claim_gets_native_verdict(self, tmp_path, monkeypatch):
        import fno.claims.core as claims_core

        monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
        # The space root rides env: the native acquire below resolves it in
        # the binary's process, where a Python path-symbol patch is invisible.
        monkeypatch.setenv("FNO_SPACES_DIR", str(tmp_path / "spaces"))
        space = claims_dir().parent
        # Session-prover pid not running, no session id: the shape the
        # native classifier reads Stale on its own (dead pid proves death on
        # this machine regardless of the TTL arm).
        acquire_claim(
            "reap:pr-1496", "reap:pr-1496", pid=_dead_pid(), root=None
        )
        asked: list = []
        real_door = claims_core.claim_verdicts

        def _recording_door(keys=None, *, prefix=None, root=None, claims_dir_path=None):
            asked.append(claims_dir_path)
            return real_door(keys, prefix=prefix, root=root, claims_dir_path=claims_dir_path)

        monkeypatch.setattr(claims_core, "claim_verdicts", _recording_door)
        summary = reap_dead_claims(roots=[None], apply=False)
        assert asked == [space / "claims"], "the door must be asked about the walked dir"
        assert summary["would_reap"] == 1
        assert summary["kept_unclassified"] == 0

    def test_AC2_ERR_sweep_event_carries_the_new_buckets(self, monkeypatch):
        from fno.claims import events as claim_events

        captured: dict = {}
        monkeypatch.setattr(claim_events, "_emit", lambda event: captured.update(event))
        summary = {
            "scanned": 1, "reaped": 0, "would_reap": 0, "kept_live": 0,
            "kept_suspect": 0, "kept_suspect_alive": 0, "kept_suspect_unprobed": 1,
            "kept_unclassified": 2, "unclassified_dirs": {"/claims": 2},
            "kept_suspect_unprobed_by": {"roster-read-degraded": 1},
            "kept_offhost": 0, "corrupted": 0, "vanished": 0, "contended": 0,
            "reap_failed": [], "apply": True,
            "roots": ["/claims"],
        }
        claim_events.emit_claim_reap_swept(summary)
        data = captured["data"]
        assert data["kept_unclassified"] == 2
        assert data["unclassified_dirs"] == {"/claims": 2}
        assert data["kept_suspect_unprobed_by"] == {"roster-read-degraded": 1}
