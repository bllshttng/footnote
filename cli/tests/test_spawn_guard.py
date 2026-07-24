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


# --- x-ba4b: suspect claim -> skip-not-steal ---------------------------------


def test_suspect_claim_already_running_no_reservation(claims_tmp):
    """A TTL-unexpired claim with a dead pid (respawned worker) reads suspect;
    spawn-guard must report already-running/reason=suspect-claim and take NO
    reservation, so the dispatcher maps it to skipped-contested and never steals."""
    import psutil

    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    # TTL unexpired + dead pid -> classify() == suspect.
    acquire_claim(
        "node:x-9999", "target-session:respawned", pid=dead_pid, ttl_ms=180_000
    )
    assert claim_status("node:x-9999")["state"] == "suspect"

    res = _invoke("x-9999", "--holder", "dispatch-node:333", "--json")
    assert res.exit_code == 0
    # x-5c08 deliberately emits a loud contested-claim warning on stderr; the
    # JSON stdout contract remains one parseable object for shell callers.
    obj = json.loads(res.stdout)
    assert obj["verdict"] == "already-running"
    assert obj["reason"] == "suspect-claim"
    assert obj["holder"] == "target-session:respawned"
    assert "WARNING: dispatch blocked for x-9999" in res.stderr
    # No reservation was taken - the node stays for the live worker.
    assert claim_status("dispatch:x-9999")["state"] == "free"


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


def test_dead_failure_limit_refuses_spawn_guard_path(claims_tmp, monkeypatch: pytest.MonkeyPatch):
    """The shell dispatch choke consumes the family-2 failure-limit decision."""
    from fno.config import SettingsModel
    from fno.graph import failure

    settings = SettingsModel(active_backlog={"failure_limit": 2})
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    monkeypatch.setattr(
        failure,
        "read_events",
        lambda *_a, **_k: [
            {"type": "node_failed", "data": {"unit_id": "x-dead"}},
            {"type": "node_failed", "data": {"unit_id": "x-dead"}},
        ],
    )
    monkeypatch.setattr(
        "fno.target_cli._classify_node_claim",
        lambda _node: ("free", None),
        raising=False,
    )
    monkeypatch.setattr(
        "fno.agents.truth_status.resolve_truth_status",
        lambda *_a, **_k: {"state": "unknown"},
    )
    monkeypatch.setattr(
        "fno.backlog.advance.subprocess.run",
        lambda *_a, **_k: type("P", (), {"returncode": 0, "stdout": "", "stderr": ""})(),
    )
    monkeypatch.setattr("fno.agents.events.emit", lambda *_a, **_k: None)
    monkeypatch.setattr("fno.notify._impl.send_notification", lambda *_a, **_k: (0, ""))

    res = _invoke("x-dead", "--holder", "dispatch-node:me", "--json")

    assert res.exit_code == 0
    obj = json.loads(res.output)
    assert obj["verdict"] == "refused"
    assert obj["reason"] == "auto-deferred"
    assert claim_status("dispatch:x-dead")["state"] == "free"


def test_direct_node_spawn_crosses_shared_guard_before_spawn(
    claims_tmp, monkeypatch: pytest.MonkeyPatch
):
    """A direct `fno agents spawn --node` cannot bypass the shell guard."""
    import fno.agents.cli as cli_mod
    from fno.agents import mux_spawn, spawn_gate

    monkeypatch.setattr(
        cli_mod,
        "_spawn_guard_decision",
        lambda *_a, **_k: (
            {
                "verdict": "refused",
                "reason": "auto-deferred",
                "holder": "target-session:prior",
            },
            0,
        ),
    )
    monkeypatch.setattr(
        mux_spawn,
        "resolve_provenance",
        lambda *_a, **_k: {"FNO_NODE": "x-dead"},
    )
    monkeypatch.setattr(
        spawn_gate,
        "run_gate",
        lambda *_a, **_k: pytest.fail("guard refusal must precede the spawn gate"),
    )

    res = runner.invoke(
        agents_app,
        ["spawn", "--name", "dead-worker", "--node", "x-dead", "--here", "work"],
    )

    assert res.exit_code == 2
    assert "reason=auto-deferred" in res.output
    assert "no worker launched" in res.output


