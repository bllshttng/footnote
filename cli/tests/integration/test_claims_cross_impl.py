"""Cross-implementation compatibility matrix for the claims lockfile protocol.

The protocol has two implementations: the Python reference
(``fno.claims``, the only operator CLI) and the native Rust module
(``crates/fno-agents/src/claims.rs``, used by the daemon/adopt/drive hot
paths). Both operate on the same ``.fno/claims/`` files, so any divergence is
split-brain: one side reclaims what the other considers held. This module is
the merge gate proving they agree, in both directions.

The Rust side is driven through the hidden ``fno-agents claim`` debug verb.
The Python side uses the library directly (the installed ``fno`` binary may be
stale relative to this checkout; ``cli/src`` is authoritative).

Binary resolution: ``$FNO_AGENTS_BIN``, else the repo debug build. Without a
binary the module SKIPS - except when ``FNO_CLAIMS_COMPAT_REQUIRED=1`` (set by
the CI job that builds both toolchains), where a missing binary FAILS loudly
so the gate cannot silently soften into a skip.
"""
from __future__ import annotations

import json
import os
import subprocess
import threading
import time
from pathlib import Path

import pytest
import yaml

from fno.claims.core import ClaimHeldByOther, acquire_claim, claim_status, release_claim
from fno.claims.io import claim_path, encode_key, read_claim_file, serialize_claim
from fno.claims.types import Claim


def _find_repo_root(start: Path) -> Path | None:
    for parent in [start, *start.parents]:
        if (parent / "crates" / "fno-agents").is_dir():
            return parent
    return None


def _rust_bin() -> Path | None:
    env = os.environ.get("FNO_AGENTS_BIN", "")
    if env:
        p = Path(env)
        return p if p.exists() else None
    root = _find_repo_root(Path(__file__).resolve().parent)
    if root is None:
        return None
    for profile in ("debug", "release"):
        p = root / "crates" / "fno-agents" / "target" / profile / "fno-agents"
        if p.exists():
            return p
    return None


RUST_BIN = _rust_bin()
COMPAT_REQUIRED = os.environ.get("FNO_CLAIMS_COMPAT_REQUIRED") == "1"

if RUST_BIN is None and not COMPAT_REQUIRED:
    pytestmark = pytest.mark.skip(
        reason="fno-agents binary not built (cargo build -p fno-agents); "
        "set FNO_AGENTS_BIN or build the debug profile"
    )


def test_required_mode_has_a_binary() -> None:
    """On the designated CI job the matrix must RUN, never skip (AC3-FR)."""
    if COMPAT_REQUIRED:
        assert RUST_BIN is not None, (
            "FNO_CLAIMS_COMPAT_REQUIRED=1 but no fno-agents binary was found: "
            "the compat gate would silently skip. Build crates/fno-agents or "
            "set FNO_AGENTS_BIN."
        )


# --------------------------------------------------------------------------
# Harness
# --------------------------------------------------------------------------


def rust(op: str, key: str, root: Path, cwd: Path, *extra: str) -> subprocess.CompletedProcess:
    """Run the Rust side of the protocol via the hidden debug verb."""
    assert RUST_BIN is not None
    return subprocess.run(
        [str(RUST_BIN), "claim", op, key, "--root", str(root), *extra],
        capture_output=True,
        text=True,
        cwd=cwd,  # claim events land in <cwd>/.fno/events.jsonl on both sides
        timeout=60,
    )


def rust_json(proc: subprocess.CompletedProcess) -> dict:
    assert proc.stdout.strip(), f"expected JSON on stdout, stderr: {proc.stderr}"
    return json.loads(proc.stdout)


STATUS_PARITY_FIELDS = ("state", "holder", "pid", "host", "acquired_at", "expires_at", "metadata")


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


