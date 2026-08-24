"""Unit tests for fno.claims.staleness: PID-liveness + TTL expiry."""
from __future__ import annotations

import os
import socket
import time
from unittest.mock import patch

import psutil
import pytest

from fno.claims import hostid
from fno.claims.staleness import (
    classify,
    is_expired,
    is_live,
    now_ms,
)
from fno.claims.types import Claim, ClaimState


def _live_claim(**overrides) -> Claim:
    pid = os.getpid()
    proc_create_ms = int(psutil.Process(pid).create_time() * 1000)
    # Mark the claim as created AFTER the process started so it's "ours".
    defaults: dict = {
        "key": "k",
        "holder": "h",
        "acquired_at": proc_create_ms + 1000,
        "expires_at": None,
        "pid": pid,
        "host": socket.gethostname(),
    }
    defaults.update(overrides)
    return Claim(**defaults)


# ---------------------------------------------------------------------------
# is_live
# ---------------------------------------------------------------------------


def test_is_live_returns_true_for_current_process():
    claim = _live_claim()
    assert is_live(claim) is True


def test_is_live_returns_false_for_remote_host():
    claim = _live_claim(host="some-other-host-that-does-not-exist")
    assert is_live(claim) is False


def test_is_live_returns_false_when_pid_does_not_exist():
    # Pick a PID that definitely doesn't exist - very high numbers are safe.
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid)
    assert is_live(claim) is False


def test_is_live_false_when_pid_reused():
    """PID-reuse: process create_time AFTER acquired_at means the slot was recycled."""
    pid = os.getpid()
    proc_create_ms = int(psutil.Process(pid).create_time() * 1000)
    # Claim acquired BEFORE the process started => stale (different process).
    claim = Claim(
        key="k",
        holder="h",
        acquired_at=proc_create_ms - 10_000,
        expires_at=None,
        pid=pid,
        host=socket.gethostname(),
    )
    assert is_live(claim) is False


# ---------------------------------------------------------------------------
# is_expired
# ---------------------------------------------------------------------------


def test_is_expired_true_when_past():
    claim = _live_claim(expires_at=now_ms() - 1000)
    assert is_expired(claim) is True


def test_is_expired_false_when_future():
    claim = _live_claim(expires_at=now_ms() + 60_000)
    assert is_expired(claim) is False


def test_is_expired_false_for_pid_liveness_claim():
    """PID-liveness claims (expires_at=None) are NEVER expired by this check."""
    claim = _live_claim(expires_at=None)
    assert is_expired(claim) is False


def test_is_expired_at_exactly_now_is_expired():
    now = 1747641600000
    claim = _live_claim(expires_at=now)
    assert is_expired(claim, now=now) is True


# ---------------------------------------------------------------------------
# classify
# ---------------------------------------------------------------------------


def test_classify_live_pid_liveness():
    claim = _live_claim()
    assert classify(claim) == ClaimState.LIVE


def test_classify_stale_pid_liveness_dead_process():
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid)
    assert classify(claim) == ClaimState.STALE


def test_classify_stale_ttl_expired_dead_pid():
    """Expired TTL + dead recorded pid -> STALE (byte-for-byte today)."""
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid, expires_at=now_ms() - 1000)
    assert classify(claim) == ClaimState.STALE


def test_classify_live_ttl_expired_with_live_prover_pid():
    """CORROBORATED HYBRID ARM: an expired TTL claim whose recorded pid is a
    live process on this host (create_time < acquired_at) AND was prover-proven
    at write time is LIVE, not STALE - the session is alive (incl. suspended)
    even though the clock lapsed. refresh_claim has no automatic caller, so
    this arm is what keeps a long live session's claim LIVE past its TTL
    window; only its foreign-pid case was the defect."""
    claim = _live_claim(
        expires_at=now_ms() - 1000, pid_provenance="session-prover"
    )  # live pid via _live_claim
    assert classify(claim) == ClaimState.LIVE


