"""Cross-implementation compatibility matrix for the claims table.

The protocol has two implementations: the Python reference
(``fno.claims``, the only operator CLI) and the native Rust module
(``crates/fno-agents/src/claims.rs``, used by the daemon/adopt/drive hot
paths). Both operate on the same claims table in ``graph.db``, so any divergence is
split-brain: one side reclaims what the other considers held. This module is
the merge gate proving they agree, in both directions.

The Rust side is driven through the hidden ``fno-agents claim`` debug verb.
The Python side uses the library directly (the installed ``fno`` binary may be
stale relative to this checkout; ``cli/src`` is authoritative).

Binary resolution: this checkout's build, through
``fno.rust_binary.find_dev_binary``. No ``$FNO_AGENTS_BIN`` or installed copy
answers for it. Without a build the module SKIPS. The merge gate is
``tests/test-claims-compat-matrix.sh``: smoke runs it after its build step,
and it fails when there is no build, so the gate cannot soften into a skip.
"""
from __future__ import annotations

import json
import os
import socket
import subprocess
import threading
import time
from pathlib import Path

import pytest

from fno.claims.core import ClaimHeldByOther, acquire_claim, claim_status, release_claim
from fno.claims.hostid import machine_id as py_machine_id
from fno.claims.io import claim_path, read_claim_file, serialize_claim
from fno.claims.types import Claim
from fno.rust_binary import find_dev_binary
from tests._table_seed import claim_history_rows, read_claim_row

RUST_BIN = find_dev_binary()

if RUST_BIN is None:
    pytestmark = pytest.mark.skip(
        reason="fno-agents binary not built (cargo build -p fno-agents)"
    )


# --------------------------------------------------------------------------
# Harness
# --------------------------------------------------------------------------


def rust(op: str, key: str, root: Path, cwd: Path, *extra: str) -> subprocess.CompletedProcess:
    """Run the Rust side of the protocol via the hidden debug verb."""
    assert RUST_BIN is not None
    # --json: since the wave-1 leaf port the acquire verb prints the human
    # line by default; rust_json below parses the record payload.
    return subprocess.run(
        [str(RUST_BIN), "claim", op, key, "--root", str(root), "--json", *extra],
        capture_output=True,
        text=True,
        cwd=cwd,  # no env=, so the FNO_EVENTS_PATH pin below reaches this child too
        timeout=60,
    )


def rust_json(proc: subprocess.CompletedProcess) -> dict:
    assert proc.stdout.strip(), f"expected JSON on stdout, stderr: {proc.stderr}"
    return json.loads(proc.stdout)


STATUS_PARITY_FIELDS = (
    "state", "holder", "pid", "pid_unavailable", "schema_version", "host",
    "machine_id", "acquired_at", "expires_at", "metadata",
)


def assert_status_parity(direction: str, py: dict, rs: dict) -> None:
    """Field-by-field diff so a failure names the direction and the field (AC3-UI)."""
    for field in STATUS_PARITY_FIELDS:
        assert py.get(field) == rs.get(field), (
            f"{direction}: field {field!r} diverged: python={py.get(field)!r} "
            f"rust={rs.get(field)!r}"
        )


def write_raw_claim(root: Path, claim: Claim) -> Path:
    """Plant an on-disk claim directly (for expired-TTL / dead-pid states the
    public acquire APIs deliberately cannot produce)."""
    path = claim_path(claim.key, root=root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(claim), encoding="utf-8")
    return path


def dead_pid() -> int:
    """A pid that existed and is now gone (its create time can never validate)."""
    proc = subprocess.Popen(["true"])
    proc.wait()
    return proc.pid


def now_ms() -> int:
    return int(time.time() * 1000)


