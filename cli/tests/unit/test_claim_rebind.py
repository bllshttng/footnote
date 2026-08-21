"""Tests for ``claims.core.compare_and_rebind`` (x-2ccd wave 1).

The atomic same-session claim rebind: the full result matrix from the design
doc. ``emit=False`` keeps these off the real events log; the schema/parity
tests cover the audit row separately.
"""
from __future__ import annotations

import os
import socket
import time

import pytest

from fno.claims import RebindRefused, claim_status
from fno.claims.core import compare_and_rebind
from fno.claims.hostid import machine_id
from fno.claims.io import claim_path, serialize_claim
from fno.claims.staleness import now_ms
from fno.claims.types import Claim

HOLDER = "target-session:507dddb2-4559-4faa-8e34-ab94d627da8e"
KEY = "node:x-2ccd"
# A pid that does not exist: is_live reads it as dead.
_DEAD_PID = 9_999_999


def _write(root, claim: Claim) -> None:
    path = claim_path(KEY, root=root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(claim), encoding="utf-8")


def _claim(
    pid: int,
    acquired_at: int,
    *,
    expires_at=None,
    host=None,
    mid=None,
    holder=HOLDER,
) -> Claim:
    return Claim(
        schema_version=1,
        key=KEY,
        holder=holder,
        acquired_at=acquired_at,
        expires_at=expires_at,
        pid=pid,
        host=host if host is not None else socket.gethostname(),
        machine_id=mid if mid is not None else (machine_id() or None),
    )


def test_rebind_local_dead_stale_pid_is_rebound(tmp_path):
    """AC1: a local same-holder STALE claim (dead prior pid, expired TTL) rebinds."""
    prior = _claim(_DEAD_PID, now_ms() - 200_000, expires_at=now_ms() - 100_000)
    _write(tmp_path, prior)
    new_pid = os.getpid()
    claim, mode = compare_and_rebind(KEY, HOLDER, new_pid=new_pid, root=tmp_path, emit=False)
    assert mode == "rebound"
    assert claim.pid == new_pid
    assert claim.holder == HOLDER
    assert claim_status(KEY, root=tmp_path)["state"] == "live"


def test_rebind_local_dead_suspect_pid_is_rebound(tmp_path):
    """AC1: a local SUSPECT claim (TTL unexpired, dead pid) also rebinds."""
    prior = _claim(_DEAD_PID, now_ms(), expires_at=now_ms() + 60_000)
    _write(tmp_path, prior)
    claim, mode = compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert mode == "rebound"
    assert claim.pid == os.getpid()


def test_rebind_live_same_pid_is_idempotent_lease_refresh(tmp_path):
    """A LIVE claim on THIS pid refreshes the lease (idempotent), no pid change."""
    me = os.getpid()
    # acquired_at = now: this process started at/before now, so is_live is True
    # (an acquired_at in the recent past would read as PID reuse for a freshly
    # spawned pytest process).
    prior = _claim(me, now_ms(), expires_at=now_ms() + 30_000)
    _write(tmp_path, prior)
    before_exp = claim_status(KEY, root=tmp_path)["expires_at"]
    time.sleep(0.01)
    claim, mode = compare_and_rebind(KEY, HOLDER, new_pid=me, root=tmp_path, emit=False)
    assert mode == "idempotent"
    assert claim.pid == me
    assert claim.expires_at > before_exp


def test_rebind_live_different_pid_refuses_concurrent_writer(tmp_path):
    """AC2: a LIVE claim on a DIFFERENT pid refuses (concurrent writer of one session)."""
    other = os.getppid()
    if other == os.getpid():
        pytest.skip("no distinct live pid available on this platform")
    prior = _claim(other, now_ms(), expires_at=now_ms() + 60_000)
    _write(tmp_path, prior)
    with pytest.raises(RebindRefused) as exc:
        compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert exc.value.state == "live"
    assert exc.value.pid == other


def test_rebind_offhost_dead_pid_refuses_death_unproven(tmp_path):
    """AC3: a dead pid on ANOTHER machine refuses (death unproven -> fail closed)."""
    prior = _claim(
        _DEAD_PID,
        now_ms(),
        expires_at=now_ms() + 60_000,
        mid="00000000-0000-0000-0000-000000000000",
    )
    _write(tmp_path, prior)
    with pytest.raises(RebindRefused) as exc:
        compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert exc.value.state in {"suspect", "stale"}


def test_rebind_holder_mismatch_refuses(tmp_path):
    """AC3: a claim held by a different holder refuses, unchanged."""
    prior = _claim(
        _DEAD_PID, now_ms(), expires_at=now_ms() + 60_000,
        holder="target-session:someone-else",
    )
    _write(tmp_path, prior)
    with pytest.raises(RebindRefused) as exc:
        compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert "holder mismatch" in exc.value.reason


