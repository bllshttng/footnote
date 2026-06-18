"""Unit tests for the `fno agents spawn-guard` verb (x-73cc).

The verb is the single source of truth for the bg-dispatch mutex shared by
`/target bg` (dispatch-node.sh) and `/agent spawn` (spawn.sh): Guard 1 (the
node:<id> claim probe, fail-closed) + Guard 2 (the create-only dispatch:<id>
reservation). These tests root all claims under a tmp dir so they never touch
real ~/.fno state.

Acceptance criteria mapped:
  AC1-HP   dispatchable verdict acquires the reservation
  AC1-ERR  a crashing probe fails closed (verdict=error, non-zero exit)
  AC1-UI   the verdict is exactly one parseable line / JSON object
  AC1-EDGE live and corrupted claims -> already-running / corrupted, no reservation
  AC1-FR   on dispatchable the reservation is held by --holder (caller releases)
  AC2-EDGE a racing dispatcher (different holder) gets already-running
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.cli import agents_app
from fno.claims.core import acquire_claim, claim_status

runner = CliRunner()


@pytest.fixture
def claims_tmp(tmp_path: Path, monkeypatch):
    """Root BOTH node:<id> (global root) and dispatch:<id> (cwd/env root) claims
    under one tmp dir, and force the Python agents dispatch so the verb never
    execs the Rust binary."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    return tmp_path


def _invoke(*args: str):
    return runner.invoke(agents_app, ["spawn-guard", *args])


def _parse_line(output: str) -> dict[str, str]:
    """Parse the bareword `verdict=v key=value` line into a dict."""
    import shlex

    out: dict[str, str] = {}
    # exactly one non-empty line is emitted
    lines = [ln for ln in output.splitlines() if ln.strip()]
    assert len(lines) == 1, f"expected exactly one verdict line, got: {output!r}"
    for tok in shlex.split(lines[0]):
        k, _, v = tok.partition("=")
        out[k] = v
    return out


# --- AC1-HP / AC1-FR: dispatchable acquires the reservation ------------------


def test_dispatchable_free_node_acquires_reservation(claims_tmp):
    res = _invoke("x-aaaa", "--holder", "dispatch-node:111", "--json")
    assert res.exit_code == 0
    obj = json.loads(res.output)
    assert obj["verdict"] == "dispatchable"
    assert obj["reservation_key"] == "dispatch:x-aaaa"
    assert obj["reservation_holder"] == "dispatch-node:111"
    # AC1-FR: the reservation is genuinely held by the caller now.
    status = claim_status("dispatch:x-aaaa")
    assert status["state"] in ("live", "stale")
    assert status["holder"] == "dispatch-node:111"


def test_dispatchable_bareword_line_parses(claims_tmp):
    res = _invoke("x-bbbb", "--holder", "h")
    assert res.exit_code == 0
    fields = _parse_line(res.output)
    assert fields["verdict"] == "dispatchable"
    assert fields["reservation_key"] == "dispatch:x-bbbb"
    assert fields["reservation_holder"] == "h"


# --- AC1-EDGE: live + corrupted -> no reservation ----------------------------


def test_live_claim_already_running_no_reservation(claims_tmp):
    import os

    # A live node:<id> claim held by the running test process.
    acquire_claim("node:x-cccc", "target-session:owner", pid=os.getpid())
    res = _invoke("x-cccc", "--holder", "dispatch-node:222", "--json")
    assert res.exit_code == 0
    obj = json.loads(res.output)
    assert obj["verdict"] == "already-running"
    assert obj["reason"] == "live-claim"
    assert obj["holder"] == "target-session:owner"
    # No reservation was taken.
    assert claim_status("dispatch:x-cccc")["state"] == "free"


