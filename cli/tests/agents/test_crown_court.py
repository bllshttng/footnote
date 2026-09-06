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

import pytest

from fno.plan._status import TERMINAL_STATUSES
from fno.paths_testing import use_tmpdir


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry

    harness = kw.pop("harness", "claude")
    kw.setdefault("cwd", "/w")
    kw.setdefault("harness_session_id", f"{name}-session")
    return AgentEntry(name=name, log_path="", harness=harness, **kw)


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
            "manifest_path": None,
            "manifest_session": None,
            "crown_source": "row",
        }
    ]
    s = court["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (1, 0, 0, 0)
    assert s["manifest_only"] == 0


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
    s = court["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (1, 1, 0, 0)
    assert s["manifest_only"] == 0


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


@pytest.mark.parametrize("status", TERMINAL_STATUSES)
def test_court_uses_every_canonical_plan_terminal_status(
    tmp_path: Path, monkeypatch, status: str
) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("king", status="busy", crown_level=2, crown_scope="e-1")],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": status}],
    )

    row = gather_court()["crowns"][0]

    assert row["agree"] is False
    assert status in row["reason"]


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
    s = court["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (1, 0, 1, 0)
    assert s["manifest_only"] == 0


def test_a_crown_over_a_set_of_epics_agrees_only_if_every_member_is_a_live_epic(
    tmp_path: Path, monkeypatch
) -> None:
    """Rung 2 stores a set; agreement must check every member, not the first -
    one dead member makes the whole crown disagree."""
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "mux-king",
                status="busy",
                crown_level=2,
                crown_scope="e-1,e-2",
                crown_grantor="human",
            )
        ],
        graph_entries=[
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"},
            {"id": "e-2", "type": "epic", "project": "beta", "status": "done"},
        ],
    )

    court = gather_court()

    row = court["crowns"][0]
    assert row["agree"] is False
    assert "e-2" in row["reason"]
    assert "done" in row["reason"]


def test_a_crown_over_two_live_epics_agrees(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "mux-king",
                status="busy",
                crown_level=2,
                crown_scope="e-1,e-2",
                crown_grantor="human",
            )
        ],
        graph_entries=[
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"},
            {"id": "e-2", "type": "epic", "project": "beta", "status": "ready"},
        ],
    )

    assert gather_court()["crowns"][0]["agree"] is True


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
    s = court["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (0, 0, 0, 0)
    assert s["manifest_only"] == 0


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


def test_aliases_and_ordered_scopes_share_one_conflict_group(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry("king-a", status="busy", crown_level=0, crown_scope="alpha,beta"),
            _entry("king-b", status="idle", crown_level=0, crown_scope="beta,a"),
        ],
        graph_entries=[],
    )

    assert gather_court()["conflicts"] == [
        {"scope": "alpha,beta", "holders": ["king-a", "king-b"]}
    ]


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


# --- the manifest limb (x-f0d2): manifest is the durable record, row the cache


def _stub_reign_reader(monkeypatch, tmp_path: Path, payload: dict, orphans=None, sweep_fail=False) -> None:
    """Answer reign-state with a canned payload and court-orphans with a
    canned array (test_crown_court pins the RENDER, not the reader;
    loop_reign.rs pins the comparison and the sweep). The sweep answer honors
    --held like the real binary (a held scope is filtered out) and can be
    made to fail, pinning the ran-marker."""
    import stat

    script = tmp_path / "fno-agents"
    body = (
        "#!/usr/bin/env python3\n"
        "import json, sys\n"
        f"REIGN = {json.dumps(json.dumps(payload))}\n"
        f"FAIL = {repr(bool(sweep_fail))}\n"
        f"ORPHANS = {json.dumps(orphans or [])}\n"
        "if 'court-orphans' not in sys.argv:\n"
        "    print(REIGN, end='')\n"
        "    sys.exit(0)\n"
        "if FAIL:\n"
        "    sys.exit(1)\n"
        "held = {v for i, v in enumerate(sys.argv) if i and sys.argv[i-1] == '--held'}\n"
        "print(json.dumps([o for o in ORPHANS if o.get('scope') not in held]), end='')\n"
    )
    script.write_text(body, encoding="utf-8")
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: script)


