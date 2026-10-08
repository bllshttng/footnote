"""``fno agents team``: one read of every role and whether the graph agrees.

Agreement is a POSITIVE marker (AGENTS.md's positive-marker pitfall): a team
that shows no disagreements because it could not read anything must never
render the same as a healthy fleet. ``agree`` is ``None`` with a stated
reason on an unreadable graph - never ``True``, never ``False`` - and the
summary counts unknowns separately from disagreements.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

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
        seed_graph(graph_path, json.dumps({"entries": graph_entries}))


def test_a_role_over_a_real_live_epic_agrees(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=2,
                role_scope="e-1",
                role_grantor="human",
            )
        ],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )

    team = gather_team()

    assert team["roles"] == [
        {
            "holder": "lead",
            "level": 2,
            "scope": "e-1",
            "grantor": "human",
            "status": "busy",
            "agree": True,
            "reason": None,
            "manifest_path": None,
            "manifest_session": None,
            "role_source": "row",
        }
    ]
    s = team["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (1, 0, 0, 0)
    assert s["manifest_only"] == 0


def test_a_role_over_an_id_the_graph_does_not_hold_disagrees(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=2,
                role_scope="ghost-epic",
                role_grantor="human",
            )
        ],
        graph_entries=[],
    )

    team = gather_team()

    row = team["roles"][0]
    assert row["agree"] is False
    assert "ghost-epic" in row["reason"]
    s = team["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (1, 1, 0, 0)
    assert s["manifest_only"] == 0


def test_a_role_over_a_wrongly_typed_node_disagrees(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=2,
                role_scope="n-1",
                role_grantor="human",
            )
        ],
        graph_entries=[{"id": "n-1", "type": "feature", "project": "alpha"}],
    )

    team = gather_team()

    row = team["roles"][0]
    assert row["agree"] is False
    assert "not an epic" in row["reason"]


@pytest.mark.parametrize("status", TERMINAL_STATUSES)
def test_team_uses_every_canonical_plan_terminal_status(
    tmp_path: Path, monkeypatch, status: str
) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("lead", status="busy", role_level=2, role_scope="e-1")],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": status}],
    )

    row = gather_team()["roles"][0]

    assert row["agree"] is False
    assert status in row["reason"]


def test_unreadable_graph_answers_null_never_true_or_false(
    tmp_path: Path, monkeypatch
) -> None:
    """AC7-EDGE: every affected row reads agree=null with a stated reason, and
    the unknown count is tracked separately from disagreements."""
    from fno.agents.team import gather_team
    from fno.tracker import metadata

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=2,
                role_scope="e-1",
                role_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(
        metadata,
        "read_entries",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("unreadable")),
    )

    team = gather_team()

    row = team["roles"][0]
    assert row["agree"] is None
    assert row["reason"] is not None
    assert team["graph_readable"] is False
    s = team["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (1, 0, 1, 0)
    assert s["manifest_only"] == 0


def test_a_role_over_a_set_of_epics_agrees_only_if_every_member_is_a_live_epic(
    tmp_path: Path, monkeypatch
) -> None:
    """Rung 2 stores a set; agreement must check every member, not the first -
    one dead member makes the whole role disagree."""
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "mux-lead",
                status="busy",
                role_level=2,
                role_scope="e-1,e-2",
                role_grantor="human",
            )
        ],
        graph_entries=[
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"},
            {"id": "e-2", "type": "epic", "project": "beta", "status": "done"},
        ],
    )

    team = gather_team()

    row = team["roles"][0]
    assert row["agree"] is False
    assert "e-2" in row["reason"]
    assert "done" in row["reason"]


def test_a_role_over_two_live_epics_agrees(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "mux-lead",
                status="busy",
                role_level=2,
                role_scope="e-1,e-2",
                role_grantor="human",
            )
        ],
        graph_entries=[
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"},
            {"id": "e-2", "type": "epic", "project": "beta", "status": "ready"},
        ],
    )

    assert gather_team()["roles"][0]["agree"] is True


def test_a_portfolio_role_over_configured_projects_agrees(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=0,
                role_scope="alpha,beta",
                role_grantor="human",
            )
        ],
        graph_entries=[],
    )

    team = gather_team()

    assert team["roles"][0]["agree"] is True


def test_terminal_rows_are_excluded_from_the_team(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "dead-lead",
                status="exited",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            )
        ],
        graph_entries=[],
    )

    team = gather_team()

    assert team["roles"] == []
    s = team["summary"]
    assert (s["total"], s["disagreements"], s["unknowns"], s["splits"]) == (0, 0, 0, 0)
    assert s["manifest_only"] == 0


def test_two_live_rows_holding_the_same_territory_is_a_conflict(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead-a",
                status="busy",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            ),
            _entry(
                "lead-b",
                status="idle",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            ),
        ],
        graph_entries=[],
    )

    team = gather_team()

    assert team["conflicts"] == [{"scope": "alpha", "holders": ["lead-a", "lead-b"]}]
    # `agree` answers a different question than `conflicts`, so both rivals read
    # true here by design. `conflicts` is the only field that detects the rivalry.
    assert [c["agree"] for c in team["roles"]] == [True, True]
    assert team["summary"]["disagreements"] == 0


def test_aliases_and_ordered_scopes_share_one_conflict_group(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry("lead-a", status="busy", role_level=0, role_scope="alpha,beta"),
            _entry("lead-b", status="idle", role_level=0, role_scope="beta,a"),
        ],
        graph_entries=[],
    )

    assert gather_team()["conflicts"] == [
        {"scope": "alpha,beta", "holders": ["lead-a", "lead-b"]}
    ]


def test_a_set_holder_conflicts_with_a_holder_over_one_member(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """A rung-2 set-holder rules each member, so a live row over 'e-1,e-2'
    beside a live row over 'e-1' is a double rule. Keying conflicts on exact
    territory equality reported none of it."""
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "set-lead",
                status="busy",
                role_level=2,
                role_scope="e-1,e-2",
                role_grantor="human",
            ),
            _entry(
                "member-lead",
                status="idle",
                role_level=2,
                role_scope="e-1",
                role_grantor="human",
            ),
        ],
        graph_entries=[],
    )

    assert gather_team()["conflicts"] == [
        {"scope": "e-1", "holders": ["set-lead", "member-lead"]}
    ]


def test_rivalry_is_reported_per_pair_never_per_merged_group(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """A rivals B over e-1, B rivals C over e-2, A and C share nothing. A
    merged group would claim three rows hold e-1,e-2; the truth is two
    rivalries, each naming its own pair and the members that pair shares."""
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry("lead-a", status="busy", role_level=2, role_scope="e-1"),
            _entry("lead-b", status="busy", role_level=2, role_scope="e-1,e-2"),
            _entry("lead-c", status="busy", role_level=2, role_scope="e-2"),
        ],
        graph_entries=[],
    )

    assert gather_team()["conflicts"] == [
        {"scope": "e-1", "holders": ["lead-a", "lead-b"]},
        {"scope": "e-2", "holders": ["lead-b", "lead-c"]},
    ]


def test_a_portfolio_and_its_project_lead_are_a_team_not_a_conflict(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """The ladder's documented shape: a portfolio lead's team IS project
    leads. Overlap-keyed conflicts would cry double-rule on every legitimate
    team, so rivalry is rung-aware - same rung overlaps, cross rung only
    names the same territory outright."""
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry("portfolio-lead", status="busy", role_level=0, role_scope="alpha,beta"),
            _entry("project-lead", status="idle", role_level=1, role_scope="alpha"),
        ],
        graph_entries=[],
    )

    assert gather_team()["conflicts"] == []


def test_render_team_json_matches_gather_team(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.team import gather_team, render_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=2,
                role_scope="e-1",
                role_grantor="human",
            )
        ],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )

    rendered = json.loads(render_team(as_json=True))
    # The render is the gather plus the reads only a render pays for: the
    # spawn gate, the session-liveness judgement, and the stuck verdict
    # computed from them. `gather_team` stays cheap for its three other
    # callers. Popping them by name means a fourth key added silently still
    # fails here.
    assert rendered.pop("gate")["verdict"]
    assert rendered.pop("sessions_readable") is True
    assert "blind" in rendered["summary"].pop("stuck")
    # The line's CONTENT depends on whether a native binary is installed to
    # fold with, so this test pins its presence and test_team_stuck.py pins
    # what it says. `pop` raises when the key is missing, which is the check.
    rendered["summary"].pop("stuck_line")
    assert rendered == gather_team()


def test_render_team_table_names_scope_holder_and_agreement(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.team import render_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            )
        ],
        graph_entries=[],
    )

    text = render_team(as_json=False)

    assert "alpha" in text
    assert "lead" in text
    assert "team: 1 role, 0 disagreements, 0 unknowns" in text


# --- the manifest limb (x-f0d2): manifest is the durable record, row the cache


def _door_passthrough() -> str:
    """Script prelude: hand the registry door to the real binary."""
    from fno import rust_binary

    real = str(rust_binary.find_dev_binary() or rust_binary.resolve_binary())
    return (
        "import os\n"
        "if 'registry-commit' in sys.argv:\n"
        f"    os.execv({real!r}, [{real!r}, *sys.argv[1:]])\n"
    )


def _stub_term_reader(monkeypatch, tmp_path: Path, payload: dict, orphans=None, sweep_fail=False) -> None:
    """Answer lead-state with a canned payload and org-vacancies with a
    canned array (test_role_team pins the RENDER, not the reader;
    lead_state.rs pins the comparison and the sweep). The sweep answer honors
    --held like the real binary (a held scope is filtered out) and can be
    made to fail, pinning the ran-marker."""
    import stat

    script = tmp_path / "fno-agents"
    body = (
        "#!/usr/bin/env python3\n"
        "import json, sys\n"
        + _door_passthrough()
        + f"TERM = {json.dumps(json.dumps(payload))}\n"
        f"FAIL = {repr(bool(sweep_fail))}\n"
        f"ORPHANS = {json.dumps(orphans or [])}\n"
        "if 'org-vacancies' not in sys.argv:\n"
        "    print(TERM, end='')\n"
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
        "promoted": True,
        "scope": scope,
        "shape": "pass",
        "manifest_session": session,
        "registry_session": session,
        "live": True,
        "split": False,
        "unknown_reason": None,
    }


def test_team_names_the_manifest_and_role_source_per_scope(
    tmp_path: Path, monkeypatch
) -> None:
    """A promoted row with a matching promoted manifest renders source `both`."""
    import fno.lead.state as lead_state
    from fno.agents.team import gather_team
    from fno.paths import space_dir

    row = _entry(
        "lead",
        cwd=str(tmp_path),
        status="busy",
        role_level=2,
        role_scope="e-1",
        role_grantor="human",
    )
    _prepare(
        monkeypatch,
        tmp_path,
        [row],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    manifest = space_dir(tmp_path) / "leads" / "e-1.md"
    lead_state.write_manifest(
        manifest,
        scope="e-1",
        harness_session_id="lead-session",
        owner_cwd=str(tmp_path),
        role_level=2,
        role_scope="e-1",
        role_grantor="human",
    )
    payload = _agreeing_reader("e-1", "lead-session")
    payload.update(role_on_manifest=True, manifest_path=str(manifest))
    _stub_term_reader(monkeypatch, tmp_path, payload)

    team = gather_team()

    entry = team["roles"][0]
    assert entry["role_source"] == "both"
    assert entry["manifest_session"] == "lead-session"
    assert entry["manifest_path"] == str(manifest)
    assert team["summary"]["splits"] == 0


def test_a_split_role_counts_apart_from_disagreements_and_unknowns(
    tmp_path: Path, monkeypatch
) -> None:
    """Two stores naming different holders is a SPLIT, not a graph
    disagreement and not an unknown: the summary counts it separately."""
    from fno.agents.team import gather_team
    from fno.paths import space_dir
    import fno.lead.state as lead_state

    row = _entry(
        "lead",
        cwd=str(tmp_path),
        status="busy",
        role_level=2,
        role_scope="e-1",
        role_grantor="human",
    )
    _prepare(
        monkeypatch,
        tmp_path,
        [row],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    lead_state.write_manifest(
        space_dir(tmp_path) / "leads" / "e-1.md",
        scope="e-1",
        harness_session_id="someone-else",
        owner_cwd=str(tmp_path),
        role_level=2,
        role_scope="e-1",
        role_grantor="human",
    )
    payload = _agreeing_reader("e-1", "lead-session")
    payload.update(manifest_session="someone-else", split=True)
    _stub_term_reader(monkeypatch, tmp_path, payload)

    team = gather_team()

    entry = team["roles"][0]
    assert entry["role_source"] == "split"
    # The graph limb still answers its own question; the split is not folded in.
    assert entry["agree"] is True
    assert team["summary"]["disagreements"] == 0
    assert team["summary"]["unknowns"] == 0
    assert team["summary"]["splits"] == 1


def test_a_role_on_only_the_manifest_is_surfaced_unassigned_in_the_registry(
    tmp_path: Path, monkeypatch
) -> None:
    """A scope whose row vanished keeps its role on the manifest; team must
    show it with the manifest named, not as an empty team."""
    import fno.lead.state as lead_state
    from fno.agents.team import gather_team
    from fno.paths import space_dir

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("plain-worker", cwd=str(tmp_path), status="busy")],
        graph_entries=[],
    )
    manifest = space_dir(tmp_path) / "leads" / "x-dede.md"
    lead_state.write_manifest(
        manifest,
        scope="x-dede",
        harness_session_id="gone-lead",
        owner_cwd=str(tmp_path),
        role_level=2,
        role_scope="x-dede",
        role_grantor="operator",
    )
    _stub_term_reader(
        monkeypatch, tmp_path, _agreeing_reader("x-dede", "gone-lead"),
        orphans=[{
            "scope": "x-dede", "level": 2, "grantor": "operator",
            "manifest_session": "gone-lead", "manifest_path": str(manifest),
        }],
    )

    team = gather_team()

    orphan = next(e for e in team["roles"] if e["scope"] == "x-dede")
    assert orphan["role_source"] == "manifest"
    assert orphan["manifest_path"] == str(manifest)
    assert orphan["manifest_session"] == "gone-lead"
    assert orphan["agree"] is None
    assert "no live registry row" in orphan["reason"]


def test_a_manifest_role_survives_its_project_having_no_rows_at_all(
    tmp_path: Path, monkeypatch
) -> None:
    """The vanished-row case this read exists for: the row is GONE, so no row
    names the project and no cwd can be derived. The sweep must key on the
    spaces root, not on rows, or the orphan role is invisible exactly when
    the fleet lost it."""
    import fno.lead.state as lead_state
    from fno.agents.team import gather_team
    from fno.paths import space_dir

    _prepare(monkeypatch, tmp_path, [], graph_entries=[])
    manifest = space_dir(tmp_path) / "leads" / "x-dede.md"
    lead_state.write_manifest(
        manifest,
        scope="x-dede",
        harness_session_id="gone-lead",
        owner_cwd=str(tmp_path),
        role_level=2,
        role_scope="x-dede",
        role_grantor="operator",
    )
    _stub_term_reader(
        monkeypatch, tmp_path, _agreeing_reader("x-dede", "gone-lead"),
        orphans=[{
            "scope": "x-dede", "level": 2, "grantor": "operator",
            "manifest_session": "gone-lead", "manifest_path": str(manifest),
        }],
    )

    team = gather_team()

    orphan = next(e for e in team["roles"] if e["scope"] == "x-dede")
    assert orphan["role_source"] == "manifest"
    assert orphan["manifest_path"] == str(manifest)


def test_total_counts_row_roles_only_so_the_census_keeps_its_arithmetic(
    tmp_path: Path, monkeypatch
) -> None:
    """doctor_lanes._census reads summary.total as a ROW count and computes
    workers = len(rows) - total; an orphan inside total subtracts a worker
    that still exists. manifest-only roles count in their own field."""
    import fno.lead.state as lead_state
    from fno.agents.team import gather_team
    from fno.paths import space_dir

    _prepare(
        monkeypatch, tmp_path, [_entry("plain-worker", cwd=str(tmp_path), status="busy")]
    )
    manifest = space_dir(tmp_path) / "leads" / "x-dede.md"
    lead_state.write_manifest(
        manifest, scope="x-dede", harness_session_id="gone-lead",
        owner_cwd=str(tmp_path), role_level=2, role_scope="x-dede",
    )
    _stub_term_reader(
        monkeypatch, tmp_path, _agreeing_reader("x-dede", "gone-lead"),
        orphans=[{"scope": "x-dede", "level": 2, "manifest_session": "gone-lead"}],
    )

    s = gather_team()["summary"]
    assert s["total"] == 0
    assert s["manifest_only"] == 1
    assert s["sweep_ran"] is True


def test_a_sweep_that_cannot_run_is_an_absence_never_zero_orphans(
    tmp_path: Path, monkeypatch
) -> None:
    """A stale binary without the org-vacancies verb exits non-zero; reading
    that as [] would print a clean team. The ran marker must say it never ran."""
    from fno.agents.team import gather_team, render_team

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("lead", cwd=str(tmp_path), status="busy", role_level=1,
                role_scope="alpha", role_grantor="human")],
    )
    _stub_term_reader(
        monkeypatch, tmp_path, _agreeing_reader("alpha", "lead-session"),
        orphans=[{"scope": "x-dede", "level": 2}], sweep_fail=True,
    )

    team = gather_team()
    assert team["summary"]["sweep_ran"] is False
    assert team["summary"]["manifest_only"] == 0
    text = render_team(as_json=False)
    assert "orphan sweep did not run" in text


def test_a_half_role_holds_its_territory_against_the_orphan_sweep(
    tmp_path: Path, monkeypatch
) -> None:
    """A scope-without-level row is a claim (the conflicts join counts it), so
    the sweep must see its scope as held; otherwise a manifest for it renders
    as an orphan beside the live row that holds it."""
    import fno.agents.team as team_mod
    from fno.agents.team import gather_team

    _prepare(monkeypatch, tmp_path, [])
    _stub_term_reader(
        monkeypatch, tmp_path, _agreeing_reader("half", "half-session"),
        orphans=[{"scope": "half", "level": 1, "manifest_session": "half-session"}],
    )
    half = _entry("half-lead", cwd=str(tmp_path), status="busy", role_scope="half")

    def _none_reading(row):
        return None  # level stays None: the half-role shape

    monkeypatch.setattr(team_mod, "role_reading", _none_reading)
    gathered = gather_team([half])

    scopes = [e["scope"] for e in gathered["roles"]]
    assert scopes.count("half") == 1  # the half-role row, not row + orphan
    assert gathered["summary"]["manifest_only"] == 0


def test_unreadable_registry_nulls_the_team_rather_than_reporting_it_empty(
    tmp_path: Path, monkeypatch
) -> None:
    """The absence-lie one layer below the agreement verdict. Degrading a failed
    registry read to [] would print a healthy, empty team, so a caller gating
    on summary.disagreements == 0 passes on a read that saw nothing."""
    from fno.agents import team as team_mod
    from fno.agents.team import gather_team, render_team

    _prepare(monkeypatch, tmp_path, [], graph_entries=[])
    monkeypatch.setattr(
        team_mod,
        "load_registry",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("truncated")),
        raising=False,
    )
    monkeypatch.setattr(
        "fno.agents.registry.load_registry",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("truncated")),
    )

    team = gather_team()

    assert team["registry_readable"] is False
    assert team["roles"] is None
    # Every count is null, so no naive zero-gate can read this as healthy.
    assert team["summary"]["total"] is None
    assert team["summary"]["disagreements"] is None
    assert team["summary"]["unknowns"] is None
    text = render_team(as_json=False)
    assert "CANNOT READ" in text
    assert "nothing was checked" in text


def test_a_half_role_renders_and_never_certifies_agreement(
    tmp_path: Path, monkeypatch
) -> None:
    """A row with a level but no scope rules no territory. The team is the read
    meant to SURFACE that corruption, so it must not crash on it (formatting a
    null scope to a width raises) and must not certify it as agreeing."""
    from fno.agents.team import gather_team, render_team

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("half", status="busy", role_level=1, role_scope=None)],
        graph_entries=[],
    )

    team = gather_team()

    assert team["roles"][0]["agree"] is False
    assert "half a role" in team["roles"][0]["reason"]
    assert team["summary"]["disagreements"] == 1
    # The table renders rather than raising TypeError on the null scope.
    assert "half" in render_team(as_json=False)


def test_a_scope_with_no_level_is_surfaced_not_silently_dropped(
    tmp_path: Path, monkeypatch
) -> None:
    """The mirror corruption: a row carries a scope but no level. role_reading
    gates on role_label, which registry.py returns None whenever role_level
    is None regardless of role_scope - so this row would otherwise vanish
    from the team entirely: not counted, not flagged unknown, not flagged
    disagreeing. That is the absence-lie this module exists to prevent."""
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("half-scope", status="busy", role_level=None, role_scope="alpha")],
        graph_entries=[],
    )

    team = gather_team()

    assert team["summary"]["total"] == 1
    assert team["roles"][0]["holder"] == "half-scope"
    assert team["roles"][0]["agree"] is False
    assert "half a role" in team["roles"][0]["reason"]
    assert team["summary"]["disagreements"] == 1


def test_a_half_role_still_counts_as_a_claim_on_its_territory(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """The two halves of one read must agree. gather_team surfaces a
    scope-without-level row as a disagreement, so _conflicts must not skip it:
    joining on role_reading drops exactly those rows, and `conflicts` then
    comes back empty while two live rows claim alpha. A caller gating on
    conflicts would read "no territorial overlap" from a read that saw one."""
    from fno.agents.team import gather_team

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead-a",
                status="busy",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            ),
            _entry("half-scope", status="busy", role_level=None, role_scope="alpha"),
        ],
        graph_entries=[],
    )

    team = gather_team()

    assert team["conflicts"] == [
        {"scope": "alpha", "holders": ["lead-a", "half-scope"]}
    ]


def test_a_non_string_scope_never_reaches_the_conflict_join(
    tmp_path: Path, monkeypatch
) -> None:
    """`fno agents team` promises to exit 0 on a read, so a corrupted row
    carrying a non-string role_scope must degrade rather than raise. The
    table refuses to store such a row, so the reader hands it in directly."""
    import fno.agents.registry as registry_mod
    from fno.agents.team import gather_team, render_team

    _prepare(monkeypatch, tmp_path, [], graph_entries=[])
    bad = _entry("bad-scope", status="busy", role_level=None, role_scope=5)
    monkeypatch.setattr(registry_mod, "load_registry", lambda *a, **k: [bad])

    assert gather_team()["conflicts"] == []
    assert "bad-scope" in render_team(as_json=False)


def test_project_rungs_stay_determinate_when_the_graph_is_unreadable(
    tmp_path: Path, monkeypatch
) -> None:
    """Only the epic rung consults the graph. A project or portfolio role
    resolves entirely from config, so an external tracker backend must not
    blank out an answer that is fully determinate."""
    from fno.agents.team import gather_team
    from fno.tracker import metadata

    _prepare(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "lead",
                status="busy",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(
        metadata,
        "read_entries",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("external backend")),
    )

    team = gather_team()

    assert team["roles"][0]["agree"] is True
    assert team["summary"]["unknowns"] == 0


def test_no_live_roles_renders_a_plain_statement(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.team import render_team

    _prepare(monkeypatch, tmp_path, [], graph_entries=[])

    assert render_team(as_json=False) == "team: no live roles"


def test_promoting_an_adopted_row_never_makes_it_the_grantors_worker(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-HP (x-5283): adoption is vouching, not spawning. The adopted row
    keeps ``spawned_by_session`` null, records the grantor on
    ``adopted_by_session``, and the grantor's ``held`` is unchanged across
    the role - on main the adoption stamped the grantor as spawner, so
    promoting moved the row's cost into the grantor's share."""
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
    unpromoted = [worker, load_registry()[-1]]
    write_registry(unpromoted)
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: unpromoted)

    # The grantor's `held` now reads from the gate probe's share block; the
    # worker-row buckets it sums are the census's, read directly here.
    def _held() -> int:
        return len(spawn_gate.census().worker_rows.get(grantor, []))

    before = _held()
    assert before == 1

    # The role itself (the verb's field write): the adopted row becomes a
    # lead; its spawner stays null and nobody's held moves.
    unpromoted[-1].role_level = 1
    write_registry(unpromoted)
    after = _held()
    assert after == 1
    assert adopted in spawn_gate.census().promoted_sessions


