"""Unit tests for fno.claims.io: atomic-write + YAML round-trip + URL-encoding."""
from __future__ import annotations

import os
import socket
import threading
from pathlib import Path

import pytest

from fno.claims.io import (
    ClaimAlreadyHeld,
    ClaimCorrupted,
    ClaimGoneAway,
    archive_claim,
    atomic_create_exclusive,
    claim_path,
    claims_dir,
    claims_root_for,
    decode_key,
    encode_key,
    global_claims_root,
    read_claim_file,
    serialize_claim,
)
from fno.claims.types import Claim


def _make_claim(**overrides) -> Claim:
    defaults: dict = {
        "key": "node:ab-1234",
        "holder": "target-session:s1",
        "acquired_at": 1747641600000,
        "expires_at": None,
        "pid": os.getpid(),
        "host": socket.gethostname(),
    }
    defaults.update(overrides)
    return Claim(**defaults)


# ---------------------------------------------------------------------------
# Key encoding
# ---------------------------------------------------------------------------


def test_encode_key_url_encodes_colon():
    assert encode_key("node:ab-1234") == "node%3Aab-1234"


def test_encode_decode_round_trip():
    for key in [
        "node:ab-1234",
        "fleet:ab-mission01",
        "project:ab-mission:proj1",
        "simple",
        "with spaces and / slashes",
    ]:
        encoded = encode_key(key)
        assert "/" not in encoded
        decoded = decode_key(encoded + ".lock")
        assert decoded == key


# ---------------------------------------------------------------------------
# YAML round-trip
# ---------------------------------------------------------------------------


def test_yaml_round_trip_pid_liveness_omits_expires_at(tmp_path):
    """PID-liveness claims (expires_at=None) must OMIT the key in YAML."""
    claim = _make_claim(expires_at=None)
    text = serialize_claim(claim)
    assert "expires_at" not in text, "PID-liveness claims must omit expires_at"

    path = tmp_path / "claims" / "test.lock"
    path.parent.mkdir(parents=True)
    path.write_text(text)
    parsed = read_claim_file(path)
    assert parsed.expires_at is None
    assert parsed.holder == claim.holder
    assert parsed.acquired_at == claim.acquired_at


def test_yaml_round_trip_ttl_serializes_expires_at(tmp_path):
    claim = _make_claim(expires_at=1747641660000)
    text = serialize_claim(claim)
    assert "expires_at" in text

    path = tmp_path / "test.lock"
    path.write_text(text)
    parsed = read_claim_file(path)
    assert parsed.expires_at == 1747641660000


def test_yaml_reading_null_expires_at_equals_absent(tmp_path):
    """A reader must treat ``expires_at: null`` the same as absent."""
    path = tmp_path / "null.lock"
    path.write_text(
        "schema_version: 1\n"
        "key: x\n"
        "holder: h\n"
        "acquired_at: 1\n"
        "expires_at: null\n"
        f"pid: {os.getpid()}\n"
        f"host: {socket.gethostname()}\n"
    )
    claim = read_claim_file(path)
    assert claim.expires_at is None


def test_yaml_reading_missing_expires_at_equals_null(tmp_path):
    path = tmp_path / "absent.lock"
    path.write_text(
        "schema_version: 1\n"
        "key: x\n"
        "holder: h\n"
        "acquired_at: 1\n"
        f"pid: {os.getpid()}\n"
        f"host: {socket.gethostname()}\n"
    )
    claim = read_claim_file(path)
    assert claim.expires_at is None


def test_yaml_corrupted_raises_claim_corrupted(tmp_path):
    path = tmp_path / "bad.lock"
    path.write_text("not: valid: yaml: at: all: ::::")
    with pytest.raises(ClaimCorrupted):
        read_claim_file(path)


def test_yaml_missing_required_field_raises_claim_corrupted(tmp_path):
    path = tmp_path / "incomplete.lock"
    path.write_text("schema_version: 1\nkey: x\n")
    with pytest.raises(ClaimCorrupted):
        read_claim_file(path)


def test_yaml_root_not_dict_raises_claim_corrupted(tmp_path):
    path = tmp_path / "list.lock"
    path.write_text("- a\n- b\n")
    with pytest.raises(ClaimCorrupted):
        read_claim_file(path)


def test_read_missing_file_raises_claim_gone_away(tmp_path):
    with pytest.raises(ClaimGoneAway):
        read_claim_file(tmp_path / "nope.lock")


# ---------------------------------------------------------------------------
# Schema version forward-compat
# ---------------------------------------------------------------------------


def test_future_schema_version_rejected(tmp_path):
    path = tmp_path / "future.lock"
    path.write_text(
        "schema_version: 999\n"
        "key: x\n"
        "holder: h\n"
        "acquired_at: 1\n"
        f"pid: {os.getpid()}\n"
        f"host: {socket.gethostname()}\n"
    )
    with pytest.raises(ClaimCorrupted):
        read_claim_file(path)


# ---------------------------------------------------------------------------
# Atomic create exclusive
# ---------------------------------------------------------------------------