def test_classify_stale_ttl_expired_with_live_foreign_pid():
    """THE SPECIMEN (PR 1031): an expired TTL claim whose recorded pid is a
    live process the holder does not own (a chat app's app-server) reads
    STALE, not LIVE. A live pid alone no longer extends the lease - only a
    pid proven to be the holder session's own process does, or the TTL is a
    lease in name only. Provenance here is explicitly "ambient"."""
    claim = _live_claim(expires_at=now_ms() - 1000, pid_provenance="ambient")
    assert classify(claim) == ClaimState.STALE


def test_classify_stale_ttl_expired_live_pid_legacy_claim():
    """A legacy claim written before pid_provenance existed (field absent)
    reads STALE at expiry, exactly as a pre-hybrid claim did: a claim that
    cannot prove its pid cannot outrank its own TTL."""
    claim = _live_claim(expires_at=now_ms() - 1000)  # no pid_provenance field
    assert claim.pid_provenance is None
    assert classify(claim) == ClaimState.STALE


def test_classify_live_ttl_unexpired_with_live_pid_regardless_of_provenance():
    """The corroboration gate touches ONLY the expired arm: inside the TTL
    window a live pid reads LIVE whether or not its provenance is proven (the
    TTL itself is the protection; provenance governs only past-expiry
    extension)."""
    claim = _live_claim(expires_at=now_ms() + 60_000, pid_provenance="ambient")
    assert classify(claim) == ClaimState.LIVE


def test_classify_stale_ttl_expired_pid_reused():
    """AC4-EDGE: expired TTL + pid whose process started AFTER acquired_at
    (pid reuse) -> STALE. The is_live create_time guard prevents a false-live."""
    pid = os.getpid()
    proc_create_ms = int(psutil.Process(pid).create_time() * 1000)
    claim = Claim(
        key="k",
        holder="h",
        acquired_at=proc_create_ms - 10_000,  # claim filed BEFORE this proc -> reuse
        expires_at=now_ms() - 1000,
        pid=pid,
        host=socket.gethostname(),
    )
    assert classify(claim) == ClaimState.STALE


def test_classify_stale_ttl_expired_remote_host():
    """AC2-FR: an expired TTL claim on another host is STALE - the pid arm is
    same-host-only and does not falsely keep a remote claim alive."""
    claim = _live_claim(
        host="some-other-host-that-does-not-exist", expires_at=now_ms() - 1000
    )
    assert classify(claim) == ClaimState.STALE


def test_classify_live_ttl_unexpired_with_live_pid():
    """Unexpired TTL + live pid -> LIVE (the normal in-window case)."""
    claim = _live_claim(expires_at=now_ms() + 60_000)  # live pid via _live_claim
    assert classify(claim) == ClaimState.LIVE


def test_classify_suspect_ttl_unexpired_dead_pid():
    """SUSPECT arm (x-ba4b): unexpired TTL + dead pid -> SUSPECT, not LIVE.

    The respawned-worker case: the supervisor pid died but the session lives on
    inside its TTL window. Before x-ba4b this returned LIVE; now the distinct
    SUSPECT state lets init/dispatch refuse-and-skip (never steal) while the TTL
    still protects the slot. Only TTL expiry (-> STALE) frees it."""
    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    claim = _live_claim(pid=dead_pid, expires_at=now_ms() + 60_000)
    assert classify(claim) == ClaimState.SUSPECT


def test_classify_pid_unavailable_ttl_is_suspect_until_expiry():
    claim = _live_claim(
        pid=None,
        pid_unavailable=True,
        expires_at=now_ms() + 60_000,
    )
    assert is_live(claim) is False
    assert classify(claim) == ClaimState.SUSPECT
    claim = claim.model_copy(update={"expires_at": now_ms() - 1})
    assert classify(claim) == ClaimState.STALE


