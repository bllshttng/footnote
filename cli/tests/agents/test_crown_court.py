"""``fno agents court``: one read of every crown and whether the graph agrees.

Agreement is a POSITIVE marker (AGENTS.md's positive-marker pitfall): a court
that shows no disagreements because it could not read anything must never
render the same as a healthy fleet. ``agree`` is ``None`` with a stated
reason on an unreadable graph - never ``True``, never ``False`` - and the
summary counts unknowns separately from disagreements.
"""
from __future__ import annotations

import json
from pathlib import Path


from fno.paths_testing import use_tmpdir


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry

    harness = kw.pop("harness", "claude")
    kw.setdefault("harness_session_id", f"{name}-session")
    return AgentEntry(name=name, cwd="/w", log_path="", harness=harness, **kw)


def _prepare(monkeypatch, tmp_path, rows, graph_entries=None) -> None:
    from fno import paths
    from fno.agents.registry import write_registry
    from fno.projects import resolve as proj_resolve

    use_tmpdir(monkeypatch, tmp_path)
    write_registry(rows)
    config = tmp_path / "config.toml"
    config.write_text(
        '[work.workspaces.ws1]\n'
        'projects = [{ name = "alpha", short_name = "a" }, { name = "beta" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", config)
    proj_resolve._clear_cache()
    if graph_entries is not None:
        graph_path = paths.graph_json()
        graph_path.parent.mkdir(parents=True, exist_ok=True)
        graph_path.write_text(
            json.dumps({"entries": graph_entries}), encoding="utf-8"
        )


def test_a_crown_over_a_real_live_epic_agrees(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=2,
                crown_scope="e-1",
                crown_grantor="human",
            )
        ],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )

    court = gather_court()

    assert court["crowns"] == [
        {
            "holder": "king",
            "level": 2,
            "scope": "e-1",
            "grantor": "human",
            "status": "busy",
            "agree": True,
            "reason": None,
        }
    ]
    assert court["summary"] == {"total": 1, "disagreements": 0, "unknowns": 0}


def test_a_crown_over_an_id_the_graph_does_not_hold_disagrees(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=2,
                crown_scope="ghost-epic",
                crown_grantor="human",
            )
        ],
        graph_entries=[],
    )

    court = gather_court()

    row = court["crowns"][0]
    assert row["agree"] is False
    assert "ghost-epic" in row["reason"]
    assert court["summary"] == {"total": 1, "disagreements": 1, "unknowns": 0}


def test_a_crown_over_a_wrongly_typed_node_disagrees(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=2,
                crown_scope="n-1",
                crown_grantor="human",
            )
        ],
        graph_entries=[{"id": "n-1", "type": "feature", "project": "alpha"}],
    )

    court = gather_court()

    row = court["crowns"][0]
    assert row["agree"] is False
    assert "not an epic" in row["reason"]


def test_unreadable_graph_answers_null_never_true_or_false(
    tmp_path: Path, monkeypatch
) -> None:
    """AC7-EDGE: every affected row reads agree=null with a stated reason, and
    the unknown count is tracked separately from disagreements."""
    from fno.agents.court import gather_court
    from fno.tracker import metadata

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=2,
                crown_scope="e-1",
                crown_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(
        metadata,
        "read_entries",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("unreadable")),
    )

    court = gather_court()

    row = court["crowns"][0]
    assert row["agree"] is None
    assert row["reason"] is not None
    assert court["graph_readable"] is False
    assert court["summary"] == {"total": 1, "disagreements": 0, "unknowns": 1}


def test_a_portfolio_crown_over_configured_projects_agrees(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=0,
                crown_scope="alpha,beta",
                crown_grantor="human",
            )
        ],
        graph_entries=[],
    )

    court = gather_court()

    assert court["crowns"][0]["agree"] is True