def test_atomic_create_exclusive_writes_content(tmp_path):
    path = tmp_path / "claims" / "test.lock"
    atomic_create_exclusive(path, "hello")
    assert path.read_text() == "hello"


def test_atomic_create_exclusive_collision_raises(tmp_path):
    path = tmp_path / "x.lock"
    path.parent.mkdir(parents=True, exist_ok=True)
    atomic_create_exclusive(path, "first")
    with pytest.raises(ClaimAlreadyHeld):
        atomic_create_exclusive(path, "second")
    assert path.read_text() == "first", "loser must NOT overwrite"


def test_atomic_create_creates_parent_directory(tmp_path):
    path = tmp_path / "deep" / "nested" / "x.lock"
    atomic_create_exclusive(path, "ok")
    assert path.read_text() == "ok"


def test_two_threads_race_one_wins(tmp_path):
    """Two threads racing on the same path: exactly one ClaimAlreadyHeld."""
    path = tmp_path / "race.lock"
    path.parent.mkdir(parents=True, exist_ok=True)
    barrier = threading.Barrier(2)
    results: list[str | type] = []
    lock = threading.Lock()

    def worker(tag: str) -> None:
        barrier.wait()
        try:
            atomic_create_exclusive(path, tag)
            with lock:
                results.append("won:" + tag)
        except ClaimAlreadyHeld:
            with lock:
                results.append("lost:" + tag)

    t1 = threading.Thread(target=worker, args=("a",))
    t2 = threading.Thread(target=worker, args=("b",))
    t1.start(); t2.start(); t1.join(); t2.join()

    wins = [r for r in results if r.startswith("won")]
    losses = [r for r in results if r.startswith("lost")]
    assert len(wins) == 1, f"expected 1 winner, got {results}"
    assert len(losses) == 1, f"expected 1 loser, got {results}"


# ---------------------------------------------------------------------------
# Archive
# ---------------------------------------------------------------------------


def test_archive_claim_moves_to_expired_dir(tmp_path):
    cdir = tmp_path / ".fno" / "claims"
    cdir.mkdir(parents=True)
    path = cdir / "node%3Aab-1.lock"
    path.write_text("dummy")

    archived = archive_claim(path, ts_ms=1234567890)
    assert not path.exists()
    assert archived.exists()
    assert archived.parent.name == ".expired"
    assert "1234567890" in archived.name


def test_archive_missing_file_is_noop(tmp_path):
    path = tmp_path / "nope.lock"
    result = archive_claim(path, ts_ms=1)
    assert result == path


# ---------------------------------------------------------------------------
# Global node-claims root resolution (ab-fcf9cec5)
# ---------------------------------------------------------------------------

def test_claims_dir_explicit_root_wins(tmp_path, monkeypatch):
    """An explicit root arg ignores the env override (per-root claims)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "global"))
    assert claims_dir(tmp_path) == tmp_path / ".fno/claims"


def test_claims_dir_honors_env_when_root_none(tmp_path, monkeypatch):
    """With no root arg, $FNO_CLAIMS_ROOT selects the base dir."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    assert claims_dir() == tmp_path / ".fno/claims"


def test_claims_dir_defaults_to_cwd_without_env(tmp_path, monkeypatch):
    """No root + no env => cwd-local (unchanged legacy behavior)."""
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.chdir(tmp_path)
    assert claims_dir() == tmp_path / ".fno/claims"


def test_global_claims_root_env_then_home(tmp_path, monkeypatch):
    """global_claims_root() prefers the env, else falls back to $HOME."""
    from fno.claims.io import global_claims_root
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    assert global_claims_root() == tmp_path
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    assert global_claims_root() == tmp_path / "home"


# ---------------------------------------------------------------------------
# claims_root_for: identity-based routing (node/dispatch/reconcile -> global)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("prefix", ["node", "dispatch", "reconcile", "session"])
def test_claims_root_for_global_id_kinds_route_global(prefix, monkeypatch):
    """node:/dispatch:/reconcile:/session: all root at the global root regardless of env.

    AC1-HP: a global-id key returns global_claims_root() with no
    FNO_CLAIMS_ROOT set, and the same root that node: resolves to.
    """
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    assert claims_root_for(f"{prefix}:x-abcd") == global_claims_root()
    # All three global-id kinds for the same id land in the SAME directory.
    assert claims_root_for(f"{prefix}:x-abcd") == claims_root_for("node:x-abcd")


def test_claims_root_for_honors_env_override(tmp_path, monkeypatch):
    """global-id kinds follow $FNO_CLAIMS_ROOT (via global_claims_root)."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    assert claims_root_for("dispatch:x-abcd") == tmp_path


@pytest.mark.parametrize(
    "key",
    [
        "walker:/some/repo",
        "fleet:m-123",
        "colonless",
        "",
        "unknown:x-abcd",
        # A bare prefix with no colon is NOT a global-id key (needs "<prefix>:<id>").
        "node",
        "dispatch",
        "reconcile",
        "session",
    ],
)
def test_claims_root_for_repo_local_and_unknown_keys_return_none(key):
    """AC1-ERR: repo-local / unrecognized / colon-less keys keep the default (None)."""
    assert claims_root_for(key) is None