def _agreeing_reader(scope: str, session: str) -> dict:
    return {
        "crowned": True,
        "scope": scope,
        "shape": "pass",
        "manifest_session": session,
        "registry_session": session,
        "live": True,
        "split": False,
        "unknown_reason": None,
    }


def test_court_names_the_manifest_and_crown_source_per_scope(
    tmp_path: Path, monkeypatch
) -> None:
    """A crowned row with a matching crowned manifest renders source `both`."""
    import fno.king.state as king_state
    from fno.agents.court import gather_court
    from fno.paths import space_dir

    row = _entry(
        "king",
        cwd=str(tmp_path),
        status="busy",
        crown_level=2,
        crown_scope="e-1",
        crown_grantor="human",
    )
    _prepare(
        monkeypatch,
        tmp_path,
        [row],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    manifest = space_dir(tmp_path) / "kings" / "e-1.md"
    king_state.write_manifest(
        manifest,
        scope="e-1",
        harness_session_id="king-session",
        owner_cwd=str(tmp_path),
        crown_level=2,
        crown_scope="e-1",
        crown_grantor="human",
    )
    payload = _agreeing_reader("e-1", "king-session")
    payload.update(crown_on_manifest=True, manifest_path=str(manifest))
    _stub_reign_reader(monkeypatch, tmp_path, payload)

    court = gather_court()

    entry = court["crowns"][0]
    assert entry["crown_source"] == "both"
    assert entry["manifest_session"] == "king-session"
    assert entry["manifest_path"] == str(manifest)
    assert court["summary"]["splits"] == 0


def test_a_split_crown_counts_apart_from_disagreements_and_unknowns(
    tmp_path: Path, monkeypatch
) -> None:
    """Two stores naming different holders is a SPLIT, not a graph
    disagreement and not an unknown: the summary counts it separately."""
    from fno.agents.court import gather_court
    from fno.paths import space_dir
    import fno.king.state as king_state

    row = _entry(
        "king",
        cwd=str(tmp_path),
        status="busy",
        crown_level=2,
        crown_scope="e-1",
        crown_grantor="human",
    )
    _prepare(
        monkeypatch,
        tmp_path,
        [row],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    king_state.write_manifest(
        space_dir(tmp_path) / "kings" / "e-1.md",
        scope="e-1",
        harness_session_id="someone-else",
        owner_cwd=str(tmp_path),
        crown_level=2,
        crown_scope="e-1",
        crown_grantor="human",
    )
    payload = _agreeing_reader("e-1", "king-session")
    payload.update(manifest_session="someone-else", split=True)
    _stub_reign_reader(monkeypatch, tmp_path, payload)

    court = gather_court()

    entry = court["crowns"][0]
    assert entry["crown_source"] == "split"
    # The graph limb still answers its own question; the split is not folded in.
    assert entry["agree"] is True
    assert court["summary"]["disagreements"] == 0
    assert court["summary"]["unknowns"] == 0
    assert court["summary"]["splits"] == 1


def test_a_crown_on_only_the_manifest_is_surfaced_crownless_in_the_registry(
    tmp_path: Path, monkeypatch
) -> None:
    """A scope whose row vanished keeps its crown on the manifest; court must
    show it with the manifest named, not as an empty court."""
    import fno.king.state as king_state
    from fno.agents.court import gather_court
    from fno.paths import space_dir

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("plain-worker", cwd=str(tmp_path), status="busy")],
        graph_entries=[],
    )
    manifest = space_dir(tmp_path) / "kings" / "x-dede.md"
    king_state.write_manifest(
        manifest,
        scope="x-dede",
        harness_session_id="gone-king",
        owner_cwd=str(tmp_path),
        crown_level=2,
        crown_scope="x-dede",
        crown_grantor="operator",
    )
    _stub_reign_reader(
        monkeypatch, tmp_path, _agreeing_reader("x-dede", "gone-king"),
        orphans=[{
            "scope": "x-dede", "level": 2, "grantor": "operator",
            "manifest_session": "gone-king", "manifest_path": str(manifest),
        }],
    )

    court = gather_court()

    orphan = next(e for e in court["crowns"] if e["scope"] == "x-dede")
    assert orphan["crown_source"] == "manifest"
    assert orphan["manifest_path"] == str(manifest)
    assert orphan["manifest_session"] == "gone-king"
    assert orphan["agree"] is None
    assert "no live registry row" in orphan["reason"]


