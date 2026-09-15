"""Ready selection must exclude positively worked nodes."""
from __future__ import annotations

import threading

import pytest

from fno.graph import store


class _Keeper:
    def __init__(self):
        self.params = None

    def request(self, method, params):
        assert method == "ready"
        self.params = params
        return {"rows": [], "drops": []}


def test_ready_unites_claimed_and_worked(monkeypatch):
    keeper = _Keeper()
    monkeypatch.setattr(store, "_client_for", lambda _path: keeper)
    monkeypatch.setattr("fno.graph.statuses.live_claimed_node_ids", lambda **_kw: {"claimed-node"})
    monkeypatch.setattr(
        "fno.graph.statuses.live_worked_node_ids",
        lambda **_kw: {"worked-node": ["bp-worker"]},
    )

    store.ready()

    assert keeper.params["claimed"] == ["claimed-node", "worked-node"]


def test_ready_names_worked_overlay_degradation(monkeypatch, capsys):
    keeper = _Keeper()
    monkeypatch.setattr(store, "_client_for", lambda _path: keeper)
    monkeypatch.setattr("fno.graph.statuses.live_claimed_node_ids", lambda **_kw: {"claimed-node"})

    def _raise(**_kw):
        raise RuntimeError("roster timeout")

    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", _raise)

    store.ready()

    assert keeper.params["claimed"] == ["claimed-node"]
    assert "worked overlay degraded: roster timeout" in capsys.readouterr().err


def test_ready_reuses_a_precomputed_occupancy(monkeypatch):
    """`backlog next` pays one strict read for its whole selection."""
    keeper = _Keeper()
    monkeypatch.setattr(store, "_client_for", lambda _path: keeper)

    def _boom(**_kw):
        raise AssertionError("precomputed occupancy must not be re-read")

    monkeypatch.setattr("fno.graph.statuses.live_claimed_node_ids", _boom)
    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", _boom)

    store.ready(occupancy={"worked-node", "claimed-node"})

    assert keeper.params["claimed"] == ["claimed-node", "worked-node"]


def test_ready_reads_claims_and_worked_concurrently(monkeypatch):
    """AC3-HP: the two occupancy legs overlap. Both wait on the same
    barrier, so a sequential spelling breaks it (5s bound) and fails here.
    The 7.18s claim read plus the 8.96s worked read queueing one after the
    other is the wall clock this node is about."""
    keeper = _Keeper()
    monkeypatch.setattr(store, "_client_for", lambda _path: keeper)
    barrier = threading.Barrier(2, timeout=5)

    def _claimed(**_kw):
        barrier.wait()
        return {"claimed-node"}

    def _worked(**_kw):
        barrier.wait()
        return {"worked-node": ["bp-worker"]}

    monkeypatch.setattr("fno.graph.statuses.live_claimed_node_ids", _claimed)
    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", _worked)

    store.ready()

    assert keeper.params["claimed"] == ["claimed-node", "worked-node"]


def test_ready_claim_refusal_never_reaches_the_keeper(monkeypatch):
    """AC3-ERR: a claim reader that raises -> ClaimsUnavailableError
    carrying the original cause, and the keeper is never asked."""
    def _boom(**_kw):
        raise RuntimeError("claims root unreadable")

    monkeypatch.setattr("fno.graph.statuses.live_claimed_node_ids", _boom)
    monkeypatch.setattr(
        "fno.graph.statuses.live_worked_node_ids", lambda **_kw: {"w": ["x"]}
    )

    def _no_request(_path):
        raise AssertionError("the keeper must not be asked when claims refuse")

    monkeypatch.setattr(store, "_client_for", _no_request)

    from fno.graph.store import ClaimsUnavailableError

    with pytest.raises(ClaimsUnavailableError) as exc:
        store.ready()
    assert "claims root unreadable" in str(exc.value)