def test_classify_suspect_ttl_unexpired_remote_host():
    """Unexpired TTL on another host -> SUSPECT: same-host pid arm can't prove
    liveness, but the TTL still protects the slot (not stealable, not stale)."""
    claim = _live_claim(
        host="some-other-host-that-does-not-exist", expires_at=now_ms() + 60_000
    )
    assert classify(claim) == ClaimState.SUSPECT


# ---------------------------------------------------------------------------
# now_ms is monotonic enough
# ---------------------------------------------------------------------------


def test_now_ms_increases_over_time():
    a = now_ms()
    time.sleep(0.01)
    b = now_ms()
    assert b >= a


# ---------------------------------------------------------------------------
# machine identity: a hostname flip must not stale a live claim
# ---------------------------------------------------------------------------


def _requires_stable_machine_id():
    """Skip where the OS exposes no stable machine id.

    machine_id() is "" there, claims omit the field, and every reader takes the
    hostname fallback - so the flip these tests simulate cannot be expressed.
    """
    hostid.machine_id.cache_clear()
    if not hostid.machine_id():
        pytest.skip("no stable OS machine id on this platform")


def test_is_live_survives_hostname_flip(monkeypatch):
    """gethostname() is derived from DHCP/DNS on macOS with HostName
    unset, so it flips on network join, VPN, and sleep/wake. A live holder must
    NOT read as cross-host when the name moves - the host check short-circuits
    is_live before the pid check, and the resulting STALE is stealable."""
    _requires_stable_machine_id()
    claim = _live_claim(machine_id=hostid.machine_id())
    monkeypatch.setattr(hostid, "hostname", lambda: "BB16s-MBP")
    hostid.machine_id.cache_clear()
    assert is_live(claim) is True


def test_classify_live_ttl_expired_after_hostname_flip(monkeypatch):
    """The observed failure: a TTL claim whose clock lapsed while its holder
    kept working. The hybrid-liveness rescue (pid is the live original holder,
    prover-proven at write time) was short-circuited by the host mismatch, so
    the claim read STALE and any re-dispatch could steal the node out from
    under the live session."""
    _requires_stable_machine_id()
    claim = _live_claim(
        machine_id=hostid.machine_id(), expires_at=now_ms() - 1000,
        pid_provenance="session-prover",
    )
    monkeypatch.setattr(hostid, "hostname", lambda: "BB16s-MBP")
    hostid.machine_id.cache_clear()
    assert classify(claim) == ClaimState.LIVE


def test_classify_stale_remote_machine_id_still_opaque():
    """Cross-machine opacity is preserved: a claim carrying ANOTHER machine's id
    is not ours, so it stays recoverable. Widening this would be a different bug
    (a remote claim wedged live forever)."""
    claim = _live_claim(
        machine_id="00000000-0000-0000-0000-000000000000", expires_at=now_ms() - 1000
    )
    assert classify(claim) == ClaimState.STALE


def test_machine_id_wins_over_a_matching_host():
    """machine_id is authoritative when present. A claim from another machine
    that happens to share this one's hostname must NOT read as ours; OR-ing the
    two candidates instead of dispatching on presence would let it through."""
    _requires_stable_machine_id()
    claim = _live_claim(host=hostid.hostname(), machine_id="not-this-machine")
    assert is_live(claim) is False


def test_pre_change_claim_falls_back_to_the_host_compare():
    """A claim written before machine_id existed carries no such field. It must
    classify exactly as it did before - no worse - via the hostname compare."""
    claim = _live_claim(host=hostid.hostname(), machine_id=None)
    assert is_live(claim) is True
    assert is_live(_live_claim(host="some-other-host", machine_id=None)) is False