def test_a_manifest_crown_survives_its_project_having_no_rows_at_all(
    tmp_path: Path, monkeypatch
) -> None:
    """The vanished-row case this read exists for: the row is GONE, so no row
    names the project and no cwd can be derived. The sweep must key on the
    spaces root, not on rows, or the orphan crown is invisible exactly when
    the fleet lost it."""
    import fno.king.state as king_state
    from fno.agents.court import gather_court
    from fno.paths import space_dir

    _prepare(monkeypatch, tmp_path, [], graph_entries=[])
    manifest = space_dir(tmp_path) / "kings" / "x-dede.md"
    king_state.write_manifest(
        manifest,
        scope="x-dede",
        harness_session_id="gone-king",
        owner_cwd=str(tmp_path),
        crown_level=2,
        crown_scope="x-dede",
        crown_grantor="operator",
    )
    _stub_reign_reader(
        monkeypatch, tmp_path, _agreeing_reader("x-dede", "gone-king"),
        orphans=[{
            "scope": "x-dede", "level": 2, "grantor": "operator",
            "manifest_session": "gone-king", "manifest_path": str(manifest),
        }],
    )

    court = gather_court()

    orphan = next(e for e in court["crowns"] if e["scope"] == "x-dede")
    assert orphan["crown_source"] == "manifest"
    assert orphan["manifest_path"] == str(manifest)


def test_total_counts_row_crowns_only_so_the_census_keeps_its_arithmetic(
    tmp_path: Path, monkeypatch
) -> None:
    """doctor_lanes._census reads summary.total as a ROW count and computes
    workers = len(rows) - total; an orphan inside total subtracts a worker
    that still exists. manifest-only crowns count in their own field."""
    import fno.king.state as king_state
    from fno.agents.court import gather_court
    from fno.paths import space_dir

    _prepare(
        monkeypatch, tmp_path, [_entry("plain-worker", cwd=str(tmp_path), status="busy")]
    )
    manifest = space_dir(tmp_path) / "kings" / "x-dede.md"
    king_state.write_manifest(
        manifest, scope="x-dede", harness_session_id="gone-king",
        owner_cwd=str(tmp_path), crown_level=2, crown_scope="x-dede",
    )
    _stub_reign_reader(
        monkeypatch, tmp_path, _agreeing_reader("x-dede", "gone-king"),
        orphans=[{"scope": "x-dede", "level": 2, "manifest_session": "gone-king"}],
    )

    s = gather_court()["summary"]
    assert s["total"] == 0
    assert s["manifest_only"] == 1
    assert s["sweep_ran"] is True


def test_a_sweep_that_cannot_run_is_an_absence_never_zero_orphans(
    tmp_path: Path, monkeypatch
) -> None:
    """A stale binary without the court-orphans verb exits non-zero; reading
    that as [] would print a clean court. The ran marker must say it never ran."""
    from fno.agents.court import gather_court, render_court

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("king", cwd=str(tmp_path), status="busy", crown_level=1,
                crown_scope="alpha", crown_grantor="human")],
    )
    _stub_reign_reader(
        monkeypatch, tmp_path, _agreeing_reader("alpha", "king-session"),
        orphans=[{"scope": "x-dede", "level": 2}], sweep_fail=True,
    )

    court = gather_court()
    assert court["summary"]["sweep_ran"] is False
    assert court["summary"]["manifest_only"] == 0
    text = render_court(as_json=False)
    assert "orphan sweep did not run" in text


def test_a_half_crown_holds_its_territory_against_the_orphan_sweep(
    tmp_path: Path, monkeypatch
) -> None:
    """A scope-without-level row is a claim (the conflicts join counts it), so
    the sweep must see its scope as held; otherwise a manifest for it renders
    as an orphan beside the live row that holds it."""
    import fno.agents.court as court_mod
    from fno.agents.court import gather_court

    _prepare(monkeypatch, tmp_path, [])
    _stub_reign_reader(
        monkeypatch, tmp_path, _agreeing_reader("half", "half-session"),
        orphans=[{"scope": "half", "level": 1, "manifest_session": "half-session"}],
    )
    half = _entry("half-king", cwd=str(tmp_path), status="busy", crown_scope="half")

    def _none_reading(row):
        return None  # level stays None: the half-crown shape

    monkeypatch.setattr(court_mod, "crown_reading", _none_reading)
    gathered = gather_court([half])

    scopes = [e["scope"] for e in gathered["crowns"]]
    assert scopes.count("half") == 1  # the half-crown row, not row + orphan
    assert gathered["summary"]["manifest_only"] == 0


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