def test_terminal_rows_are_excluded_from_the_court(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "dead-king",
                status="exited",
                crown_level=1,
                crown_scope="alpha",
                crown_grantor="human",
            )
        ],
        graph_entries=[],
    )

    court = gather_court()

    assert court["crowns"] == []
    assert court["summary"] == {"total": 0, "disagreements": 0, "unknowns": 0}


def test_two_live_rows_holding_the_same_territory_is_a_conflict(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king-a",
                status="busy",
                crown_level=1,
                crown_scope="alpha",
                crown_grantor="human",
            ),
            _entry(
                "king-b",
                status="idle",
                crown_level=1,
                crown_scope="alpha",
                crown_grantor="human",
            ),
        ],
        graph_entries=[],
    )

    court = gather_court()

    assert court["conflicts"] == [{"scope": "alpha", "holders": ["king-a", "king-b"]}]


def test_render_court_json_matches_gather_court(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import gather_court, render_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=2,
                crown_scope="e-1",
                crown_grantor="human",
            )
        ],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )

    assert json.loads(render_court(as_json=True)) == gather_court()


def test_render_court_table_names_scope_holder_and_agreement(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import render_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=1,
                crown_scope="alpha",
                crown_grantor="human",
            )
        ],
        graph_entries=[],
    )

    text = render_court(as_json=False)

    assert "alpha" in text
    assert "king" in text
    assert "court: 1 crown, 0 disagreements, 0 unknowns" in text


def test_unreadable_registry_nulls_the_court_rather_than_reporting_it_empty(
    tmp_path: Path, monkeypatch
) -> None:
    """The absence-lie one layer below the agreement verdict. Degrading a failed
    registry read to [] would print a healthy, empty court, so a caller gating
    on summary.disagreements == 0 passes on a read that saw nothing."""
    from fno.agents import court as court_mod
    from fno.agents.court import gather_court, render_court

    _prepare(monkeypatch, tmp_path, [], graph_entries=[])
    monkeypatch.setattr(
        court_mod,
        "load_registry",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("truncated")),
        raising=False,
    )
    monkeypatch.setattr(
        "fno.agents.registry.load_registry",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("truncated")),
    )

    court = gather_court()

    assert court["registry_readable"] is False
    assert court["crowns"] is None
    # Every count is null, so no naive zero-gate can read this as healthy.
    assert court["summary"]["total"] is None
    assert court["summary"]["disagreements"] is None
    assert court["summary"]["unknowns"] is None
    text = render_court(as_json=False)
    assert "CANNOT READ" in text
    assert "nothing was checked" in text


def test_a_half_crown_renders_and_never_certifies_agreement(
    tmp_path: Path, monkeypatch
) -> None:
    """A row with a level but no scope rules no territory. The court is the read
    meant to SURFACE that corruption, so it must not crash on it (formatting a
    null scope to a width raises) and must not certify it as agreeing."""
    from fno.agents.court import gather_court, render_court

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("half", status="busy", crown_level=1, crown_scope=None)],
        graph_entries=[],
    )

    court = gather_court()

    assert court["crowns"][0]["agree"] is False
    assert "half a crown" in court["crowns"][0]["reason"]
    assert court["summary"]["disagreements"] == 1
    # The table renders rather than raising TypeError on the null scope.
    assert "half" in render_court(as_json=False)


def test_project_rungs_stay_determinate_when_the_graph_is_unreadable(
    tmp_path: Path, monkeypatch
) -> None:
    """Only the epic rung consults the graph. A project or portfolio crown
    resolves entirely from config, so an external tracker backend must not
    blank out an answer that is fully determinate."""
    from fno.agents.court import gather_court
    from fno.tracker import metadata

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "king",
                status="busy",
                crown_level=1,
                crown_scope="alpha",
                crown_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(
        metadata,
        "read_entries",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("external backend")),
    )

    court = gather_court()

    assert court["crowns"][0]["agree"] is True
    assert court["summary"]["unknowns"] == 0


def test_no_live_crowns_renders_a_plain_statement(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [], graph_entries=[])

    assert render_court(as_json=False) == "court: no live crowns"