def test_new_claims_stay_readable_by_a_pre_change_reader():
    """Rolling-upgrade guard. A pre-change reader compares `host` against its own
    gethostname() and knows nothing of machine_id, so writing the machine id INTO
    host would make it call a live claim stale - which is stealable, i.e. this
    very bug from the other side. host must stay the hostname."""
    from fno.claims.core import _make_claim

    claim = _make_claim("node:x", "h", None, None, None, os.getpid(), None)
    assert claim.host == socket.gethostname(), "a pre-change reader still matches host"
    assert claim.machine_id == hostid.machine_id()


def test_machine_id_is_never_a_hostname_substitute(monkeypatch):
    """A PATH-stripped hook cannot run /usr/sbin/ioreg. The old fallback wrote a
    HOSTNAME into machine_id, which readers trust as authoritative, so a reader
    that could compute the real id would disagree and stale a live claim. Absent
    is the honest answer; readers then use the hostname compare."""
    monkeypatch.setattr(hostid, "_macos_platform_uuid", lambda: "")
    monkeypatch.setattr(hostid, "_linux_machine_id", lambda: "")
    hostid.machine_id.cache_clear()
    assert hostid.machine_id() == ""
    assert hostid.machine_id() != hostid.hostname()
    hostid.machine_id.cache_clear()


def test_claim_omits_machine_id_when_none_is_available(monkeypatch):
    """The writer must omit the field rather than record a stand-in."""
    from fno.claims.core import _make_claim

    monkeypatch.setattr(hostid, "_macos_platform_uuid", lambda: "")
    monkeypatch.setattr(hostid, "_linux_machine_id", lambda: "")
    hostid.machine_id.cache_clear()
    claim = _make_claim("node:x", "h", None, None, None, os.getpid(), None)
    assert claim.machine_id is None
    assert claim.host == socket.gethostname()
    hostid.machine_id.cache_clear()


def test_reader_that_cannot_read_its_own_id_does_not_steal(monkeypatch):
    """A reader whose ioreg/machine-id lookup fails cannot tell whose claim it
    is. Ambiguous liveness must degrade to skip, never to steal.

    The hostname is ALSO changed here, which is the whole point: on the roaming
    laptop this exists for, the name has usually moved too, so falling back to
    the hostname compare would fail and stale a live LOCAL holder.
    """
    claim = _live_claim(host="the-name-it-had-back-then", machine_id="some-machine-id")
    monkeypatch.setattr(hostid, "_macos_platform_uuid", lambda: "")
    monkeypatch.setattr(hostid, "_linux_machine_id", lambda: "")
    hostid.machine_id.cache_clear()
    assert hostid.machine_id() == ""
    assert is_live(claim) is True, "unreadable own id must not read as foreign"
    hostid.machine_id.cache_clear()


def test_unknown_own_id_still_stales_a_dead_pid(monkeypatch):
    """The unknown path withholds a STEAL, it does not fake liveness: the pid
    arm still decides, so a dead holder is stale exactly as before."""
    dead = 999_999
    while psutil.pid_exists(dead):
        dead += 1
    claim = _live_claim(host="whatever", machine_id="some-machine-id", pid=dead)
    monkeypatch.setattr(hostid, "_macos_platform_uuid", lambda: "")
    monkeypatch.setattr(hostid, "_linux_machine_id", lambda: "")
    hostid.machine_id.cache_clear()
    assert is_live(claim) is False
    hostid.machine_id.cache_clear()


def test_is_same_machine_arms():
    hostid.machine_id.cache_clear()
    # host arm (only when no machine id was recorded) - always available
    assert hostid.is_same_machine(hostid.hostname(), None) is True
    assert hostid.is_same_machine("some-other-host-that-does-not-exist", None) is False
    assert hostid.is_same_machine(None, None) is False
    assert hostid.is_same_machine("", "") is False


def test_is_same_machine_machine_arm():
    """The machine arm needs a real OS id to be expressible at all."""
    _requires_stable_machine_id()
    assert hostid.is_same_machine("irrelevant", hostid.machine_id()) is True
    assert hostid.is_same_machine(hostid.hostname(), "other-machine") is False