def events(cwd: Path) -> list[dict]:
    p = cwd / ".fno" / "events.jsonl"
    if not p.exists():
        return []
    return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]


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

    expired = list((tmp_path / ".fno" / "claims" / ".expired").iterdir())
    assert len(expired) == 1, "stale claim must be archived by rename, not unlinked"
    assert expired[0].name.startswith(encode_key("session:stale-a"))
    kinds = [e["type"] for e in events(tmp_path)]
    assert "claim_stale_reclaimed" in kinds
    reclaimed = [e for e in events(tmp_path) if e["type"] == "claim_stale_reclaimed"][0]
    assert reclaimed["source"] == "abi-loop"
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
    expired = list((tmp_path / ".fno" / "claims" / ".expired").iterdir())
    assert len(expired) == 1
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
# 6: filename-encoding parity
# --------------------------------------------------------------------------


def test_encoding_parity_produces_byte_identical_filenames(tmp_path: Path) -> None:
    # ':' '/' ' ' and non-ASCII must encode identically (uppercase hex) or the
    # two implementations would silently lock DIFFERENT files for one key.
    for key in ("session:a/b c", "session:naïve-café", "session:100%:done"):
        r = rust("acquire", key, tmp_path, tmp_path,
                 "--holder", "pty:x", "--pid", str(os.getpid()))
        assert r.returncode == 0, r.stderr
        expected = claim_path(key, root=tmp_path)
        assert expected.exists(), (
            f"encoding diverged for {key!r}: python expects {expected.name!r}, "
            f"rust wrote {[p.name for p in (tmp_path / '.fno' / 'claims').iterdir()]}"
        )
        # And Python can read it back through its own path derivation.
        assert read_claim_file(expected).key == key
        release_claim(key, "pty:x", root=tmp_path)


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
# 8: recovery-mutex interop - a fresh mutex is waited on; a corpse is stolen
#
# Both halves are wire protocol: the two implementations must agree on when a
# mutex is honestly held and when it is a corpse, or one side bricks a claim
# key the other could have recovered.
# --------------------------------------------------------------------------


def _hold_recovery_mutex(root: Path, key: str, seconds: float) -> tuple[Path, threading.Thread]:
    path = claim_path(key, root=root)
    mutex = path.with_name(path.name + ".recovery.d")
    mutex.mkdir(parents=True)

    def _release() -> None:
        time.sleep(seconds)
        mutex.rmdir()

    t = threading.Thread(target=_release)
    t.start()
    return mutex, t


def test_recovery_mutex_held_rust_waits_does_not_steal(tmp_path: Path) -> None:
    write_raw_claim(tmp_path, _stale_claim("session:rec-a"))
    mutex, releaser = _hold_recovery_mutex(tmp_path, "session:rec-a", seconds=1.0)
    t0 = time.monotonic()
    r = rust("acquire", "session:rec-a", tmp_path, tmp_path,
             "--holder", "pty:waiter", "--pid", str(os.getpid()))
    elapsed = time.monotonic() - t0
    releaser.join()
    assert r.returncode == 0, f"acquire after mutex release failed: {r.stderr}"
    assert elapsed >= 0.9, "rust must WAIT for the held recovery mutex, not steal it"
    assert not mutex.exists()


def test_recovery_mutex_held_python_waits_does_not_steal(tmp_path: Path, monkeypatch) -> None:
    write_raw_claim(tmp_path, _stale_claim("session:rec-b"))
    monkeypatch.chdir(tmp_path)
    mutex, releaser = _hold_recovery_mutex(tmp_path, "session:rec-b", seconds=1.0)
    t0 = time.monotonic()
    claim = acquire_claim("session:rec-b", "pty:waiter", pid=os.getpid(), root=tmp_path)
    elapsed = time.monotonic() - t0
    releaser.join()
    assert claim.holder == "pty:waiter"
    assert elapsed >= 0.9, "python must WAIT for the held recovery mutex, not steal it"


def _plant_recovery_corpse(root: Path, key: str) -> Path:
    """A recovery mutex left by a killed recoverer, backdated past the threshold."""
    from fno.mutex import STALE_MUTEX_STEAL_S

    mutex = claim_path(key, root=root).with_name(
        claim_path(key, root=root).name + ".recovery.d"
    )
    mutex.mkdir(parents=True)
    old = time.time() - (STALE_MUTEX_STEAL_S + 60)
    os.utime(mutex, (old, old))
    return mutex


