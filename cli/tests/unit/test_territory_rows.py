"""x-e221 task 4.1: the territory readout projection (active_backlog.territory_rows)."""
import fno.active_backlog as ab
import fno.king.scope as scope_mod


class _Census:
    def __init__(self, names, nodes):
        self.live_registry_names = set(names)
        self.live_row_nodes = list(nodes)


class _Agents:
    max_live_per_territory = 4


class _Settings:
    agents = _Agents()


def _patch_world(monkeypatch, entries, records=None):
    """One graph (x-epic with children x-1/x-2; project fno with p-1), two
    territories (crowned epic + kingless project), a live census, and a cap."""
    monkeypatch.setattr(
        ab,
        "_territories",
        lambda strict=False: [
            {"scope": "x-epic", "rung": 2, "kingless": False, "members": ["x-epic"]},
            {"scope": "fno", "rung": 1, "kingless": True, "members": ["fno"]},
        ],
    )
    monkeypatch.setattr(
        ab,
        "_live_crowns",
        lambda strict=False: [{"scope": "x-epic", "level": 2, "holder": "king-1"}],
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate.census",
        lambda: _Census(names={"bp-x", "w1"}, nodes=["x-1", "x-2"]),
    )
    monkeypatch.setattr("fno.graph.store.read_graph", lambda path: entries)
    monkeypatch.setattr("fno.config.load_settings", lambda: _Settings())

    def tm_for(scope, entries, **k):
        ids = {
            "x-epic": {"x-epic", "x-1", "x-2"},
            "fno": {"x-epic", "x-1", "x-2", "p-1"},
        }[scope]
        return scope_mod.TerritoryMembership(state="ok", key=scope, ids=frozenset(ids))

    monkeypatch.setattr("fno.king.scope.territory_membership", tm_for)
    records = records or {}
    monkeypatch.setattr(
        "fno.worker.blueprint._read_record", lambda key: records.get(key, {"worker": None, "fed": {}, "repairs": []})
    )


def _entries():
    return [
        {"id": "x-epic", "type": "epic", "project": "fno"},
        {"id": "x-1", "parent": "x-epic", "project": "fno"},
        {"id": "x-2", "parent": "x-epic", "project": "fno"},
        {"id": "p-1", "project": "fno"},
    ]


def test_each_territory_row_names_its_own_state(monkeypatch):
    _patch_world(
        monkeypatch,
        _entries(),
        records={
            "x-epic": {
                "worker": {"name": "bp-x", "spawned_at": "t"},
                "fed": {"x-a": {"at": "t", "ok": True}},
                "repairs": [{"ts": "t", "reason": "r"}],
            }
        },
    )

    rows = ab.territory_rows()
    crowned = next(r for r in rows if r["scope"] == "x-epic")
    loose = next(r for r in rows if r["scope"] == "fno")

    assert crowned["membership"] == "ok"
    assert crowned["kingless"] is False and crowned["holder"] == "king-1"
    assert crowned["mission"] == "x-epic"
    assert crowned["live"] == 2 and crowned["cap"] == 4
    assert crowned["blueprinter"] == {
        "name": "bp-x",
        "live": True,
        "spawned_at": "t",
        "fed": 1,
        "repairs": 1,
    }
    assert loose["kingless"] is True and loose["holder"] is None
    assert loose["live"] == 0 and loose["blueprinter"] is None


def test_membership_unknown_reads_as_unknown_never_zero(monkeypatch):
    _patch_world(monkeypatch, _entries())

    def unknown(scope, entries, **k):
        return scope_mod.TerritoryMembership(
            state="unknown", reason="crown scope x-epic is not an epic in the graph"
        )

    monkeypatch.setattr("fno.king.scope.territory_membership", unknown)

    rows = ab.territory_rows()
    crowned = next(r for r in rows if r["scope"] == "x-epic")
    assert crowned["membership"] == "unknown"
    assert crowned["live"] is None
    assert crowned["cap"] == 4


def test_a_dead_blueprinter_handle_reads_not_live(monkeypatch):
    _patch_world(
        monkeypatch,
        _entries(),
        records={
            "x-epic": {
                "worker": {"name": "bp-dead", "spawned_at": "t"},
                "fed": {},
                "repairs": [],
            }
        },
    )

    rows = ab.territory_rows()
    crowned = next(r for r in rows if r["scope"] == "x-epic")
    assert crowned["blueprinter"]["live"] is False


def test_cap_read_failure_keeps_the_default(monkeypatch):
    _patch_world(monkeypatch, _entries())
    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: (_ for _ in ()).throw(RuntimeError("no config")),
    )

    rows = ab.territory_rows()
    assert rows[0]["cap"] == 4