# --- the scope fold: the native fold, relayed onto the role rows


def _stub_team_fold(monkeypatch, tmp_path: Path, scope_nodes: dict, fail: bool = False) -> None:
    """A stub fno-agents binary that answers `org-fold` from a canned
    scope_nodes map and org-vacancies from an empty list (the fold tests pin
    the RELAY, not the fold computation; team_fold.rs pins that)."""
    import stat

    script = tmp_path / "fno-agents"
    body = (
        "#!/usr/bin/env python3\n"
        "import json, sys\n"
        + _door_passthrough()
        + f"NODES = {scope_nodes!r}\n"
        f"FAIL = {repr(bool(fail))}\n"
        "TERM = json.dumps({'promoted': True, 'scope': 'x', 'shape': 'pass',\n"
        "    'manifest_session': 's', 'registry_session': 's', 'live': True,\n"
        "    'split': False, 'unknown_reason': None, 'role_on_manifest': False})\n"
        "if 'org-fold' in sys.argv:\n"
        "    if FAIL:\n"
        "        sys.exit(3)\n"
        "    print(json.dumps({'scope_nodes': NODES}), end='')\n"
        "    sys.exit(0)\n"
        "if 'org-vacancies' in sys.argv:\n"
        "    print('[]', end='')\n"
        "    sys.exit(0)\n"
        "print(TERM, end='')\n"
    )
    script.write_text(body, encoding="utf-8")
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: script)