def test_recovery_mutex_corpse_stolen_by_rust(tmp_path: Path) -> None:
    write_raw_claim(tmp_path, _stale_claim("session:rec-c"))
    mutex = _plant_recovery_corpse(tmp_path, "session:rec-c")

    r = rust("acquire", "session:rec-c", tmp_path, tmp_path,
             "--holder", "pty:heir", "--pid", str(os.getpid()))

    assert r.returncode == 0, f"rust never recovered past the corpse: {r.stderr}"
    assert not mutex.exists()


def test_recovery_mutex_corpse_stolen_by_python(tmp_path: Path, monkeypatch) -> None:
    write_raw_claim(tmp_path, _stale_claim("session:rec-d"))
    monkeypatch.chdir(tmp_path)
    mutex = _plant_recovery_corpse(tmp_path, "session:rec-d")

    claim = acquire_claim("session:rec-d", "pty:heir", pid=os.getpid(), root=tmp_path)

    assert claim.holder == "pty:heir"
    assert not mutex.exists()


# --------------------------------------------------------------------------
# 9: expires_at absence discipline
# --------------------------------------------------------------------------


def test_rust_pid_claim_omits_expires_at_line(tmp_path: Path) -> None:
    r = rust("acquire", "session:no-ttl", tmp_path, tmp_path,
             "--holder", "pty:x", "--pid", str(os.getpid()))
    assert r.returncode == 0, r.stderr
    path = claim_path("session:no-ttl", root=tmp_path)
    text = path.read_text(encoding="utf-8")
    assert "expires_at" not in text, (
        f"PID-liveness claims must OMIT expires_at entirely (never null): {text}"
    )
    # And Python parses it as PID-liveness.
    rec = read_claim_file(path)
    assert rec.expires_at is None
    # Semantic YAML parity: yaml.safe_load sees the exact base field set. The
    # additive `harness` tag (x-3e70) is present only when the acquiring process
    # carries a session marker (a codex/claude/gemini session), absent otherwise
    # (bare CI), so it is excluded from the exact-set check rather than asserted.
    data = yaml.safe_load(text)
    assert set(data) - {"harness"} == {
        "schema_version",
        "key",
        "holder",
        "acquired_at",
        "pid",
        "host",
    }


# --------------------------------------------------------------------------
# AC3-ERR: corrupted-file parity
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "content",
    [
        "{{{{not yaml",
        "- a\n- list\n",
        "schema_version: 2\nkey: k\nholder: h\nacquired_at: 5\npid: 1\nhost: x\n",
    ],
    ids=["invalid-yaml", "non-dict-root", "newer-schema"],
)
def test_corrupted_file_parity(tmp_path: Path, content: str) -> None:
    key = "session:corrupt"
    path = claim_path(key, root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")

    # Both classify Corrupted.
    py = claim_status(key, root=tmp_path)
    rs = rust_json(rust("status", key, tmp_path, tmp_path))
    assert py["state"] == "corrupted", f"python: {py}"
    assert rs["state"] == "corrupted", f"rust: {rs}"

    # Both refuse to reclaim via plain acquire.
    with pytest.raises(Exception):
        acquire_claim(key, "pty:x", pid=os.getpid(), root=tmp_path)
    r = rust("acquire", key, tmp_path, tmp_path, "--holder", "pty:x",
             "--pid", str(os.getpid()))
    assert r.returncode != 0, "rust acquire must refuse a corrupted claim"

    # Both leave the file in place for force-release.
    assert path.exists()
    release_claim(key, "pty:x", root=tmp_path)
    r = rust("release", key, tmp_path, tmp_path, "--holder", "pty:x")
    assert r.returncode == 0
    assert path.exists(), "release must never delete what it cannot verify it owns"