def test_direct_node_spawn_failure_releases_shared_reservation(
    claims_tmp, monkeypatch: pytest.MonkeyPatch
):
    """A substrate failure leaves a node immediately re-dispatchable."""
    import fno.agents.cli as cli_mod
    from fno.agents import mux_spawn, spawn_gate
    from fno.agents.dispatch import DispatchAskError

    monkeypatch.setattr(
        cli_mod,
        "_spawn_guard_decision",
        lambda *_a, **_k: (
            {
                "verdict": "dispatchable",
                "reservation_key": "dispatch:x-fail",
                "reservation_holder": "spawn-cli:test",
            },
            0,
        ),
    )
    monkeypatch.setattr(
        mux_spawn,
        "resolve_provenance",
        lambda *_a, **_k: {"FNO_NODE": "x-fail"},
    )
    monkeypatch.setattr(
        spawn_gate, "run_gate", lambda *_a, **_k: type("G", (), {"release": lambda self: None})()
    )
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **_k: (_ for _ in ()).throw(DispatchAskError("mux failed", exit_code=7)),
    )
    released: list[tuple[str, str]] = []
    monkeypatch.setattr(
        "fno.claims.core.release_claim",
        lambda key, holder, **_k: released.append((key, holder)),
    )

    res = runner.invoke(
        agents_app,
        ["spawn", "--name", "failed-worker", "--node", "x-fail", "--here", "work"],
    )

    assert res.exit_code == 7
    assert released == [("dispatch:x-fail", "spawn-cli:test")]


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


# --- AC1-UI: reachable (Python-registered, not Rust-shadowed) ----------------
# spawn-guard is a machine verb (spawn.sh calls it, not a human), so x-4ce9
# hides it from `fno agents --help`. The original discoverability concern was
# routing - that it stays Python-registered and is not shadowed by the Rust
# runtime - which `hidden=True` preserves: the verb is still invokable, just
# unlisted. Assert reachability by invocation, and that it is intentionally
# absent from the group listing.


def test_spawn_guard_hidden_but_reachable(claims_tmp):
    listing = runner.invoke(agents_app, ["--help"])
    assert listing.exit_code == 0
    assert "spawn-guard" not in listing.output  # hidden from the human listing
    own = runner.invoke(agents_app, ["spawn-guard", "--help"])
    assert own.exit_code == 0  # still Python-registered + invokable


# --- x-4652: orphaned dispatch reservation reaps (already-correct path) -------


def test_expired_dead_dispatch_reservation_reaped(claims_tmp, monkeypatch):
    """Regression (x-4652): a dispatch:<id> reservation whose TTL has expired AND
    whose recorded pid is dead is STALE, so spawn-guard reaps it and dispatches,
    taking a fresh reservation. This locks the already-correct reap path (the
    literal 'expired + dead PID' case classify handles) against regression.

    The residual x-4652 concern - a within-TTL reservation whose dispatcher pid
    is dead reads SUSPECT and is protected for up to the 3m TTL - is left as-is:
    it self-heals at TTL and is arguably a correct boot-window guard. The
    SUSPECT-arm behavior is asserted by
    test_suspect_claim_already_running_no_reservation above."""
    import psutil
    from fno.claims import staleness

    dead_pid = 999_999
    while psutil.pid_exists(dead_pid):
        dead_pid += 1
    # Orphan: dispatcher died mid-launch, reservation taken with the shortest
    # legal TTL. Advance the clock past expiry (the reservation self-heals at
    # TTL in reality; here we jump the clock instead of sleeping 60s).
    acquire_claim(
        "dispatch:x-7777", "dispatch-node:orphan", pid=dead_pid, ttl_ms=60_000
    )
    future = staleness.now_ms() + 120_000
    monkeypatch.setattr(staleness, "now_ms", lambda: future)
    assert claim_status("dispatch:x-7777")["state"] == "stale"

    res = _invoke("x-7777", "--holder", "dispatch-node:fresh", "--json")
    assert res.exit_code == 0
    obj = json.loads(res.output)
    assert obj["verdict"] == "dispatchable"
    assert obj["reservation_key"] == "dispatch:x-7777"
    assert obj["reservation_holder"] == "dispatch-node:fresh"
    # The orphan was reaped; the fresh dispatcher now holds the reservation.
    assert claim_status("dispatch:x-7777")["holder"] == "dispatch-node:fresh"
