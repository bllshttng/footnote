"""Unit tests for fno.claims.io: atomic-write + YAML round-trip + URL-encoding."""
from __future__ import annotations

import os
import socket
import threading
from pathlib import Path

import pytest

from fno.claims.io import (
    ClaimAlreadyHeld,
    ClaimGoneAway,
    atomic_create_exclusive,
    claims_dir,
    decode_key,
    encode_key,
    global_claims_root,
    read_claim_file,
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


def test_read_missing_file_raises_claim_gone_away(tmp_path):
    with pytest.raises(ClaimGoneAway):
        read_claim_file(tmp_path / "nope.lock")


# ---------------------------------------------------------------------------
# Schema version forward-compat
# ---------------------------------------------------------------------------


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


def test_claims_dir_defaults_to_the_repo_space_without_env(tmp_path, monkeypatch):
    """No root + no env => the repo's space (no claims dir inside a checkout)."""
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.chdir(tmp_path)
    from fno.paths import space_dir, space_slug

    # The slug is a cross-language wire format (fno-agents::paths swaps the
    # same separators); a golden value here catches a one-sided drift.
    assert space_slug(Path("/repos/web")) == "-repos-web"
    assert claims_dir() == space_dir(tmp_path) / "claims"


def test_global_claims_root_env_then_home(tmp_path, monkeypatch):
    """global_claims_root() prefers the env, else falls back to $HOME."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    assert global_claims_root() == tmp_path
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    assert global_claims_root() == tmp_path / "home"


# Identity-based root routing (node/dispatch/reconcile -> global, repo-local
# keys -> the default) is owned by the Rust leg now: its characterization
# golden lives in crates/fno-agents/tests/claims_root_parity.rs.


# --- The state-root denial (x-f22f) -----------------------------------------
#
# A permission denial and lock contention both leave the caller without a
# claim, and they have opposite remedies: contention clears on its own, a
# missing write grant never does. A worker told only "no claim" reads it as
# contention and waits for a holder that does not exist.


def test_denied_claim_write_is_not_reported_as_contention(tmp_path, monkeypatch):
    from fno.claims.io import ClaimStateRootDenied

    state_root = tmp_path / ".fno"
    claims = state_root / "claims"
    claims.mkdir(parents=True)
    repo = tmp_path / "repo"
    repo.mkdir()
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: repo)
    claims.chmod(0o500)  # readable, not writable: the sandbox's shape
    try:
        with pytest.raises(ClaimStateRootDenied) as caught:
            atomic_create_exclusive(claims / "node%3Aab-1.lock", "x: 1\n")
    finally:
        claims.chmod(0o700)

    message = str(caught.value)
    assert str(state_root) in message, message
    assert "NOT lock contention" in message, message


def test_denied_claim_write_leaves_a_breadcrumb_the_operator_can_read(tmp_path, monkeypatch):
    """The one channel a mute worker still has.

    A worker denied the state root has lost the claim store, the mail bus and
    the spawn mutex at once, so it cannot report its own condition through any
    of them. The repo stays writable, so the refusal is written there.
    """
    import json

    from fno.claims.io import ClaimStateRootDenied

    claims = tmp_path / ".fno" / "claims"
    claims.mkdir(parents=True)
    repo = tmp_path / "repo"
    repo.mkdir()
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: repo)
    # The canonical carrier, not a per-harness one: the resolver reads the
    # marker table in fno.harness_identity rather than a second hand-written
    # list, so this pins the contract instead of one harness's spelling.
    monkeypatch.setenv("FNO_HARNESS_SESSION_ID", "sess-probe-1")
    claims.chmod(0o500)
    try:
        with pytest.raises(ClaimStateRootDenied):
            atomic_create_exclusive(claims / "node%3Aab-2.lock", "x: 1\n")
    finally:
        claims.chmod(0o700)

    crumb = repo / ".fno" / "state-root-denied.json"
    assert crumb.exists(), "the denial must be readable from outside the sandbox"
    payload = json.loads(crumb.read_text())
    assert payload["denied_root"] == str(tmp_path / ".fno")
    assert payload["session_id"] == "sess-probe-1"
    assert payload["denied_at"].endswith("+00:00")


def test_a_later_successful_claim_clears_the_breadcrumb(tmp_path, monkeypatch):
    claims = tmp_path / ".fno" / "claims"
    claims.mkdir(parents=True)
    repo = tmp_path / "repo"
    repo.mkdir()
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: repo)
    crumb = repo / ".fno" / "state-root-denied.json"
    crumb.parent.mkdir(parents=True)
    crumb.write_text("{}")

    atomic_create_exclusive(claims / "node%3Aab-3.lock", "x: 1\n")

    assert not crumb.exists(), "a successful claim proves the grant is back"


def test_contention_is_still_contention(tmp_path):
    """The denial branch must not swallow the AlreadyHeld signal."""
    claims = tmp_path / ".fno" / "claims"
    claims.mkdir(parents=True)
    target = claims / "node%3Aab-4.lock"
    atomic_create_exclusive(target, "x: 1\n")
    with pytest.raises(ClaimAlreadyHeld):
        atomic_create_exclusive(target, "x: 2\n")


def test_denial_on_a_not_yet_created_claims_dir_is_still_named(tmp_path, monkeypatch):
    """The case this feature exists for, and the one the first version missed.

    A sandboxed worker usually meets a state root whose ``claims`` child does
    not exist yet. The create then fails ``FileNotFoundError``, the retry branch
    calls ``mkdir``, and THAT is the call the sandbox denies. An earlier version
    mapped denials only on the first attempt, so this escaped as a raw
    ``PermissionError``: no named refusal, no breadcrumb, and a worker left to
    read it as lock contention.
    """
    import json

    from fno.claims.io import ClaimStateRootDenied

    state_root = tmp_path / ".fno"
    state_root.mkdir()  # exists, but has NO claims child
    repo = tmp_path / "repo"
    repo.mkdir()
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: repo)
    state_root.chmod(0o500)
    try:
        with pytest.raises(ClaimStateRootDenied) as caught:
            atomic_create_exclusive(state_root / "claims" / "node%3Aab-9.lock", "x: 1\n")
    finally:
        state_root.chmod(0o700)

    assert str(state_root) in str(caught.value)
    crumb = repo / ".fno" / "state-root-denied.json"
    assert crumb.exists(), "the denial must leave a breadcrumb here too"
    assert json.loads(crumb.read_text())["denied_root"] == str(state_root)


def test_a_claim_under_a_different_root_leaves_a_live_denial_alone(tmp_path, monkeypatch):
    """An unconditional clear erased a denial the worker still had.

    A worker holds claims under more than one root. A success under the
    repo-local store must not delete the report that the GLOBAL store is
    denied: the problem stands and the only record of it disappears.
    """
    import json

    other_root = tmp_path / "other" / ".fno"
    claims = other_root / "claims"
    claims.mkdir(parents=True)
    repo = tmp_path / "repo"
    repo.mkdir()
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: repo)
    crumb = repo / ".fno" / "state-root-denied.json"
    crumb.parent.mkdir(parents=True)
    crumb.write_text(json.dumps({"denied_root": "/some/other/.fno"}))

    atomic_create_exclusive(claims / "node%3Aab-10.lock", "x: 1\n")

    assert crumb.exists(), "a success elsewhere must not erase a live denial"
    assert json.loads(crumb.read_text())["denied_root"] == "/some/other/.fno"


# ---------------------------------------------------------------------------
# The read and archive locators over the claims table
# ---------------------------------------------------------------------------


def test_read_claim_file_round_trips_ttl_and_session_id(tmp_path):
    from fno.claims import acquire_claim
    from fno.claims.io import claim_path

    acquire_claim(
        "node:ab-1", "h", ttl_ms=60_000, pid=os.getpid(),
        harness_session_id="abc123", root=tmp_path,
    )
    parsed = read_claim_file(claim_path("node:ab-1", root=tmp_path))
    assert parsed.holder == "h"
    assert parsed.expires_at is not None
    assert parsed.session_id == "abc123"


def test_pid_liveness_row_reads_expires_at_none(tmp_path):
    from fno.claims import acquire_claim
    from fno.claims.io import claim_path

    acquire_claim("node:ab-2", "h", root=tmp_path)
    assert read_claim_file(claim_path("node:ab-2", root=tmp_path)).expires_at is None


@pytest.mark.parametrize("column,value", [("schema_version", 999), ("holder", "")])
def test_an_invalid_row_raises_claim_corrupted(tmp_path, column, value):
    from fno.claims import acquire_claim, claim_status
    from fno.claims.io import ClaimCorrupted, claim_path
    from tests._table_seed import update_claim

    acquire_claim("node:ab-3", "h", root=tmp_path)
    update_claim("node:ab-3", root=tmp_path, **{column: value})
    with pytest.raises(ClaimCorrupted):
        read_claim_file(claim_path("node:ab-3", root=tmp_path))
    assert claim_status("node:ab-3", root=tmp_path)["state"] == "corrupted"


def test_archive_claim_retires_the_row(tmp_path):
    from fno.claims import acquire_claim, claim_status
    from fno.claims.io import archive_claim, claim_path

    acquire_claim("node:ab-4", "h", root=tmp_path)
    archive_claim(claim_path("node:ab-4", root=tmp_path), ts_ms=1234567890)
    assert claim_status("node:ab-4", root=tmp_path)["state"] == "free"


def test_archive_missing_claim_is_noop(tmp_path):
    from fno.claims.io import archive_claim, claim_path

    path = claim_path("node:ab-5", root=tmp_path)
    assert archive_claim(path, ts_ms=1) == path
