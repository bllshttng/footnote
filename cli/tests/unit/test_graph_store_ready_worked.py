"""Ready selection must exclude positively worked nodes."""
from __future__ import annotations

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