def test_rebind_waits_out_brief_recovery_mutex_contention_instead_of_refusing(tmp_path):
    """A recovery mutex held briefly by a peer (acquire_claim/reap mid-archive)
    must not refuse the rebind - it waits (acquire_dir_mutex, same as
    acquire_claim/refresh_claim), then proceeds once the peer releases.

    Contention handling here changed from "one steal attempt, else refuse"
    to acquire_dir_mutex's steal-or-poll-until-timeout: a mutex that clears
    well inside the 5s window now lets the rebind succeed instead of forcing
    the caller to retry the whole resume bind for what was a few-ms window.
    """
    prior = _claim(_DEAD_PID, now_ms() - 200_000, expires_at=now_ms() - 100_000)
    _write(tmp_path, prior)
    path = claim_path(KEY, root=tmp_path)
    recovery_lock = path.with_name(path.name + ".recovery.d")
    recovery_lock.mkdir(parents=True)
    (recovery_lock / "owner").write_text("peer-token")

    import threading

    result = {}

    def _rebind():
        try:
            result["claim"], result["mode"] = compare_and_rebind(
                KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False
            )
        except RebindRefused as exc:
            result["error"] = exc

    racer = threading.Thread(target=_rebind)
    racer.start()
    time.sleep(0.2)
    assert racer.is_alive(), "rebind returned before the mutex was released"

    import shutil

    shutil.rmtree(recovery_lock)
    racer.join(timeout=6)

    assert "error" not in result, f"rebind refused instead of waiting: {result.get('error')}"
    assert result["mode"] == "rebound"
    assert result["claim"].pid == os.getpid()


def test_rebind_missing_claim_refuses_and_never_creates(tmp_path):
    """AC3: a free/missing claim refuses and never creates one."""
    with pytest.raises(RebindRefused) as exc:
        compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert exc.value.state == "free"
    assert claim_status(KEY, root=tmp_path)["state"] == "free"


def test_rebind_corrupt_claim_refuses(tmp_path):
    """AC3: an unparseable claim refuses rather than guessing."""
    path = claim_path(KEY, root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{{{not yaml", encoding="utf-8")
    with pytest.raises(RebindRefused) as exc:
        compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert exc.value.state == "corrupted"


def test_rebound_pid_liveness_claim_stays_pid_liveness(tmp_path):
    """Invariant: rebind preserves holder; a PID-liveness claim stays PID-liveness."""
    prior = _claim(_DEAD_PID, now_ms() - 200_000)  # no expires_at -> PID-liveness
    _write(tmp_path, prior)
    claim, mode = compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert mode == "rebound"
    assert claim.holder == HOLDER
    assert claim.expires_at is None


def test_rebind_ttl_window_does_not_compound(tmp_path):
    """A TTL rebind pins expires_at to now+window, never a growing span (renew lesson)."""
    window = 120_000
    acq = now_ms() - 200_000
    prior = _claim(_DEAD_PID, acq, expires_at=acq + window)  # window length == `window`
    _write(tmp_path, prior)
    t0 = now_ms()
    claim, _ = compare_and_rebind(KEY, HOLDER, new_pid=os.getpid(), root=tmp_path, emit=False)
    assert abs(claim.expires_at - (t0 + window)) < 5_000


def test_a_live_handover_needs_a_launch_window_holder(tmp_path):
    """Naming the prior holder is the proof, and it is only proof for a holder
    that is not published. `fno agents claim status` prints every other one, so without
    this gate anyone could read a live worker's holder off the store and hand
    the node to themselves - taking a running owner's claim."""
    prior = _claim(os.getpid(), now_ms(), expires_at=now_ms() + 60_000,
                   holder="target-session:sid-live")
    _write(tmp_path, prior)
    before = claim_status(KEY, root=tmp_path)
    with pytest.raises(RebindRefused):
        compare_and_rebind(
            KEY, "target-session:sid-live", new_holder="target-session:sid-thief",
            new_pid=os.getpid(), root=tmp_path, emit=False,
        )
    # REFUSED, not quietly downgraded. Dropping only the rename let the call
    # fall into the same-holder rebind, which rewrote the victim's pid and
    # republished their claim as LIVE under the caller - unreapable, because
    # `sweep_verdict` short-circuits on LIVE, and benign-looking to every
    # dispatcher. Nothing on disk may change.
    after = claim_status(KEY, root=tmp_path)
    assert after["holder"] == "target-session:sid-live"
    assert after["pid"] == before["pid"]
    assert after.get("expires_at") == before.get("expires_at")


def test_a_live_launch_window_holder_still_hands_over(tmp_path):
    """The case the branch exists for. `--substrate headless` keeps the spawner
    alive for the worker's whole run, so its claim is LIVE when the worker
    reaches init. Refusing there left the worker unclaimed for the full lease."""
    prior = _claim(os.getpid(), now_ms(), expires_at=now_ms() + 60_000,
                   holder="spawn-handover:t-worker")
    _write(tmp_path, prior)
    claim, mode = compare_and_rebind(
        KEY, "spawn-handover:t-worker", new_holder="target-session:sid-worker",
        new_pid=os.getpid(), root=tmp_path, emit=False,
    )
    assert mode == "handover"
    assert claim.holder == "target-session:sid-worker"