@pytest.fixture(autouse=True)
def _one_journal_for_both_sides(tmp_path: Path, monkeypatch) -> None:
    """Name the journal both implementations must write to.

    Python (`fno.paths.project_events_json`) and Rust (`claim_events_path`) both
    read `FNO_EVENTS_PATH` ahead of the repo root they resolve. The two share
    this journal AND its `.lock.d` mutex as a wire contract, so a pin only one
    side honored would put the writers on different files and stop the mutex
    serialising them against each other.

    Deliberately NOT `<cwd>/.fno/events.jsonl`. Every test here runs with cwd at
    `tmp_path`, so a pin there is also what cwd-derived resolution returns, and
    the assertions pass whether or not either side reads the var. Pointing it at
    a sibling directory makes the pinned path and the cwd-derived path differ, so
    a side that ignores the pin writes somewhere this file does not read, and the
    gate goes red. Verified: reverting the Rust read fails these tests.
    """
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / "pinned" / "events.jsonl"))


def events(_cwd: Path | None = None) -> list[dict]:
    """Every claim audit row, read from the journal the pin names.

    Takes the old cwd argument and ignores it: which file the writers use is the
    property under test, so reading a hand-built `<cwd>/.fno/events.jsonl` would
    assert the answer instead of observing it. The claim lifecycle kinds are
    ephemeral-class, so both implementations write them to the journal's
    ``.ephemeral`` sibling (retention routing); the reader takes both files and
    the wire contract under test is unchanged - both sides still share one
    store and its mutex.
    """
    from fno.paths import journal_and_ephemeral_sibling

    from tests._event_rows import event_rows

    rows: list[dict] = []
    for p in journal_and_ephemeral_sibling(Path(os.environ["FNO_EVENTS_PATH"])):
        rows.extend(event_rows(p))
    return rows


# --------------------------------------------------------------------------
# 1 + 2: each side reads the other's records identically
# --------------------------------------------------------------------------


def test_python_writes_rust_reads_pid_and_ttl(tmp_path: Path) -> None:
    meta = {"nested": {"a": [1, 2], "b": "text"}, "flag": True}
    acquire_claim(
        "session:py-pid", "pty:py", pid=os.getpid(), reason="why", metadata=meta, root=tmp_path
    )
    acquire_claim("session:py-ttl", "pty:py", pid=os.getpid(), ttl_ms=60_000, root=tmp_path)

    for key in ("session:py-pid", "session:py-ttl"):
        py = claim_status(key, root=tmp_path)
        rs = rust_json(rust("status", key, tmp_path, tmp_path))
        assert_status_parity(f"python-writes-rust-reads ({key})", py, rs)
    rs = rust_json(rust("status", "session:py-pid", tmp_path, tmp_path))
    assert rs["state"] == "live"
    assert rs["metadata"] == meta
    assert rs["expires_at"] is None
    assert rust_json(rust("status", "session:py-ttl", tmp_path, tmp_path))["expires_at"] is not None


def test_rust_writes_python_reads_pid_and_ttl(tmp_path: Path) -> None:
    meta = json.dumps({"nested": {"a": [1, 2]}, "s": "héllo"})
    r = rust(
        "acquire", "session:rs-pid", tmp_path, tmp_path,
        "--holder", "pty:rs", "--pid", str(os.getpid()), "--reason", "why", "--metadata", meta,
    )
    assert r.returncode == 0, r.stderr
    r = rust(
        "acquire", "session:rs-ttl", tmp_path, tmp_path,
        "--holder", "pty:rs", "--pid", str(os.getpid()), "--ttl-ms", "60000",
    )
    assert r.returncode == 0, r.stderr

    for key in ("session:rs-pid", "session:rs-ttl"):
        py = claim_status(key, root=tmp_path)
        rs = rust_json(rust("status", key, tmp_path, tmp_path))
        assert_status_parity(f"rust-writes-python-reads ({key})", py, rs)
    py = claim_status("session:rs-pid", root=tmp_path)
    assert py["state"] == "live"
    assert py["holder"] == "pty:rs"
    assert py["metadata"] == json.loads(meta)