def test_the_fold_lands_on_the_role_rows(tmp_path: Path, monkeypatch) -> None:
    """The fold answer keys on scope and lands on every role that names it;
    a role the fold answered nothing for reads unresolved, never missing."""
    from fno.agents.team import fold_scope_nodes

    _stub_team_fold(
        monkeypatch,
        tmp_path,
        {
            "e-1": {
                "status": "ok",
                "total": 2,
                "counts": {"in_progress": 1, "ready": 1},
                "nodes": [{"id": "x-1", "slug": "", "status": "in_progress",
                           "worker": "w1", "pr_number": 3, "sessions": ["s1"]}],
                "omitted": 0,
            }
        },
    )
    roles = [
        {"holder": "lead", "level": 2, "scope": "e-1"},
        {"holder": "gone-lead", "level": 2, "scope": "x-dede", "status": "manifest-only"},
    ]

    fold_scope_nodes(roles)

    assert roles[0]["scope_nodes"]["status"] == "ok"
    assert roles[0]["scope_nodes"]["nodes"][0]["pr_number"] == 3
    assert roles[1]["scope_nodes"]["status"] == "unresolved"


def test_a_half_role_resolves_locally_without_the_binary(
    tmp_path: Path, monkeypatch
) -> None:
    """A scope-less or level-less role rules no territory; the fold says so
    per row without paying a subprocess."""
    from fno.agents.team import fold_scope_nodes

    _stub_team_fold(monkeypatch, tmp_path, {}, fail=True)
    roles = [
        {"holder": "half", "level": None, "scope": "alpha"},
        {"holder": "scopeless", "level": 1, "scope": ""},
    ]

    fold_scope_nodes(roles)

    for role in roles:
        assert role["scope_nodes"]["status"] == "unresolved"
        assert "no scope or no role level" in role["scope_nodes"]["reason"]