def test_a_scope_with_no_level_is_surfaced_not_silently_dropped(
    tmp_path: Path, monkeypatch
) -> None:
    """The mirror corruption: a row carries a scope but no level. crown_reading
    gates on crown_label, which registry.py returns None whenever crown_level
    is None regardless of crown_scope - so this row would otherwise vanish
    from the court entirely: not counted, not flagged unknown, not flagged
    disagreeing. That is the absence-lie this module exists to prevent."""
    from fno.agents.court import gather_court

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("half-scope", status="busy", crown_level=None, crown_scope="alpha")],
        graph_entries=[],
    )

    court = gather_court()

    assert court["summary"]["total"] == 1
    assert court["crowns"][0]["holder"] == "half-scope"
    assert court["crowns"][0]["agree"] is False
    assert "half a crown" in court["crowns"][0]["reason"]
    assert court["summary"]["disagreements"] == 1


def test_a_half_crown_still_counts_as_a_claim_on_its_territory(
    tmp_path: Path, monkeypatch
) -> None:
    """The two halves of one read must agree. gather_court surfaces a
    scope-without-level row as a disagreement, so _conflicts must not skip it:
    joining on crown_reading drops exactly those rows, and `conflicts` then
    comes back empty while two live rows claim alpha. A caller gating on
    conflicts would read "no territorial overlap" from a read that saw one."""
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
            _entry("half-scope", status="busy", crown_level=None, crown_scope="alpha"),
        ],
        graph_entries=[],
    )

    court = gather_court()

    assert court["conflicts"] == [
        {"scope": "alpha", "holders": ["king-a", "half-scope"]}
    ]


def test_a_non_string_scope_never_reaches_the_conflict_join(
    tmp_path: Path, monkeypatch
) -> None:
    """`fno agents court` promises to exit 0 on a read, so a corrupted row
    carrying a non-string crown_scope must degrade rather than raise."""
    from fno.agents.court import gather_court, render_court

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("bad-scope", status="busy", crown_level=None, crown_scope=5)],
        graph_entries=[],
    )

    assert gather_court()["conflicts"] == []
    assert "bad-scope" in render_court(as_json=False)


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


def test_crowning_an_adopted_row_never_makes_it_the_grantors_worker(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-HP (x-5283): adoption is vouching, not spawning. The adopted row
    keeps ``spawned_by_session`` null, records the grantor on
    ``adopted_by_session``, and the grantor's ``held`` is unchanged across
    the crown - on main the adoption stamped the grantor as spawner, so
    crowning moved the row's cost into the grantor's share."""
    from fno.agents import spawn_gate
    from fno.agents.registry import (
        load_registry,
        register_existing_session,
        write_registry,
    )

    grantor = "aaaaaaaa-1111-2222-3333-444455556666"
    adopted = "bbbbbbbb-1111-2222-3333-444455556666"
    for marker in (
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
    ):
        monkeypatch.delenv(marker, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", grantor)

    _prepare(monkeypatch, tmp_path, [])
    row = register_existing_session(
        session_id=adopted, cwd="/w", harness="claude", origin="adopted"
    )
    assert row.spawned_by_session is None
    assert row.adopted_by_session == grantor

    import os

    worker = _entry("w1", spawned_by_session=grantor, pid=os.getpid())
    uncrowned = [worker, load_registry()[-1]]
    write_registry(uncrowned)
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: uncrowned)
    before = spawn_gate.share_reading(spawn_gate.census(), 30, grantor)
    assert before["held"] == 1

    # The crown itself (the verb's field write): the adopted row becomes a
    # king; its spawner stays null and nobody's held moves.
    uncrowned[-1].crown_level = 1
    write_registry(uncrowned)
    after = spawn_gate.share_reading(spawn_gate.census(), 30, grantor)
    assert after["held"] == 1
    assert adopted in spawn_gate.census().crowned_sessions