def test_pid_unavailable_ttl_round_trips_between_python_and_rust(tmp_path: Path) -> None:
    acquire_claim(
        "session:py-unavailable",
        "pty:py",
        ttl_ms=60_000,
        pid_unavailable=True,
        root=tmp_path,
    )
    py = claim_status("session:py-unavailable", root=tmp_path)
    rs = rust_json(rust("status", "session:py-unavailable", tmp_path, tmp_path))
    assert_status_parity("python-writes-rust-reads (pid unavailable)", py, rs)
    assert py["pid"] is None and py["pid_unavailable"] is True
    assert py["schema_version"] == 2

    r = rust(
        "acquire",
        "session:rs-unavailable",
        tmp_path,
        tmp_path,
        "--holder",
        "pty:rs",
        "--ttl-ms",
        "60000",
        "--pid-unavailable",
    )
    assert r.returncode == 0, r.stderr
    py = claim_status("session:rs-unavailable", root=tmp_path)
    rs = rust_json(rust("status", "session:rs-unavailable", tmp_path, tmp_path))
    assert_status_parity("rust-writes-python-reads (pid unavailable)", py, rs)
    assert rs["pid"] is None and rs["pid_unavailable"] is True


# --------------------------------------------------------------------------
# 3: release across implementations
# --------------------------------------------------------------------------


def test_cross_impl_release_same_holder(tmp_path: Path) -> None:
    # Rust releases a Python-written claim...
    acquire_claim("session:x-rel", "pty:owner", pid=os.getpid(), root=tmp_path)
    r = rust("release", "session:x-rel", tmp_path, tmp_path, "--holder", "pty:owner")
    assert r.returncode == 0, r.stderr
    assert claim_status("session:x-rel", root=tmp_path)["state"] == "free"

    # ...and Python releases a Rust-written claim.
    rust("acquire", "session:x-rel2", tmp_path, tmp_path, "--holder", "pty:owner",
         "--pid", str(os.getpid()))
    release_claim("session:x-rel2", "pty:owner", root=tmp_path)
    assert rust_json(rust("status", "session:x-rel2", tmp_path, tmp_path))["state"] == "free"


def test_cross_impl_release_different_holder_is_silent_noop(tmp_path: Path) -> None:
    acquire_claim("session:keep", "pty:owner", pid=os.getpid(), root=tmp_path)
    r = rust("release", "session:keep", tmp_path, tmp_path, "--holder", "pty:other")
    assert r.returncode == 0, r.stderr
    assert claim_status("session:keep", root=tmp_path)["state"] == "live"

    rust("acquire", "session:keep2", tmp_path, tmp_path, "--holder", "pty:owner",
         "--pid", str(os.getpid()))
    release_claim("session:keep2", "pty:other", root=tmp_path)
    assert rust_json(rust("status", "session:keep2", tmp_path, tmp_path))["state"] == "live"


# --------------------------------------------------------------------------
# 4: stale reclaim across implementations (+ archive + audit event)
# --------------------------------------------------------------------------


def _stale_claim(key: str, holder: str = "pty:dead") -> Claim:
    return Claim(
        key=key, holder=holder, acquired_at=now_ms(), pid=dead_pid(),
        host=__import__("socket").gethostname(),
    )


def test_python_stale_rust_reclaims_archives_and_audits(tmp_path: Path) -> None:
    write_raw_claim(tmp_path, _stale_claim("session:stale-a"))
    r = rust("acquire", "session:stale-a", tmp_path, tmp_path,
             "--holder", "pty:new", "--pid", str(os.getpid()))
    assert r.returncode == 0, f"stale claim not reclaimed: {r.stderr}"
    assert rust_json(r)["holder"] == "pty:new"

    archived = claim_history_rows("session:stale-a", tmp_path)
    assert [r["holder"] for r in archived] == ["pty:dead"], "stale claim must be archived"
    kinds = [e["type"] for e in events(tmp_path)]
    assert "claim_stale_reclaimed" in kinds
    reclaimed = [e for e in events(tmp_path) if e["type"] == "claim_stale_reclaimed"][0]
    assert reclaimed["source"] == "fno-loop"
    assert reclaimed["data"]["previous_holder"] == "pty:dead"


def test_rust_stale_python_reclaims_and_archives(tmp_path: Path, monkeypatch) -> None:
    # Rust writes a claim anchored to a now-dead pid...
    r = rust("acquire", "session:stale-b", tmp_path, tmp_path,
             "--holder", "pty:dead", "--pid", str(dead_pid()))
    assert r.returncode == 0, r.stderr
    # ...Python observes it stale and reclaims it.
    monkeypatch.chdir(tmp_path)  # Python audit events land in <cwd>/.fno/events.jsonl
    claim = acquire_claim("session:stale-b", "pty:new", pid=os.getpid(), root=tmp_path)
    assert claim.holder == "pty:new"
    assert [r["holder"] for r in claim_history_rows("session:stale-b", tmp_path)] == ["pty:dead"]
    assert "claim_stale_reclaimed" in [e["type"] for e in events(tmp_path)]


