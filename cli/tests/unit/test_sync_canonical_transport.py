"""Transport tests for the native sync-canonical verb door.

The sync/catch-up/staleness logic is native (crates/fno-agents
src/sync_canonical.rs); these tests pin only the Python side: payload
carriage, line echo, exit passthrough, the VerbUnavailable fallbacks, and the
two caller contracts (reconcile --json, doctor health).
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json

import pytest


def _stub_verb(monkeypatch, answer):
    seen = {}

    def fake(verb, payload, **kw):
        seen["verb"] = verb
        seen["payload"] = payload
        seen["timeout"] = kw.get("timeout")
        if isinstance(answer, Exception):
            raise answer
        return answer

    monkeypatch.setattr("fno.rust_binary.verb_call", fake)
    return seen


def test_sync_exits_passthrough_and_echoes_lines(monkeypatch, capsys):
    from fno.pr import _sync_canonical as sc

    seen = _stub_verb(
        monkeypatch,
        {
            "exit": 1,
            "stdout": ["post-merge sync: running in /c for abcdef123456"],
            "stderr": ["post-merge sync: failed (exit 1); marker withheld, will retry"],
        },
    )
    rc = sc.run_sync_canonical(5)
    assert rc == 1
    assert seen["verb"] == "sync-canonical"
    assert seen["payload"]["pr"] == 5
    # The door must outlast the 600s shell bound, not report it unreachable.
    assert seen["timeout"] >= 600
    out = capsys.readouterr()
    assert "running in /c for abcdef123456" in out.out
    assert "failed (exit 1); marker withheld, will retry" in out.err


def test_sync_verb_unavailable_returns_1_and_withholds(monkeypatch, capsys):
    from fno.pr import _sync_canonical as sc
    from fno.rust_binary import VerbUnavailable

    _stub_verb(monkeypatch, VerbUnavailable("the fno-agents binary was not found"))
    rc = sc.run_sync_canonical(5)
    assert rc == 1
    out = capsys.readouterr()
    assert "native verb unavailable" in out.err
    assert "binary was not found" in out.err


def test_catchup_returns_the_answer_fields_and_echoes(monkeypatch, capsys):
    from fno.pr import _sync_canonical as sc

    seen = _stub_verb(
        monkeypatch,
        {
            "exit": 0,
            "stdout": ["post-merge sync catch-up: synced PR #7, stamped 2 older merge(s)"],
            "stderr": [],
            "outcome": "synced",
            "pr_number": 7,
            "swept": 2,
            "detail": "",
            "stale": False,
        },
    )
    result = sc.run_sync_catchup()
    assert seen["payload"]["action"] == "catchup"
    assert result == {
        "outcome": "synced",
        "pr_number": 7,
        "swept": 2,
        "detail": "",
        "stale": False,
    }
    assert "synced PR #7" in capsys.readouterr().out


def test_catchup_verb_unavailable_reads_unknown(monkeypatch):
    from fno.pr import _sync_canonical as sc
    from fno.rust_binary import VerbUnavailable

    _stub_verb(monkeypatch, VerbUnavailable("spawn trouble"))
    result = sc.run_sync_catchup()
    assert result["outcome"] == "unknown"
    assert "spawn trouble" in result["detail"]


def test_staleness_returns_the_answer_fields(monkeypatch):
    from fno.pr import _sync_canonical as sc

    seen = _stub_verb(
        monkeypatch,
        {
            "exit": 0,
            "stdout": [],
            "stderr": [],
            "state": "stale",
            "markerless": [{"number": 50, "sha": "a" * 40, "merged_at": "2026-09-16T10:00:00Z"}],
            "behind": 3,
            "detail": "PR #50 merged 48h ago, never synced",
        },
    )
    result = sc.sync_staleness(fetch=True)
    assert seen["payload"]["action"] == "staleness"
    assert seen["payload"]["fetch"] is True
    assert result["state"] == "stale"
    assert result["behind"] == 3
    assert result["markerless"][0]["number"] == 50


def test_staleness_verb_unavailable_reads_unknown(monkeypatch):
    from fno.pr import _sync_canonical as sc
    from fno.rust_binary import VerbUnavailable

    _stub_verb(monkeypatch, VerbUnavailable("bad output"))
    result = sc.sync_staleness(fetch=False)
    assert result["state"] == "unknown"
    assert result["behind"] is None
    assert result["markerless"] == []


# -- caller contracts -------------------------------------------------------


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    seed_graph(g, '{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    for mod, attr, val in (
        (gc, "GRAPH_JSON", g),
        (gc, "GRAPH_MD", tmp_path / "graph.md"),
        (gc, "GRAPH_HTML", tmp_path / "graph.html"),
        (gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json"),
        (gs, "GRAPH_JSON", g),
    ):
        monkeypatch.setattr(mod, attr, val)
    return g


def _reconcile_json(monkeypatch, catchup_result):
    """`fno backlog reconcile --json` with only the catch-up leg live."""
    from typer.testing import CliRunner

    from fno.graph import cli as gcli
    from fno.pr import _sync_canonical as sc_mod

    if catchup_result is None:
        monkeypatch.setattr(
            sc_mod,
            "run_sync_catchup",
            lambda **_kw: (_ for _ in ()).throw(RuntimeError("gh exploded")),
        )
    else:
        monkeypatch.setattr(sc_mod, "run_sync_catchup", lambda **_kw: catchup_result)
    res = CliRunner().invoke(gcli.cli, ["reconcile", "--json"])
    assert res.exit_code == 0, res.output
    return json.loads(res.stdout)


def test_reconcile_reports_catchup_in_json(tmp_graph, monkeypatch):
    """The SessionStart hook runs reconcile --json and discards stderr, so the
    outcome has to ride the payload or it is unobservable."""
    payload = _reconcile_json(
        monkeypatch,
        {"outcome": "synced", "stale": False, "pr_number": 52, "swept": 3, "detail": ""},
    )
    assert payload["sync_catchup"] == {
        "outcome": "synced",
        "stale": False,
        "pr_number": 52,
        "swept": 3,
        "detail": "",
    }


def test_reconcile_survives_a_catchup_exception(tmp_graph, monkeypatch):
    payload = _reconcile_json(monkeypatch, None)
    assert payload["sync_catchup"]["outcome"] == "error"
    assert "gh exploded" in payload["sync_catchup"]["detail"]


def test_reconcile_dry_run_never_syncs(tmp_graph, monkeypatch):
    from typer.testing import CliRunner

    from fno.graph import cli as gcli
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(
        sc_mod,
        "run_sync_catchup",
        lambda **_kw: pytest.fail("a preview must mutate nothing"),
    )
    res = CliRunner().invoke(gcli.cli, ["reconcile", "--json", "--dry-run"])
    assert res.exit_code == 0
    assert json.loads(res.stdout)["sync_catchup"]["outcome"] == "not-run"


def test_doctor_reports_staleness(monkeypatch):
    from fno import doctor
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(
        sc_mod,
        "sync_staleness",
        lambda **_kw: {
            "state": "stale",
            "markerless": [],
            "behind": 7,
            "detail": "PR #50 merged 48h ago",
        },
    )
    health = doctor._post_merge_sync_health()
    assert health["stale"] is True
    assert "#50" in health["detail"]
    assert health["behind"] == 7


def test_doctor_health_never_raises(monkeypatch):
    from fno import doctor
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(
        sc_mod,
        "sync_staleness",
        lambda **_kw: (_ for _ in ()).throw(RuntimeError("gh exploded")),
    )
    assert doctor._post_merge_sync_health() == {
        "state": "unknown",
        "stale": False,
        "behind": None,
        "detail": "",
    }