def test_corrupted_claim_verdict_no_reservation(claims_tmp):
    # Write a garbage lock file at the node:<id> path so the probe classifies it
    # corrupted (claim_status returns state=corrupted, never raises).
    from fno.claims.core import claim_path
    from fno.claims.io import global_claims_root

    path = claim_path("node:x-dddd", root=global_claims_root())
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{ this is not valid claim yaml :::", encoding="utf-8")

    res = _invoke("x-dddd", "--holder", "h", "--json")
    assert res.exit_code == 0  # corrupted is a clean verdict
    obj = json.loads(res.output)
    assert obj["verdict"] == "corrupted"
    assert "force-release or repair" in obj["detail"]
    assert claim_status("dispatch:x-dddd")["state"] == "free"


# --- AC2-EDGE: racing dispatcher serializes ----------------------------------


def test_racing_reservation_already_running(claims_tmp):
    import os

    # A peer dispatcher already holds dispatch:<id> (live).
    acquire_claim("dispatch:x-eeee", "dispatch-node:peer", pid=os.getpid(), ttl_ms=180_000)
    res = _invoke("x-eeee", "--holder", "dispatch-node:me", "--json")
    assert res.exit_code == 0
    obj = json.loads(res.output)
    assert obj["verdict"] == "already-running"
    assert obj["reason"] == "reservation-held"
    # The peer still owns the reservation; we did not steal it.
    assert claim_status("dispatch:x-eeee")["holder"] == "dispatch-node:peer"


def test_same_holder_reacquire_is_idempotent_dispatchable(claims_tmp):
    import os

    acquire_claim("dispatch:x-ffff", "dispatch-node:me", pid=os.getpid(), ttl_ms=180_000)
    # Same holder -> idempotent re-acquire -> still dispatchable.
    res = _invoke("x-ffff", "--holder", "dispatch-node:me", "--json")
    assert res.exit_code == 0
    assert json.loads(res.output)["verdict"] == "dispatchable"


# --- --no-reserve: Guard 1 only ----------------------------------------------


def test_no_reserve_free_node_no_reservation(claims_tmp):
    res = _invoke("x-1111", "--holder", "h", "--no-reserve", "--json")
    assert res.exit_code == 0
    obj = json.loads(res.output)
    assert obj["verdict"] == "dispatchable"
    assert "reservation_key" not in obj
    assert claim_status("dispatch:x-1111")["state"] == "free"


def test_no_reserve_live_claim_still_already_running(claims_tmp):
    import os

    acquire_claim("node:x-2222", "owner", pid=os.getpid())
    res = _invoke("x-2222", "--holder", "h", "--no-reserve", "--json")
    assert res.exit_code == 0
    assert json.loads(res.output)["verdict"] == "already-running"


# --- AC1-ERR: a crashing probe fails closed ----------------------------------


def test_probe_crash_fails_closed(claims_tmp, monkeypatch):
    def _boom(*a, **k):
        raise RuntimeError("simulated probe crash")

    monkeypatch.setattr("fno.claims.core.claim_status", _boom)
    res = _invoke("x-3333", "--holder", "h", "--json")
    assert res.exit_code != 0  # fail-closed -> non-zero
    obj = json.loads(res.output)
    assert obj["verdict"] == "error"
    assert "not dispatching" in obj["detail"]
    # No reservation leaked.
    assert claim_status("dispatch:x-3333")["state"] == "free"


# --- fail-closed on a malformed --ttl (broadened acquire except; gemini review) ---


def test_malformed_ttl_fails_closed(claims_tmp):
    # _parse_ttl raises ValueError on a bad TTL; the verb must fail CLOSED
    # (verdict=error, non-zero) rather than trace out, so the caller refuses.
    res = _invoke("x-4444", "--holder", "h", "--ttl", "not-a-ttl", "--json")
    assert res.exit_code != 0
    obj = json.loads(res.output)
    assert obj["verdict"] == "error"
    # No reservation leaked.
    assert claim_status("dispatch:x-4444")["state"] == "free"


# --- AC1-UI: discoverability (Python-registered, not Rust-shadowed) ----------


def test_spawn_guard_listed_in_agents_help(claims_tmp):
    res = runner.invoke(agents_app, ["--help"])
    assert res.exit_code == 0
    assert "spawn-guard" in res.output