def test_a_failed_fold_marks_roles_unresolved_never_empty(
    tmp_path: Path, monkeypatch
) -> None:
    """A fold that cannot run (stale binary, unreadable graph) is stated per
    role - the rows never render as empty scopes."""
    from fno.agents.team import fold_scope_nodes

    _stub_team_fold(monkeypatch, tmp_path, {}, fail=True)
    roles = [{"holder": "lead", "level": 2, "scope": "e-1"}]

    fold_scope_nodes(roles)

    sn = roles[0]["scope_nodes"]
    assert sn["status"] == "unresolved"
    assert "could not run" in sn["reason"]


def test_a_missing_binary_marks_roles_unresolved(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.team import fold_scope_nodes

    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)
    roles = [{"holder": "lead", "level": 2, "scope": "e-1"}]

    fold_scope_nodes(roles)

    assert roles[0]["scope_nodes"]["status"] == "unresolved"


def test_empty_team_folds_to_nothing(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.team import fold_scope_nodes

    _stub_team_fold(monkeypatch, tmp_path, {}, fail=True)
    fold_scope_nodes([])  # no roles, no subprocess, no raise


def test_nodes_flag_folds_json_and_moves_no_existing_key(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-JSON: -n -J adds scope_nodes per role and nothing else moves, so a
    caller gating on summary or conflicts is unaffected."""
    from fno.agents.team import render_team

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("lead", status="busy", role_level=2, role_scope="e-1")],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    _stub_team_fold(
        monkeypatch,
        tmp_path,
        {
            "e-1": {
                "status": "ok", "total": 1, "counts": {"ready": 1},
                "nodes": [{"id": "e-1", "slug": "", "status": "ready",
                           "worker": None, "pr_number": 7, "sessions": []}],
                "omitted": 0,
            }
        },
    )

    before = json.loads(render_team(as_json=True))
    after = json.loads(render_team(as_json=True, nodes=True))

    assert after["summary"] == before["summary"]
    assert after["conflicts"] == before["conflicts"]
    assert len(after["roles"]) == len(before["roles"])
    for plain, folded in zip(before["roles"], after["roles"]):
        plain_view = {k: v for k, v in folded.items() if k != "scope_nodes"}
        assert plain_view == plain
    sn = after["roles"][0]["scope_nodes"]
    assert sn["status"] == "ok"
    assert [r["pr_number"] for r in sn["nodes"]] == [7]


def test_nodes_flag_answers_json_and_the_plain_table_is_untouched(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-COMPAT: without the flag the table renders exactly as before; with
    it the answer is JSON (the fold's row data is tabular and the board's
    team section is its human view)."""
    from fno.agents.team import render_team

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("lead", status="busy", role_level=2, role_scope="e-1")],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    _stub_team_fold(
        monkeypatch,
        tmp_path,
        {
            "e-1": {
                "status": "ok", "total": 1, "counts": {"ready": 1},
                "nodes": [{"id": "e-1", "slug": "", "status": "ready",
                           "worker": "tgt-e1", "pr_number": 7, "sessions": ["s-e1"]}],
                "omitted": 0,
            }
        },
    )

    plain = render_team(as_json=False)
    assert "scope_nodes" not in plain

    folded = json.loads(render_team(as_json=False, nodes=True))
    sn = folded["roles"][0]["scope_nodes"]
    assert sn["status"] == "ok"
    assert sn["nodes"][0]["worker"] == "tgt-e1"
    assert sn["nodes"][0]["pr_number"] == 7


def test_a_failed_fold_names_the_role_not_an_empty_scope(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-READFAIL: a fold that cannot run says so on the role's own row -
    never roles rendered as empty scopes."""
    from fno.agents.team import render_team

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("lead", status="busy", role_level=2, role_scope="e-1")],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    _stub_team_fold(monkeypatch, tmp_path, {}, fail=True)

    text = render_team(as_json=False, nodes=True)

    payload = json.loads(text)
    assert all(
        c["scope_nodes"]["status"] == "unresolved" for c in payload["roles"]
    )
    assert "could not run" in payload["roles"][0]["scope_nodes"]["reason"]


def test_the_flag_pays_no_extra_python_graph_read(tmp_path: Path, monkeypatch) -> None:
    """AC5-PERF (the part a unit test pins): the fold rides the native binary,
    so -n performs the same single python graph read the plain render does."""
    from fno.agents.team import render_team
    from fno.tracker import metadata

    _prepare(
        monkeypatch,
        tmp_path,
        [_entry("lead", status="busy", role_level=2, role_scope="e-1")],
        graph_entries=[{"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"}],
    )
    _stub_team_fold(
        monkeypatch,
        tmp_path,
        {"e-1": {"status": "ok", "total": 1, "counts": {"ready": 1},
                 "nodes": [], "omitted": 1}},
    )
    calls: list = []
    real = metadata.read_entries

    def counting(*a, **k):
        calls.append(a)
        return real(*a, **k)

    monkeypatch.setattr(metadata, "read_entries", counting)

    render_team(as_json=False)
    assert len(calls) == 1
    render_team(as_json=False, nodes=True)
    assert len(calls) == 2


# ---------------------------------------------------------------------------
# the wake's agreement-free team (x-5f26)
# ---------------------------------------------------------------------------


def test_an_agreement_free_team_skips_the_graph_and_reads_the_same_roles(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP: agree=False never touches the graph; holder, level and scope
    match the agreeing read, and the rung-2 role reads agree None."""
    from fno.agents import team as team_mod
    from fno.agents.team import gather_team

    rows = [
        _entry(
            "lead-a",
            status="busy",
            role_level=2,
            role_scope="e-1,e-2",
            role_grantor="human",
        ),
        _entry(
            "lead-b",
            status="busy",
            role_level=1,
            role_scope="alpha",
            role_grantor="human",
        ),
    ]
    _prepare(
        monkeypatch,
        tmp_path,
        rows,
        graph_entries=[
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "ready"},
            {"id": "e-2", "type": "epic", "project": "alpha", "status": "ready"},
        ],
    )

    def _refuse():
        raise AssertionError("an agreement-free team must not read the graph")

    real_index = team_mod._graph_index
    monkeypatch.setattr(team_mod, "_graph_index", _refuse)
    free = gather_team(rows, agree=False)
    monkeypatch.setattr(team_mod, "_graph_index", real_index)
    full = gather_team(rows)

    by_free = {c["holder"]: c for c in free["roles"]}
    by_full = {c["holder"]: c for c in full["roles"]}
    for holder in ("lead-a", "lead-b"):
        assert (by_free[holder]["level"], by_free[holder]["scope"]) == (
            by_full[holder]["level"],
            by_full[holder]["scope"],
        )
    assert by_free["lead-a"]["agree"] is None
    assert by_full["lead-a"]["agree"] is True
    assert by_free["lead-b"]["agree"] is True


def test_an_agreement_free_team_still_nulls_on_an_unreadable_registry(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-ERR: agree=False changes nothing about the unreadable-registry
    posture - roles stays None, never an empty team."""
    from fno.agents import registry as registry_mod
    from fno.agents.team import gather_team

    _prepare(monkeypatch, tmp_path, [])

    def _boom():
        raise OSError("locked")

    monkeypatch.setattr(registry_mod, "load_registry", _boom)
    team = gather_team(agree=False)
    assert team["roles"] is None
    assert team["registry_readable"] is False