# --------------------------------------------------------------------------
# 5: hybrid-arm liveness parity
# --------------------------------------------------------------------------


def test_hybrid_arm_parity_expired_ttl(tmp_path: Path) -> None:
    host = __import__("socket").gethostname()
    # acquired_at must NOT predate this process's create time (that would trip
    # PID-reuse detection, correctly, in both impls); an expired expires_at
    # alongside a current acquired_at isolates the hybrid arm.
    # Expired TTL + LIVE recorded pid -> both classify LIVE.
    write_raw_claim(tmp_path, Claim(
        key="session:hyb-live", holder="h", acquired_at=now_ms(),
        expires_at=now_ms() - 1_000, pid=os.getpid(), host=host,
        pid_provenance="session-prover",
    ))
    # Expired TTL + DEAD pid -> both classify STALE.
    write_raw_claim(tmp_path, Claim(
        key="session:hyb-dead", holder="h", acquired_at=now_ms(),
        expires_at=now_ms() - 1_000, pid=dead_pid(), host=host,
    ))
    for key, want in (("session:hyb-live", "live"), ("session:hyb-dead", "stale")):
        py = claim_status(key, root=tmp_path)["state"]
        rs = rust_json(rust("status", key, tmp_path, tmp_path))["state"]
        assert py == want, f"python classified {key} as {py}, want {want}"
        assert rs == want, f"rust classified {key} as {rs}, want {want}"


# --------------------------------------------------------------------------
# 7: simultaneous acquire race - exactly one winner per round
# --------------------------------------------------------------------------


def test_race_python_vs_rust_single_winner(tmp_path: Path) -> None:
    rounds = 8
    for i in range(rounds):
        key = "session:race"
        barrier = threading.Barrier(2)
        results: dict[str, object] = {}

        def py_side() -> None:
            barrier.wait()
            try:
                acquire_claim(key, "pty:python", pid=os.getpid(), root=tmp_path)
                results["python"] = "acquired"
            except ClaimHeldByOther as exc:
                results["python"] = f"held:{exc.holder}"

        def rs_side() -> None:
            barrier.wait()
            r = rust("acquire", key, tmp_path, tmp_path,
                     "--holder", "pty:rust", "--pid", str(os.getpid()))
            results["rust"] = "acquired" if r.returncode == 0 else f"held(rc={r.returncode})"

        threads = [threading.Thread(target=py_side), threading.Thread(target=rs_side)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=60)

        winners = [side for side, out in results.items() if out == "acquired"]
        assert len(winners) == 1, f"round {i}: want exactly one winner, got {results}"
        # No corrupted file: the surviving lock parses and names the winner.
        rec = read_claim_file(claim_path(key, root=tmp_path))
        assert rec.holder == f"pty:{winners[0]}", f"round {i}: {results}"
        release_claim(key, rec.holder, root=tmp_path)


# --------------------------------------------------------------------------
# 9: a pid claim has no expiry
# --------------------------------------------------------------------------


def test_rust_pid_claim_has_no_expiry(tmp_path: Path) -> None:
    r = rust("acquire", "session:no-ttl", tmp_path, tmp_path,
             "--holder", "pty:x", "--pid", str(os.getpid()))
    assert r.returncode == 0, r.stderr
    row = read_claim_row("session:no-ttl", tmp_path)
    assert row["expires_at"] is None, f"PID-liveness claims carry no expiry: {row}"
    assert claim_status("session:no-ttl", root=tmp_path)["expires_at"] is None
    # liveness compares machine_id, so a writer that dropped it would send
    # every reader down the hostname fallback.
    assert row["machine_id"] == (py_machine_id() or None)
    assert row["pid_provenance"] == "ambient"
    assert row["host"] == socket.gethostname()
