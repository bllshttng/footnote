"""Claim-visibility barrier at the spawn-guard choke point (x-a7ab 1.2 / x-b44e).

spawn-guard is the single source of truth for the dispatch:<id> reservation,
called by both spawn.sh and dispatch-node.sh. After acquiring the reservation it
re-reads the claims dir to confirm THIS holder is the one on disk before
returning dispatchable; a peer that won a visibility-lagged race surfaces as a
different holder and this dispatcher skips with duplicate-claim so exactly one
worker launches.
"""
import json
from types import SimpleNamespace

from typer.testing import CliRunner

from fno.agents.cli import agents_app

runner = CliRunner()


def _last_json(out: str) -> dict:
    for line in reversed(out.strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    raise AssertionError(f"no JSON verdict in output: {out!r}")


def _route_to(monkeypatch, root):
    # Route every claim key (node:/dispatch:) to an isolated tmp root so the real
    # acquire/claim_status machinery runs hermetically.
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: root)


def test_spawn_guard_serializes_two_callers(monkeypatch, tmp_path):
    # AC2-HP: two dispatch attempts for one node -> exactly one dispatchable.
    _route_to(monkeypatch, tmp_path)
    r1 = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "A", "--ttl", "3m", "--json"])
    assert r1.exit_code == 0, r1.output
    assert _last_json(r1.output)["verdict"] == "dispatchable"
    r2 = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "B", "--ttl", "3m", "--json"])
    v2 = _last_json(r2.output)
    assert v2["verdict"] == "already-running"
    assert v2["reason"] in ("reservation-held", "duplicate-claim")


def test_barrier_catches_peer_after_acquire(monkeypatch, tmp_path):
    # The barrier's distinct value: acquire succeeds, but the post-acquire re-read
    # sees a peer (a visibility-lagged race resolved against us) -> duplicate-claim,
    # never dispatchable.
    _route_to(monkeypatch, tmp_path)

    def status(key, root=None):
        if key == "node:N":
            return {"key": key, "state": "free"}
        return {"key": key, "state": "live", "holder": "target-session:PEER"}

    monkeypatch.setattr("fno.claims.core.claim_status", status)
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: SimpleNamespace(holder="ME"))
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--ttl", "3m", "--json"])
    v = _last_json(r.output)
    assert v["verdict"] == "already-running"
    assert v["reason"] == "duplicate-claim"
    assert v["holder"] == "target-session:PEER"


def test_barrier_passes_when_holder_is_ours(monkeypatch, tmp_path):
    _route_to(monkeypatch, tmp_path)
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, root=None: (
            {"key": key, "state": "free"}
            if key == "node:N"
            else {"key": key, "state": "live", "holder": "ME"}
        ),
    )
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: SimpleNamespace(holder="ME"))
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--ttl", "3m", "--json"])
    assert _last_json(r.output)["verdict"] == "dispatchable"


def test_no_reserve_skips_barrier(monkeypatch, tmp_path):
    # --no-reserve runs Guard 1 only and never acquires (read-only verdict).
    _route_to(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.claims.core.claim_status", lambda key, root=None: {"key": key, "state": "free"})
    acq = []
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: acq.append(1))
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--no-reserve", "--json"])
    assert _last_json(r.output)["verdict"] == "dispatchable"
    assert acq == []


def test_reservation_precedes_dispatchable(monkeypatch, tmp_path):
    # Ordering invariant: the reservation is acquired before the dispatchable
    # verdict - no observable work precedes the on-disk reservation.
    _route_to(monkeypatch, tmp_path)
    order = []

    def status(key, root=None):
        order.append(("probe", key))
        return (
            {"key": key, "state": "free"}
            if key == "node:N"
            else {"key": key, "state": "live", "holder": "ME"}
        )

    def acq(*a, **k):
        order.append(("acquire", a[0] if a else k.get("key")))
        return SimpleNamespace(holder="ME")

    monkeypatch.setattr("fno.claims.core.claim_status", status)
    monkeypatch.setattr("fno.claims.core.acquire_claim", acq)
    r = runner.invoke(agents_app, ["spawn-guard", "N", "--holder", "ME", "--ttl", "3m", "--json"])
    assert _last_json(r.output)["verdict"] == "dispatchable"
    assert order[0] == ("probe", "node:N")  # Guard 1 first
    assert ("acquire", "dispatch:N") in order  # reservation on disk
