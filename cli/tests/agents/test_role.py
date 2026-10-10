"""US9 (KFAD squad team): orchestrator role visibility.

A role is stamped on the spawned worker's registry row by the SPAWN path
(grantor derived from the spawning session, never self-declared), survives a
round-trip, and surfaces in `fno whoami` and `fno agents list`.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
import subprocess
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, replace
from pathlib import Path
from typing import Optional

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


# --- the stored scope encoding ----------------------------------------------
#
# There is no `--promote` spec parser any more. The flag takes scopes directly and
# the rung is derived (see test_role_level_derivation), so what remains to pin
# here is the ENCODING: a portfolio has to reduce to one canonical string,
# because the one-live-role guard compares scopes by equality and would
# otherwise miss a duplicate spelled in a different order.


def test_a_portfolio_canonicalizes_regardless_of_spelling() -> None:
    from fno.agents.role import canonical_scope

    assert canonical_scope(["web", "etl"]) == canonical_scope(["etl", "web"])
    assert canonical_scope(["etl", "web", "etl"]) == "etl,web"
    assert canonical_scope([" etl ", "web"]) == "etl,web"


def test_split_is_the_inverse_of_canonical() -> None:
    from fno.agents.role import canonical_scope, split_scope

    assert split_scope(canonical_scope(["web", "etl"])) == ["etl", "web"]
    assert split_scope("epic-x") == ["epic-x"]
    assert split_scope(None) == []


def test_split_scope_degrades_on_a_non_string_rather_than_raising() -> None:
    """A corrupted registry row can carry a non-string role_scope (a stray
    int from a hand-edit). Every caller here, including `fno agents team`
    (which promises to exit 0 on a read), must not crash on it."""
    from fno.agents.role import split_scope

    assert split_scope(5) == []  # type: ignore[arg-type]


def test_scope_contains_canonicalizes_an_alias_project(monkeypatch, tmp_path) -> None:
    """scope_contains must canonicalize the graph node's project field before
    comparing it to the canonicalized role scope. Graph intake stores the
    project field RAW (the short_name alias a node was filed under), so without
    canonicalization a lead over 'alpha' is falsely refused an epic filed as
    'a' - a legitimate delegation blocked."""
    import fno.projects.resolve as proj_resolve

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\nprojects = [{ name = "alpha", short_name = "a" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()

    from fno.agents import role

    # An epic filed under the alias 'a' (the raw spelling intake stores).
    # The containment resolves the index once, so the stub sits there.
    monkeypatch.setattr(
        role,
        "_graph_index",
        lambda: {"epic-1": {"id": "epic-1", "type": "epic", "project": "a"}},
    )
    assert role.scope_contains("alpha", "epic-1") is True

    # A genuinely different project is still not contained.
    monkeypatch.setattr(
        role,
        "_graph_index",
        lambda: {"epic-1": {"id": "epic-1", "type": "epic", "project": "beta"}},
    )
    assert role.scope_contains("alpha", "epic-1") is False


def test_a_mislabeled_level_cannot_switch_off_rivalry(monkeypatch, tmp_path) -> None:
    """The rung is a fact about the SCOPE, not the stored number: a row stamped
    level 0 over an epic set must not read as a different rung and slip the
    cross-rung exemption while a rung-2 role takes one member."""
    import fno.agents.role as role_mod
    from fno.agents.role import _role_rivals

    _prepare_role_cli(monkeypatch, tmp_path, [])
    monkeypatch.setattr(
        role_mod,
        "_graph_index",
        lambda: {
            nid: {"id": nid, "type": "epic", "project": "alpha"}
            for nid in ("e-1", "e-2")
        },
    )

    # Stored (0, "e-1,e-2") vs a real rung-2 role over one member: derived
    # rungs both 2, so overlap decides. Trusting stored levels answered False.
    assert _role_rivals("e-1,e-2", 0, "e-1", 2) is True
    # A mislabeled portfolio (stored 2 over two projects) still teams its
    # project lead: derivation reads 0 vs 1, equality decides, not rivals.
    assert _role_rivals("alpha,beta", 2, "alpha", 1) is False


# --- spawn stamps the role, grantor is provenance not self-declared ---------


def _space_manifest(repo: Path, scope: str) -> Path:
    """Manifest path as the product resolves it for a spawn whose cwd is repo.

    The role writes key the space on the SPAWN cwd's canonical root, so a
    test sandbox (a non-git tmp dir) hashes its own path; read the manifest
    through the same resolution instead of a hand-built checkout path.
    """
    from fno.paths import space_dir

    return space_dir(repo) / "leads" / f"{scope}.md"


class _FakeRunner:
    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        if argv[1:4] == ["mux", "pane", "run"]:
            return subprocess.CompletedProcess(argv, 0, "7\n", "")
        if argv[1:4] == ["mux", "pane", "ls"]:
            out = json.dumps(
                [{"pane_id": 7, "squad_id": 1, "tab_id": 1, "cwd": "/w", "child_pid": 4242}]
            )
            return subprocess.CompletedProcess(argv, 0, out, "")
        if argv[1:4] == ["mux", "pane", "wait"]:
            return subprocess.CompletedProcess(argv, 11, "", "")
        if argv[1:4] == ["mux", "pane", "read"]:
            return subprocess.CompletedProcess(argv, 0, "", "")
        raise AssertionError(f"unexpected invocation: {argv}")


def _spawn_promoted(monkeypatch, tmp_path, *, grantor_env: Optional[str], **role):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    for var in ("CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    if grantor_env is None:
        monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    else:
        monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", grantor_env)

    from fno.agents.mux_spawn import dispatch_spawn_pane

    return dispatch_spawn_pane(
        name="Avery",
        message="term",
        provider="claude",
        cwd=tmp_path,
        runner=_FakeRunner(),
        **role,
    )


def test_role_stamped_grantor_is_the_spawning_session(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    from fno.agents.registry import AgentEntry, load_registry, update_registry
    import fno.lead.state as lead_state

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)
    # Seat the grantor as a registered lead over epic-x; the spawn is then a
    # succession. An agent identity with no registry row is now refused at the
    # grantor check, so the agent must be seated - the corrected opposite of the
    # fail-open these tests rode. Reuse _spawn_promoted so the provider axis
    # binding stays on its baselined line rather than adding a new one inline.
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="parent",
                cwd="/w",
                log_path="",
                harness="claude",
                harness_session_id="parent-sess-abc",
                short_id="parent",
                status="busy",
                role_level=1,
                role_scope="epic-x",
                role_grantor="human",
            )
        ]
    )
    _spawn_promoted(
        monkeypatch, tmp_path,
        grantor_env="parent-sess-abc",
        role_level=1, role_scope="epic-x", succession=True,
    )
    successor = next(e for e in load_registry() if e.name == "Avery")
    assert successor.role_level == 1
    assert successor.role_scope == "epic-x"
    # Provenance, not self-declared: the grantor is who actually spawned it.
    assert successor.role_grantor == "parent-sess-abc"
    assert successor.role_label == "L1 epic-x"
    manifest = _space_manifest(tmp_path, "epic-x")
    assert lead_state.parse_manifest(manifest)["harness_session_id"] == successor.harness_session_id


def test_role_grantor_defaults_to_human_for_a_direct_spawn(
    tmp_path: Path, monkeypatch, capsys, native_backlog_door
) -> None:
    from fno.agents.registry import load_registry

    _spawn_promoted(
        monkeypatch, tmp_path,
        grantor_env=None,  # no parent session env == a human's own shell
        role_level=0, role_scope="proj-a",
    )
    row = load_registry()[0]
    assert row.role_grantor == "human"
    assert row.role_level == 0
    assert "lead loop disabled" in capsys.readouterr().err


def test_pane_spawn_clears_a_terminal_holder_before_reclaiming_its_scope(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    use_tmpdir(monkeypatch, tmp_path)
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="dead-lead",
                cwd="/w",
                log_path="",
                harness="claude",
                harness_session_id="dead-session",
                status="exited",
                role_level=1,
                role_scope="epic-x",
                role_grantor="human",
            )
        ]
    )

    _spawn_promoted(
        monkeypatch,
        tmp_path,
        grantor_env="dead-session",
        role_level=1,
        role_scope="epic-x",
    )

    dead = next(row for row in load_registry() if row.name == "dead-lead")
    assert (dead.role_level, dead.role_scope, dead.role_grantor) == (
        None,
        None,
        None,
    )
    assert [row.name for row in load_registry() if row.role_scope == "epic-x"] == [
        "Avery"
    ]
    # The reclaim is journaled from the committed write: holder_terminal for
    # the dead row, the grant for the new one.
    from fno import paths

    from tests._event_rows import event_rows

    events = event_rows(paths.state_dir() / "events.jsonl")
    vacates = [e for e in events if e["kind"] == "agent_role_vacated"]
    assert len(vacates) == 1
    assert vacates[0]["cause"] == "holder_terminal"
    assert vacates[0]["holder"] == "dead-lead"
    assert vacates[0]["holder_session"] == "dead-session"
    assert vacates[0]["scope"] == "epic-x"
    roles = [e for e in events if e["kind"] == "agent_promoted"]
    assert [c["name"] for c in roles] == ["Avery"]


def _role_row(name: str, *, status: str = "busy", scope="epic-x"):
    from fno.agents.registry import AgentEntry

    return AgentEntry(
        name=name,
        cwd="/w",
        log_path="",
        harness="claude",
        harness_session_id=f"{name}-sess",
        status=status,
        role_level=2 if scope else None,
        role_scope=scope,
        role_grantor="human" if scope else None,
    )


def test_settle_spawn_role_outcomes(tmp_path: Path, monkeypatch, native_backlog_door) -> None:
    """Exercise the Rust plan and apply stages against planted registry rows."""
    from dataclasses import replace

    from fno.agents.role import plan_spawn_role, settle_spawn_role
    from fno.agents.registry import update_registry
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)

    def plan_for(rows, succession=False):
        update_registry(lambda _: rows)
        refusal, plan = plan_spawn_role("epic-x", None, succession, proposed_name="Avery" if succession else None)
        assert refusal is None
        assert plan is not None
        return plan

    unpromoted = _role_row("a", scope=None)
    granted_plan = plan_for([unpromoted])
    _, outcome, vacated = settle_spawn_role([unpromoted], scope="epic-x", plan=granted_plan)
    assert outcome == "granted"
    assert vacated == []

    caller = _role_row("caller")
    # The predecessor's live team: one child whose current owner names the
    # caller's session. Succession re-homes it in the same write that moves
    # the role; the birth edge stays history.
    child = replace(
        _role_row("w5", scope=None),
        spawned_by_session="caller-sess",
        spawn_provenance={
            "origin": {"kind": "session", "parent": {"harness": "claude", "session_id": "caller-sess", "cwd": "/w"}, "invocation": None},
            "owner": {"kind": "session", "harness": "claude", "session_id": "caller-sess", "cwd": "/w"},
        },
    )
    succeeded_plan = plan_for([caller, child], succession=True)
    rows, outcome, vacated = settle_spawn_role(
        [caller, child], scope="epic-x", plan=succeeded_plan,
        successor="Avery", successor_harness="codex", successor_session="successor-sess", successor_cwd="/w",
    )
    assert outcome == "succeeded"
    assert [r.role_scope for r in rows] == [None, None]
    assert [(r.name, cause) for r, cause in vacated] == [("caller", "succession"), ("w5", "reowned")]
    assert rows[1].spawn_provenance["owner"]["session_id"] == "successor-sess"
    assert rows[1].spawned_by_session == "caller-sess", "the birth edge stays history"

    # The race backstop: the plan was computed against "caller" holding the
    # scope, but the write sees "stranger" instead - declines rather than
    # applying a plan for a holder that is no longer there.
    stranger = _role_row("stranger")
    race_plan = plan_for([caller, child], succession=True)
    rows, outcome, vacated = settle_spawn_role(
        [stranger, child], scope="epic-x", plan=race_plan, successor="Avery", successor_harness="codex", successor_session="successor-sess", successor_cwd="/w",
    )
    assert outcome == "declined"
    assert rows[0].role_scope == "epic-x", "a declined spawn leaves the holder alone"
    assert rows[1].spawn_provenance["owner"]["session_id"] == "caller-sess", (
        "a declined succession re-homes nobody"
    )
    assert vacated == []

    dead = _role_row("dead", status="exited")
    granted_plan = plan_for([dead])
    rows, outcome, vacated = settle_spawn_role([dead], scope="epic-x", plan=granted_plan)
    assert outcome == "granted"
    assert [(r.name, cause) for r, cause in vacated] == [("dead", "holder_terminal")]
    assert rows[0].role_scope is None

    rebound_plan = plan_for([caller, child], succession=True)
    rebound = replace(caller, harness_session_id="caller-sess-2")
    rows, outcome, vacated = settle_spawn_role(
        [rebound, child], scope="epic-x", plan=rebound_plan, successor="Avery", successor_harness="codex", successor_session="successor-sess", successor_cwd="/w",
    )
    assert outcome == "declined"
    assert rows[0].role_scope == "epic-x"
    assert vacated == []


def test_settle_spawn_role_reown_skips_a_provenance_less_child(
    tmp_path: Path, monkeypatch, native_backlog_door,
) -> None:
    """A child with no spawn_provenance (adopt, pre-v33 birth edge) is never
    reowned: the planner selects provenance-carrying rows only, and the
    applier must not fork a block onto one (an origin-less block zeroed the
    Rust decode fleet-wide)."""
    from dataclasses import replace

    from fno.agents.role import plan_spawn_role, settle_spawn_role
    from fno.agents.registry import update_registry
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    caller = _role_row("caller")
    child = replace(
        _role_row("w5", scope=None),
        spawned_by_session="caller-sess",
    )
    update_registry(lambda _: [caller, child])
    refusal, plan = plan_spawn_role("epic-x", None, True, proposed_name="Avery")
    assert refusal is None
    rows, outcome, vacated = settle_spawn_role(
        [caller, child], scope="epic-x", plan=plan,
        successor="Avery", successor_harness="codex", successor_session="successor-sess", successor_cwd="/w",
    )
    assert outcome == "succeeded"
    assert [(r.name, cause) for r, cause in vacated] == [("caller", "succession")], (
        "the provenance-less child is not reowned"
    )
    assert rows[1].spawn_provenance is None, (
        "no provenance block is forked onto the child"
    )


def test_settle_spawn_role_declines_when_rust_is_unavailable(
    tmp_path: Path, monkeypatch, native_backlog_door,
) -> None:
    from dataclasses import asdict

    from fno.agents import spawn_overlay_client
    from fno.agents.role import plan_spawn_role, settle_spawn_role
    from fno.agents.registry import update_registry
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    caller = _role_row("caller")
    update_registry(lambda _: [caller])
    refusal, plan = plan_spawn_role("epic-x", None, True, proposed_name="Avery")
    assert refusal is None
    assert plan is not None
    before = asdict(caller)

    def unavailable(*args, **kwargs):
        raise spawn_overlay_client.SpawnOverlayUnavailable("not built")

    monkeypatch.setattr(spawn_overlay_client, "spawn_overlay_call", unavailable)
    rows, outcome, vacated = settle_spawn_role([caller], scope="epic-x", plan=plan)
    assert (outcome, vacated) == ("declined", [])
    assert asdict(rows[0]) == before


def test_unpromoted_spawn_leaves_role_none(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    _spawn_promoted(monkeypatch, tmp_path, grantor_env="parent-x")  # no role args
    row = load_registry()[0]
    assert row.role_level is None
    assert row.role_scope is None
    assert row.role_grantor is None
    assert row.role_label is None


# --- registry round-trip (write -> read preserves the role) -----------------


def test_role_round_trips_through_the_registry(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        write_registry,
    )

    entry = AgentEntry(
        name="lead-epic",
        cwd="/w",
        log_path="",
        harness="claude",
        harness_session_id="sess-lead-epic",  # x-7bcd: needs a resolvable handle
        short_id="deadbeef",
        role_level=2,
        role_scope="proj-a",
        role_grantor="vp-sess",
    )
    write_registry([entry])
    back = load_registry()[0]
    assert (back.role_level, back.role_scope, back.role_grantor) == (2, "proj-a", "vp-sess")


# --- whoami surfaces the role -----------------------------------------------


def test_whoami_renders_a_role_line() -> None:
    from fno.agents.registry import AgentEntry
    from fno.agents.whoami import render_human, resolve_self

    row = AgentEntry(
        name="lead-epic", cwd="/w", log_path="", harness="claude",
        short_id="deadbeef", role_level=1, role_scope="epic-x",
        role_grantor="human", harness_session_id="s-lead",
    )
    result = resolve_self(env={"FNO_AGENT_SESSION": "s-lead"}, registry=[row])
    assert result.role == "L1 epic-x (by human)"
    assert "role:       L1 epic-x (by human)" in render_human(result)


def test_whoami_no_role_line_for_unpromoted() -> None:
    from fno.agents.registry import AgentEntry
    from fno.agents.whoami import render_human, resolve_self

    row = AgentEntry(
        name="worker", cwd="/w", log_path="", harness="claude", short_id="abc",
        harness_session_id="s-worker",
    )
    result = resolve_self(env={"FNO_AGENT_SESSION": "s-worker"}, registry=[row])
    assert result.role is None
    assert "role:" not in render_human(result)


# --- list marks promoted rows -------------------------------------------------


def test_list_serialize_marks_the_role() -> None:
    from fno.agents.format import serialize_entry
    from fno.agents.registry import AgentEntry

    promoted = AgentEntry(
        name="lead-epic", cwd="/w", log_path="", harness="claude", short_id="a",
        role_level=1, role_scope="epic-x", role_grantor="human",
    )
    plain = AgentEntry(name="worker", cwd="/w", log_path="", harness="claude", short_id="b")

    js = serialize_entry(promoted, None)
    assert js["role"] == "L1 epic-x"
    assert js["role_level"] == 1 and js["role_grantor"] == "human"
    assert serialize_entry(plain, None)["role"] is None


# --- attended in-place role promotion --------------------------------------


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry
    harness = kw.pop("harness", "claude")
    return AgentEntry(name=name, cwd="/w", log_path="", harness=harness, **kw)


def _seed(monkeypatch, tmp_path, rows) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import write_registry
    write_registry(rows)


def _prepare_role_cli(monkeypatch, tmp_path, rows) -> None:
    from fno.harness_identity import AMBIENT_IDENTITY_ENV
    from fno.projects import resolve as proj_resolve

    # The lock-time identity match runs through Rust's role-identity kind
    # (the lock-time identity match runs through Rust now), so pin this
    # checkout's dev build like native_backlog_door
    # does; the smoke pytest legs skip by design when it has none.
    from fno.rust_binary import find_dev_binary

    binary = find_dev_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    monkeypatch.setenv("FNO_AGENTS_BIN", str(binary))
    # The binary-side kinds that read the agents home (team-rescope) must
    # never resolve the ambient fleet store from a test.
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / ".agents-home"))
    _seed(monkeypatch, tmp_path, [replace(row, cwd=str(tmp_path)) for row in rows])
    for name in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
    config = tmp_path / "config.toml"
    config.write_text(
        '[work.workspaces.ws1]\n'
        'projects = [{ name = "alpha", short_name = "a" }, { name = "beta" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", config)
    proj_resolve._clear_cache()


def _invoke_role(*args: str):
    from fno.agents.cli import agents_app

    return CliRunner().invoke(agents_app, ["role", *args])


def test_attended_shell_roles_an_existing_live_session(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import load_registry
    import fno.lead.state as lead_state

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                status="idle",
                mux={"session": "main", "pane_id": 7},
            )
        ],
    )
    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)
    import fno.agents.role as role_mod

    # The receipt's delivery line is asserted by its own tests below; pin it
    # here so this exact-dict assertion stays about the role fields. The
    # team-name carry line is the same shape of advisory receipt data.
    monkeypatch.setattr(
        role_mod, "_send_term_verb", lambda address, verb: "msg-t delivered (hosted)"
    )
    monkeypatch.setattr(role_mod, "_carry_team_name", lambda *args: "carried")
    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout) == {
        "promoted": "worker",
        "level": 1,
        "scope": "alpha",
        "grantor": "human",
        "vacated_scope": None,
        "vacated_level": None,
        "stranded_subordinates": [],
        "missions_armed": [],
        "lead_loop_armed": True,
        "team_name": "carried",
        "term_delivery": "msg-t delivered (hosted)",
    }
    row = load_registry()[0]
    assert (row.role_level, row.role_scope, row.role_grantor) == (
        1,
        "alpha",
        "human",
    )
    manifest = _space_manifest(tmp_path, "alpha")
    assert lead_state.parse_manifest(manifest)["harness_session_id"] == "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"


def test_in_place_role_receipt_names_a_disabled_lead_loop(
    tmp_path: Path, monkeypatch
) -> None:
    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", status="idle")],
    )

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["lead_loop_armed"] is False


def test_role_and_manifest_are_written_in_one_call(tmp_path: Path, monkeypatch) -> None:
    """x-f0d2: the manifest is the durable role record, the row its cache.

    `fno agents role` stamps the row and writes the same role triple onto
    the manifest in the one call, so a row the registry loses can heal its
    role back from the file.
    """
    import fno.lead.state as lead_state
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                status="idle",
            )
        ],
    )
    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    row = load_registry()[0]
    manifest = lead_state.parse_manifest(_space_manifest(tmp_path, "alpha"))
    assert manifest["harness_session_id"] == row.harness_session_id
    assert manifest["role_scope"] == row.role_scope == "alpha"
    assert manifest["role_grantor"] == row.role_grantor == "human"
    assert int(manifest["role_level"]) == row.role_level == 1


def test_promote_stamps_the_holders_own_harness(tmp_path: Path, monkeypatch) -> None:
    """x-f845: a claude lead promoting a codex successor must not write
    `harness: claude` beside a codex session id. The manifest harness keys
    every transcript reader (hygiene, compactions), so the caller's ambient
    harness there resolves the successor blind."""
    import fno.lead.state as lead_state

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness="codex",
                harness_session_id="0197aaaa-1234-7abc-9def-0123456789ab",
                status="idle",
            )
        ],
    )
    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    manifest = lead_state.parse_manifest(_space_manifest(tmp_path, "alpha"))
    assert manifest["harness"] == "codex"
    assert manifest["harness_session_id"] == "0197aaaa-1234-7abc-9def-0123456789ab"


def test_in_place_role_aborts_before_registry_publish_when_manifest_write_fails(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.lead.state as lead_state
    from fno.agents.role import RolePromotionError, promote_existing_session
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", status="idle")],
    )
    monkeypatch.setattr(
        lead_state,
        "arm_lead_manifest",
        lambda *args, **kwargs: (_ for _ in ()).throw(OSError("disk full")),
        raising=False,
    )

    with pytest.raises(RolePromotionError, match="manifest"):
        promote_existing_session("worker", ["alpha"])
    assert load_registry()[0].role_scope is None


def test_in_place_role_preserves_every_non_role_field(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    target = _entry(
        "worker",
        harness="codex",
        harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        status="idle",
        mux={"session": "main", "pane_id": 7},
        delivery_policy="bus-only",
        spawned_by_session="parent-session",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [target])
    before = asdict(load_registry()[0])

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    after = asdict(load_registry()[0])
    for field in ("role_level", "role_scope", "role_grantor"):
        before.pop(field)
        after.pop(field)
    assert after == before


def test_unpromoted_agent_caller_is_refused_without_mutation(
    tmp_path: Path, monkeypatch
) -> None:
    """An agent caller holding no role has nothing to hand down - grant_error's
    unpromoted branch, reached now that identity alone no longer refuses first."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "holds none" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_agent_caller_whose_role_strictly_contains_scope_is_accepted(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP: an L1-equivalent caller promoted over a portfolio may re-scope a
    live subordinate into a project it strictly contains, and the registry
    records the CALLER as grantor rather than the literal 'human'."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["grantor"] == "caller"
    row = next(r for r in load_registry() if r.name == "worker")
    assert (row.role_level, row.role_scope, row.role_grantor) == (
        1,
        "alpha",
        "caller",
    )


@pytest.mark.parametrize("status", ["exited", "orphaned", "failed", "permanent_dead"])
def test_terminal_agent_grantor_is_refused_before_authority_check(
    tmp_path: Path, monkeypatch, status: str
) -> None:
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status=status,
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    target = _entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status="idle")
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert status in result.output
    assert "grantor" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_agent_caller_whose_role_does_not_contain_scope_is_refused_without_mutation(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-EDGE: a role that neither contains nor equals the request is
    refused, and the registry is not mutated."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
        role_level=1,
        role_scope="beta",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "neither contains nor equals" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


@pytest.mark.parametrize("status", ["exited", "orphaned", "failed", "permanent_dead"])
def test_in_place_role_refuses_a_terminal_target_without_mutation(
    tmp_path: Path, monkeypatch, status: str
) -> None:
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status=status)],
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert status in result.output
    # AC2-shaped: the refusal names the way out, and never points at a reader
    # that contradicts it. The old text sent the caller to `fno agents list`,
    # whose computed live_status says "live" for exactly the stale-stored row
    # this branch refuses - measured against a real session on 2026-08-20.
    assert "STORED status" in result.output
    assert "fno agents register" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_grantor_whose_own_role_moved_mid_grant_is_refused_under_the_lock(
    tmp_path: Path, monkeypatch
) -> None:
    """The authority check runs outside the registry lock, so a grantor's role
    can move between the check and the stamp. Re-asserted under the lock as a
    plain attribute compare, and it must fail CLOSED: the caller passed
    grant_error holding 'alpha,beta', then lost 'alpha' before the write."""
    from fno.agents import role as role_mod
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    target = _entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status="idle")
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    # Authority is read here; the demotion lands after, before the stamp.
    real_calling_agent_row = role_mod.calling_agent_row

    def _demote_after_reading(*args, **kwargs):
        row = real_calling_agent_row(*args, **kwargs)
        rows = load_registry()
        from fno.agents.registry import write_registry

        write_registry(
            [
                replace(r, role_level=1, role_scope="beta")
                if r.name == "caller"
                else r
                for r in rows
            ]
        )
        return row

    monkeypatch.setattr(role_mod, "calling_agent_row", _demote_after_reading)

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "moved from" in result.output
    # The target was never promoted: authority that evaporated grants nothing.
    # (The caller's own row DID change - this test demotes it on purpose - so a
    # whole-registry equality check would be asserting the fixture, not the fix.)
    after = {r.name: r for r in load_registry()}
    assert after["worker"].role_scope is None
    assert after["worker"].role_level is None


def test_grantor_that_becomes_terminal_mid_grant_is_refused_under_the_lock(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import role as role_mod
    from fno.agents.registry import load_registry, write_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    target = _entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status="idle")
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    real_calling_agent_row = role_mod.calling_agent_row

    def _terminal_after_reading(*args, **kwargs):
        row = real_calling_agent_row(*args, **kwargs)
        write_registry(
            [
                replace(r, status="exited") if r.name == "caller" else r
                for r in load_registry()
            ]
        )
        return row

    monkeypatch.setattr(role_mod, "calling_agent_row", _terminal_after_reading)

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "exited" in result.output
    assert load_registry()[1].role_scope is None


def test_terminal_target_refusal_names_the_stale_snapshot_not_a_live_probe(
    tmp_path: Path, monkeypatch
) -> None:
    """A live session whose row an earlier sweep stamped `orphaned` is the case
    that stalled a real promotion: the refusal read the stored field and then
    named `fno agents list`, whose freshly-computed live_status disagreed. The
    refusal must say the value is a snapshot and name what restamps it."""
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status="orphaned")],
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "snapshot, not a live probe" in result.output
    assert "fno agents register" in result.output
    assert "fno agents reconcile" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_in_place_role_refuses_an_unknown_target_without_mutation(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status="idle")],
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("ghost", "--scope", "alpha")

    assert result.exit_code == 2
    assert "no agent" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_in_place_role_rescopes_an_already_promoted_target(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry
    import fno.lead.state as lead_state

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                status="idle",
                role_level=1,
                role_scope="beta",
                role_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)
    old_manifest = _space_manifest(tmp_path, "beta")
    lead_state.write_manifest(
        old_manifest, scope="beta", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    )
    from fno.agents import spawn_overlay_client
    real_call = spawn_overlay_client.spawn_overlay_call

    captured = {}

    def capture(payload, **kwargs):
        if payload.get("kind") == "team-rescope":
            captured.update(payload)
            return {"carried": True, "named": "Kestrel", "reason": None}
        return real_call(payload, **kwargs)

    monkeypatch.setattr(spawn_overlay_client, "spawn_overlay_call", capture)

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert captured == {
        "kind": "team-rescope",
        "old_scope": "beta",
        "new_scope": "alpha",
        "candidate": "worker",
        "holder_session": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "level": 1,
    }
    assert json.loads(result.stdout)["vacated_scope"] == "beta"
    assert json.loads(result.stdout)["vacated_level"] == 1
    assert json.loads(result.stdout)["team_name"] == "Kestrel"
    row = load_registry()[0]
    assert (row.role_level, row.role_scope, row.role_grantor) == (
        1,
        "alpha",
        "human",
    )
    assert not old_manifest.exists()
    assert _space_manifest(tmp_path, "alpha").exists()


def test_in_place_rescope_does_not_delete_a_successor_manifest(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import registry
    import fno.lead.state as lead_state

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                status="idle",
                role_level=1,
                role_scope="beta",
                role_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)
    old_manifest = _space_manifest(tmp_path, "beta")
    lead_state.write_manifest(
        old_manifest, scope="beta", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    )
    real_update_registry = registry.update_registry

    def update_then_refresh_successor(mutator):
        rows = real_update_registry(mutator)
        lead_state.write_manifest(
            old_manifest, scope="beta", harness_session_id="successor-session", force=True
        )
        return rows

    monkeypatch.setattr(registry, "update_registry", update_then_refresh_successor)

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert lead_state.parse_manifest(old_manifest)["harness_session_id"] == (
        "successor-session"
    )


def test_in_place_role_rescopes_a_live_row_from_project_to_epic(
    tmp_path: Path, monkeypatch
) -> None:
    from fno import paths
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                status="busy",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            )
        ],
    )
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    seed_graph(graph_path, json.dumps({"entries": [{"id": "e-1", "type": "epic", "project": "alpha"}]}))

    result = _invoke_role("worker", "--scope", "e-1")

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["vacated_scope"] == "alpha"
    assert receipt["vacated_level"] == 1
    rows = load_registry()
    row = rows[0]
    # The level was DERIVED from the epic, not carried over from the old role.
    assert (row.role_level, row.role_scope, row.role_grantor) == (
        2,
        "e-1",
        "human",
    )
    assert not any(r.role_scope == "alpha" for r in rows)


def test_rescope_into_a_scope_another_live_row_holds_is_still_refused(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    incumbent = _entry(
        "incumbent",
        harness_session_id="incumbent-session",
        status="busy",
        role_level=1,
        role_scope="beta",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [incumbent, target])
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("worker", "--scope", "beta")

    assert result.exit_code == 2
    assert "already held" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_rescope_refusal_names_the_ways_out_and_never_force(
    tmp_path: Path, monkeypatch
) -> None:
    incumbent = _entry(
        "incumbent",
        harness_session_id="incumbent-session",
        status="busy",
        role_level=1,
        role_scope="beta",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [incumbent, target])

    result = _invoke_role("worker", "--scope", "beta")

    assert result.exit_code == 2
    assert "incumbent" in result.output
    assert "fno agents org promote incumbent --scope" in result.output
    assert "reconcile" in result.output
    assert "stop" in result.output
    assert "--force" not in result.output
    assert "-F" not in result.output


def test_rescope_onto_the_scope_already_held_is_an_idempotent_no_op(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                status="busy",
                role_level=1,
                role_scope="alpha",
                role_grantor="human",
            )
        ],
    )

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    row = load_registry()[0]
    assert (row.role_level, row.role_scope, row.role_grantor) == (
        1,
        "alpha",
        "human",
    )


def test_rescope_emits_the_vacated_pair_on_the_event(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import events

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                status="busy",
                role_level=1,
                role_scope="beta",
                role_grantor="human",
            )
        ],
    )
    emitted: list[tuple[str, dict]] = []
    monkeypatch.setattr(events, "emit", lambda kind, **data: emitted.append((kind, data)))

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert emitted == [
        (
            "agent_promoted",
            {
                "name": "worker",
                "level": 1,
                "scope": "alpha",
                "grantor": "human",
                "vacated_scope": "beta",
                "vacated_level": 1,
                "stranded_subordinates": [],
            },
        )
    ]


def test_rescope_names_subordinates_stranded_by_the_move(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    lead = _entry(
        "lead",
        harness_session_id="lead-session",
        status="busy",
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    sub = _entry(
        "sub",
        harness_session_id="sub-session",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="lead",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [lead, sub])

    result = _invoke_role("lead", "--scope", "beta")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == ["sub"]
    # The subordinate keeps serving; the report is the only trace the move
    # out-ran its grant.
    row = next(r for r in load_registry() if r.name == "sub")
    assert (row.role_level, row.role_scope) == (1, "alpha")


def test_no_op_rescope_reports_no_strands(tmp_path: Path, monkeypatch) -> None:
    """A re-scope onto the same territory strands nobody: vacated and new are
    the same territory, so the report is [] even with a live subordinate
    inside it."""
    lead = _entry(
        "lead",
        harness_session_id="lead-session",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )
    sub = _entry(
        "sub",
        harness_session_id="sub-session",
        status="busy",
        role_level=2,
        role_scope="e-1",
        role_grantor="lead",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [lead, sub])

    result = _invoke_role("lead", "--scope", "a")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == []


def test_widening_rescope_keeps_contained_subordinates_out_of_the_report(
    tmp_path: Path, monkeypatch
) -> None:
    """A widened scope still contains what the old one did, so a subordinate
    inside the old territory is not stranded by the move."""
    from fno import paths

    lead = _entry(
        "lead",
        harness_session_id="lead-session",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )
    sub = _entry(
        "sub",
        harness_session_id="sub-session",
        status="busy",
        role_level=2,
        role_scope="e-1",
        role_grantor="lead",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [lead, sub])
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    seed_graph(graph_path, json.dumps({"entries": [{"id": "e-1", "type": "epic", "project": "alpha"}]}))

    result = _invoke_role("lead", "--scope", "alpha", "--scope", "beta")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == []


@pytest.mark.parametrize(
    "half",
    [
        {"role_level": 1},
        {"role_scope": "alpha"},
    ],
)
def test_in_place_role_refuses_half_a_role(
    tmp_path: Path, monkeypatch, half: dict
) -> None:
    """Level without scope or scope without level is unstampable by
    role_validation_error, so no legal writer produces either shape; the
    re-scope must surface the corruption, not silently overwrite it."""
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                status="idle",
                **half,
            )
        ],
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("worker", "--scope", "beta")

    assert result.exit_code == 2
    assert "half a role" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_unreadable_graph_reads_null_on_the_strand_check(
    tmp_path: Path, monkeypatch
) -> None:
    """None is the could-not-check answer and must never collapse to []: a
    regression there would print verified-no-strands on machines whose graph
    the scan cannot read."""
    from fno.tracker import metadata

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                status="busy",
                role_level=1,
                role_scope="beta",
                role_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(
        metadata,
        "read_entries",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("unreadable")),
    )

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] is None


def test_stale_epic_row_is_listed_conservatively(
    tmp_path: Path, monkeypatch
) -> None:
    """A live row promoted over an epic the graph no longer holds is listed
    rather than nulled or dropped: containment for it is unknowable, and one
    stale row must not silence the determinate answers for other rows."""
    lead = _entry(
        "lead",
        harness_session_id="lead-session",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )
    stale = _entry(
        "stale",
        harness_session_id="stale-session",
        status="busy",
        role_level=2,
        role_scope="e-gone",
        role_grantor="lead",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [lead, stale])

    result = _invoke_role("lead", "--scope", "beta")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == ["stale"]


def test_in_place_role_refuses_a_second_live_holder_for_the_scope(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    incumbent = _entry(
        "incumbent",
        harness_session_id="incumbent-session",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [incumbent, target])
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("worker", "--scope", "a")

    assert result.exit_code == 2
    assert "already held" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_in_place_role_refuses_overlapping_territory_not_just_the_same_set(
    tmp_path: Path, monkeypatch
) -> None:
    """A rung-2 set is a union: a live lead over e-1,e-2 already rules e-1, so
    promoting another row over e-1 alone is a double rule. The equality-only
    scan let it through; the guard must fire on any shared member."""
    from fno.agents.registry import load_registry
    import fno.agents.role as role_mod

    incumbent = _entry(
        "incumbent",
        harness_session_id="incumbent-session",
        status="busy",
        role_level=2,
        role_scope="e-1,e-2",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [incumbent, target])
    monkeypatch.setattr(
        role_mod,
        "_graph_entry",
        lambda nid: {"id": nid, "type": "epic", "project": "fno"}
        if nid in ("e-1", "e-2")
        else None,
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("worker", "--scope", "e-1")

    assert result.exit_code == 2
    assert "already held" in result.output.lower()
    assert "incumbent" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_a_portfolio_leads_team_holds_project_leads(
    tmp_path: Path, monkeypatch
) -> None:
    """The ladder documents a portfolio lead's team AS project leads, so a
    live row over 'alpha,beta' and a new role over 'alpha' are two legitimate
    roles. Bare member overlap refused exactly that grant."""
    from fno.agents.registry import load_registry

    incumbent = _entry(
        "portfolio-lead",
        harness_session_id="portfolio-session",
        status="busy",
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [incumbent, target])

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert (receipt["level"], receipt["scope"]) == (1, "alpha")
    held = {r.name: r.role_scope for r in load_registry()}
    assert held == {"portfolio-lead": "alpha,beta", "worker": "alpha"}


def test_a_project_leads_grant_covers_an_epic_set_under_it(
    tmp_path: Path, monkeypatch
) -> None:
    """A lead could grant one epic but never two: scope_contains read every
    multi-member inner as a set-subset test, and a set of epic ids is never a
    subset of a set of project names. The set must fall under the role that
    holds every member."""
    import fno.agents.role as role_mod
    from fno.agents.role import grant_error

    _prepare_role_cli(monkeypatch, tmp_path, [])
    monkeypatch.setattr(
        role_mod,
        "_graph_index",
        lambda: {
            nid: {"id": nid, "type": "epic", "project": "alpha"}
            for nid in ("e-1", "e-2")
        },
    )
    lead = _entry(
        "lead",
        harness_session_id="lead-session",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )

    assert grant_error("e-1,e-2", lead) is None
    assert grant_error("e-1,e-3", lead) is not None


def test_two_epics_role_in_place_as_one_set(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry
    import fno.agents.role as role_mod

    target = _entry(
        "mux-lead",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [target])
    monkeypatch.setattr(
        role_mod,
        "_graph_index",
        lambda: {
            nid: {"id": nid, "type": "epic", "project": "fno"}
            for nid in ("e-1", "e-2")
        },
    )

    result = _invoke_role("mux-lead", "--scope", "e-1", "--scope", "e-2")

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert (receipt["level"], receipt["scope"]) == (2, "e-1,e-2")
    row = next(r for r in load_registry() if r.name == "mux-lead")
    assert (row.role_level, row.role_scope) == (2, "e-1,e-2")


def test_a_lead_cannot_role_itself_even_to_a_strict_subset(
    tmp_path: Path, monkeypatch
) -> None:
    """The role self-edit decision lives in the Rust role-widen rule. A
    strict SUBSET is the case the succession refusal misses, since that one
    fires only on an equal scope: here a portfolio lead over alpha,beta
    narrows itself to alpha, which passes containment AND passes succession.
    beta is still live, so the drop refuses and the hint names the command
    that would succeed - the registry never moves."""
    import fno.agents.role as role_mod
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="busy",
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller])
    rows = {
        "alpha": {"id": "alpha", "type": "epic", "status": "ready"},
        "beta": {"id": "beta", "type": "epic", "status": "ready"},
    }
    monkeypatch.setattr(role_mod, "_graph_index", lambda: rows)
    monkeypatch.setattr(role_mod, "_graph_entry", lambda node_id: rows.get(node_id))
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_role("caller", "--scope", "alpha")

    assert result.exit_code == 2
    assert "beta" in result.output
    assert "--scope alpha --scope beta" in result.output
    # The mutation this refusal exists to prevent: the wider scope is not
    # vacated on the way out.
    assert [asdict(row) for row in load_registry()] == before


WIDEN_LEAD_SESSION = "aaaaaaaa-1111-4aaa-8aaa-aaaaaaaaaaaa"
WIDEN_OTHER_SESSION = "bbbbbbbb-2222-4bbb-8bbb-bbbbbbbbbbbb"


def _widen_graph(monkeypatch, created_by: str) -> None:
    """Stub role._graph_index with epic rows whose source_session_id names
    who filed them: e-1 predates the lead, e-2 is created_by's birth."""
    import fno.agents.role as role_mod

    monkeypatch.setattr(
        role_mod,
        "_graph_index",
        lambda: {
            "e-1": {"id": "e-1", "type": "epic", "project": "fno",
                    "source_session_id": "human"},
            "e-2": {"id": "e-2", "type": "epic", "project": "fno",
                    "source_session_id": created_by},
        },
    )


def _widen_lead() -> object:
    return _entry(
        "lead-a",
        harness_session_id=WIDEN_LEAD_SESSION,
        status="busy",
        role_level=2,
        role_scope="e-1",
        role_grantor="human",
    )


def test_a_lead_adds_an_epic_its_own_session_created_to_its_role(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """AC4-HP: the lead the epic child cap creates (a new small epic) can
    role itself over it. The lead holds e-1; its own session created e-2;
    naming both widens its own role in place, keeping the upstream grantor
    instead of self-declaring the row."""
    from fno.agents.registry import load_registry

    _prepare_role_cli(monkeypatch, tmp_path, [_widen_lead()])
    _widen_graph(monkeypatch, created_by=WIDEN_LEAD_SESSION)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", WIDEN_LEAD_SESSION)

    result = _invoke_role("lead-a", "--scope", "e-1", "--scope", "e-2")

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert (receipt["level"], receipt["scope"]) == (2, "e-1,e-2")
    assert receipt["grantor"] == "human"
    assert receipt["vacated_scope"] == "e-1"
    row = next(r for r in load_registry() if r.name == "lead-a")
    assert (row.role_level, row.role_scope, row.role_grantor) == (
        2, "e-1,e-2", "human",
    )


def test_a_lead_cannot_add_an_epic_another_session_created(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """AC5-ERR: the widen answers false when the new epic's birth record
    names another session, so today's containment refusal stands alone and
    the registry never moves."""
    from fno.agents.registry import load_registry

    _prepare_role_cli(monkeypatch, tmp_path, [_widen_lead()])
    _widen_graph(monkeypatch, created_by=WIDEN_OTHER_SESSION)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", WIDEN_LEAD_SESSION)
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("lead-a", "--scope", "e-1", "--scope", "e-2")

    assert result.exit_code == 2
    assert "neither contains nor equals" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_a_widen_into_an_epic_another_live_lead_holds_is_refused(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """AC5-ERR: the widen passes the authority gates, then the rival scan
    under the registry lock refuses: another live row already roles e-2."""
    from fno.agents.registry import load_registry

    rival = _entry(
        "lead-c",
        harness_session_id=WIDEN_OTHER_SESSION,
        status="busy",
        role_level=2,
        role_scope="e-2",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [_widen_lead(), rival])
    _widen_graph(monkeypatch, created_by=WIDEN_LEAD_SESSION)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", WIDEN_LEAD_SESSION)
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("lead-a", "--scope", "e-1", "--scope", "e-2")

    assert result.exit_code == 2
    assert "already held by live row 'lead-c'" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_a_widen_shaped_grant_to_another_row_is_refused(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """AC6-EDGE: the widen answer belongs to this session's own row. The
    same scopes aimed at another row meet the ordinary containment refusal,
    never a stamped role."""
    from fno.agents.registry import load_registry

    other = _entry(
        "lead-c",
        harness_session_id=WIDEN_OTHER_SESSION,
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [_widen_lead(), other])
    _widen_graph(monkeypatch, created_by=WIDEN_LEAD_SESSION)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", WIDEN_LEAD_SESSION)
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("lead-c", "--scope", "e-1", "--scope", "e-2")

    assert result.exit_code == 2
    assert "neither contains nor equals" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_a_self_widen_fails_closed_without_the_binary(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-EDGE: no role-widen answer (an old or missing binary) means
    today's refusal stands - the widen never fires on a guess."""
    from fno.agents.registry import load_registry
    from fno.agents import spawn_overlay_client
    import fno.agents.role as role_mod

    _prepare_role_cli(monkeypatch, tmp_path, [_widen_lead()])
    _widen_graph(monkeypatch, created_by=WIDEN_LEAD_SESSION)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", WIDEN_LEAD_SESSION)

    def _unavailable(payload):
        raise spawn_overlay_client.SpawnOverlayUnavailable("no binary")

    monkeypatch.setattr(spawn_overlay_client, "spawn_overlay_call", _unavailable)
    before = [asdict(row) for row in load_registry()]

    result = _invoke_role("lead-a", "--scope", "e-1", "--scope", "e-2")

    assert result.exit_code == 2
    assert "neither contains nor equals" in result.output
    assert [asdict(row) for row in load_registry()] == before
    # The helper, called directly, answers the empty dict fail-closed form.
    assert role_mod._widen_answer("e-1,e-2", _widen_lead(), "lead-a") == {}


def test_a_lead_drops_a_done_epic_and_keeps_the_prior_grantor(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """AC4-HP (drop): a promoted lead narrows its own role over a done
    member with no attended shell, the receipt names the vacated scope and
    the skipped self-mail, and the recorded grantor survives the edit."""
    import fno.agents.role as role_mod
    from fno.agents.registry import load_registry

    lead = _entry(
        "lead-a",
        harness_session_id=WIDEN_LEAD_SESSION,
        status="busy",
        role_level=2,
        role_scope="e-1,e-2",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [lead])
    rows = {
        "e-1": {"id": "e-1", "type": "epic", "status": "ready"},
        "e-2": {"id": "e-2", "type": "epic", "status": "done"},
    }
    monkeypatch.setattr(role_mod, "_graph_index", lambda: rows)
    monkeypatch.setattr(role_mod, "_graph_entry", lambda node_id: rows.get(node_id))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", WIDEN_LEAD_SESSION)

    result = _invoke_role("lead-a", "--scope", "e-1")

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["scope"] == "e-1"
    assert receipt["grantor"] == "human"
    assert receipt["vacated_scope"] == "e-1,e-2"
    assert (
        receipt["term_delivery"] == "skipped: self-edit, this session already terms"
    )
    row = next(r for r in load_registry() if r.name == "lead-a")
    assert (row.role_level, row.role_scope, row.role_grantor) == (
        2, "e-1", "human",
    )


def test_a_noop_self_rescope_is_refused_as_self_declared(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-ERR: a self re-scope onto the same territory adds and drops
    nothing, which is the succession shape - refused with the moved
    never-self-declared text, not a stamped role."""
    import fno.agents.role as role_mod
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness_session_id="caller-session",
        status="busy",
        role_level=1,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller])
    monkeypatch.setattr(
        role_mod,
        "_graph_index",
        lambda: {
            "alpha": {"id": "alpha", "type": "epic", "status": "ready"},
            "beta": {"id": "beta", "type": "epic", "status": "ready"},
        },
    )
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "caller-session")

    result = _invoke_role("caller", "--scope", "alpha", "--scope", "beta")

    assert result.exit_code == 2
    assert "never self-declared" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_a_pure_drop_without_the_binary_refuses_fail_closed(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-EDGE: no role-widen answer (an old or missing binary) refuses a
    pure drop too - a self edit never fires on a guess."""
    from fno.agents.registry import load_registry
    from fno.agents import spawn_overlay_client

    caller = _entry(
        "caller",
        harness_session_id="caller-session",
        status="busy",
        role_level=1,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller])
    import fno.agents.role as role_mod

    rows = {
        "alpha": {"id": "alpha", "type": "epic", "status": "ready"},
        "beta": {"id": "beta", "type": "epic", "status": "ready"},
    }
    monkeypatch.setattr(role_mod, "_graph_index", lambda: rows)
    monkeypatch.setattr(role_mod, "_graph_entry", lambda node_id: rows.get(node_id))
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "caller-session")

    def _unavailable(payload):
        raise spawn_overlay_client.SpawnOverlayUnavailable("no binary")

    monkeypatch.setattr(spawn_overlay_client, "spawn_overlay_call", _unavailable)

    result = _invoke_role("caller", "--scope", "alpha")

    assert result.exit_code == 2
    assert "was unavailable" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_a_self_add_answer_without_a_grantor_refuses_fail_closed(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-EDGE: a widen-true answer with no grantor key is an OLD binary
    (it predates the grantor echo); stamping would self-declare the row,
    so the self edit refuses and names the update remedy."""
    from fno.agents.registry import load_registry
    from fno.agents import spawn_overlay_client

    lead = _entry(
        "lead-a",
        harness_session_id=WIDEN_LEAD_SESSION,
        status="busy",
        role_level=2,
        role_scope="e-1",
        role_grantor="human",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [lead])
    _widen_graph(monkeypatch, created_by=WIDEN_LEAD_SESSION)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", WIDEN_LEAD_SESSION)
    before = [asdict(row) for row in load_registry()]

    def _old_binary(payload):
        return {"widen": True, "added": ["e-2"], "hint": None}

    monkeypatch.setattr(spawn_overlay_client, "spawn_overlay_call", _old_binary)

    result = _invoke_role("lead-a", "--scope", "e-1", "--scope", "e-2")

    assert result.exit_code == 2
    assert "update the fno-agents binary" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_succession_by_an_agent_caller_is_refused_with_a_reachable_remedy(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-EDGE: grant_error's succession branch accepts an equal scope because
    SPAWN succession is legal there. This verb only stamps the target, so it
    must refuse - and the refusal has to name a remedy the caller can act on.
    Falling through to the live-holder scan named the caller's OWN row as the
    blocker and offered three remedies that all contradict the refusal: re-scope
    yourself, reconcile yourself, or stop yourself."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="busy",
        role_level=1,
        role_scope="alpha",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_role("worker", "--scope", "alpha")

    assert result.exit_code == 2
    out = result.output.lower()
    assert "your own scope" in out
    assert "fno agents spawn --promote" in out
    # The remedies that contradict the refusal must not appear: every one of
    # them tells the caller to act on its own live row.
    assert "already held" not in out
    assert "fno agents stop caller" not in out
    assert "fno agents reconcile" not in out
    assert [asdict(row) for row in load_registry()] == before


def test_agent_caller_with_a_wrongly_typed_scope_sees_the_type_refusal_not_identity(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP ordering: resolve_role runs BEFORE the authority check, so an
    agent caller naming a feature-typed node hits the type refusal - not an
    identity/containment refusal. This fails on the pre-reorder code for the
    ordering reason alone, which is the proof the reorder landed."""
    from fno import paths
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
        role_level=0,
        role_scope="alpha,beta",
        role_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        status="idle",
    )
    _prepare_role_cli(monkeypatch, tmp_path, [caller, target])
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    seed_graph(graph_path, json.dumps({"entries": [{"id": "n-1", "type": "feature", "project": "alpha"}]}))
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_role("worker", "--scope", "n-1")

    assert result.exit_code == 2
    assert "fno backlog update n-1 --type epic" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_in_place_role_canonicalizes_a_portfolio_and_derives_its_level(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status="idle")],
    )

    result = _invoke_role(
        "worker",
        "--scope",
        "beta",
        "--scope",
        "a",
    )

    assert result.exit_code == 0, result.output
    row = load_registry()[0]
    assert (row.role_level, row.role_scope) == (0, "alpha,beta")


def test_in_place_role_help_teaches_the_attended_workflow() -> None:
    result = _invoke_role("--help")

    assert result.exit_code == 0, result.output
    assert "attended shell" in result.output.lower()
    assert "fno agents register" in result.output
    assert "strictly contains" in result.output.lower()
    assert "re-scope" in result.output.lower()
    assert "--level" not in result.output
    assert "--succeed" not in result.output
    assert "--hand-off" not in result.output


def test_in_place_role_emits_one_success_event_only_after_commit(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import events

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry("worker", harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", status="idle"),
            _entry(
                "incumbent",
                harness_session_id="incumbent-session",
                status="busy",
                role_level=1,
                role_scope="beta",
                role_grantor="human",
            ),
        ],
    )
    emitted: list[tuple[str, dict]] = []
    monkeypatch.setattr(events, "emit", lambda kind, **data: emitted.append((kind, data)))

    success = _invoke_role("worker", "--scope", "alpha")
    refused = _invoke_role("worker", "--scope", "beta")

    assert success.exit_code == 0, success.output
    assert refused.exit_code == 2
    assert emitted == [
        (
            "agent_promoted",
            {
                "name": "worker",
                "level": 1,
                "scope": "alpha",
                "grantor": "human",
                "vacated_scope": None,
                "vacated_level": None,
                "stranded_subordinates": [],
            },
        )
    ]


def test_racing_in_place_roles_leave_exactly_one_live_holder(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.role import RolePromotionError, promote_existing_session
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry("one", harness_session_id="session-one", status="idle"),
            _entry("two", harness_session_id="session-two", status="idle"),
        ],
    )

    def promote(name: str) -> str:
        try:
            promote_existing_session(name, ["alpha"])
        except RolePromotionError:
            return "refused"
        return "promoted"

    with ThreadPoolExecutor(max_workers=2) as pool:
        outcomes = list(pool.map(promote, ["one", "two"]))

    assert sorted(outcomes) == ["promoted", "refused"]
    holders = [row for row in load_registry() if row.role_scope == "alpha"]
    assert len(holders) == 1


def test_in_place_role_refuses_a_name_rebound_inside_the_lock_window(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """The target resolved before the lock is matched by name AND
    session under the lock. A row re-registered under the same name with a
    new session inside the window is not the session that gets promoted."""
    import fno.agents.registry as registry_mod
    from fno.agents.role import RolePromotionError, promote_existing_session
    from fno.agents.registry import load_registry

    original = _entry(
        "worker", harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", status="idle"
    )
    _prepare_role_cli(monkeypatch, tmp_path, [original])
    # The rebind: the old row is dropped and a new one appended under the
    # same name while the role resolves its handle, so the pre-lock read
    # still reports the dead session. resolve_agent is patched to return
    # that stale snapshot; the registry write sees only the rebound row.
    rebound = replace(
        original, harness_session_id="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    )
    registry_mod.write_registry([rebound])

    class _StaleResolution:
        entry = original

    monkeypatch.setattr(
        registry_mod, "resolve_agent", lambda handle, **kw: _StaleResolution()
    )

    with pytest.raises(RolePromotionError, match="disappeared before"):
        promote_existing_session("worker", ["alpha"])
    row = load_registry()[0]
    assert (row.role_level, row.role_scope, row.role_grantor) == (None, None, None), (
        "a rebound name is never promoted"
    )
    assert row.harness_session_id == "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"


def test_in_place_role_refuses_when_the_identity_check_is_unavailable(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """No role-identity answer, no role: the racy name-only match is never
    the fallback (the lock-time identity match runs through Rust)."""
    from fno.agents import spawn_overlay_client
    from fno.agents.role import RolePromotionError, promote_existing_session
    from fno.agents.registry import load_registry

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker", harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", status="idle"
            )
        ],
    )

    def unavailable(*args, **kwargs):
        raise spawn_overlay_client.SpawnOverlayUnavailable("not built")

    monkeypatch.setattr(spawn_overlay_client, "spawn_overlay_call", unavailable)

    with pytest.raises(RolePromotionError, match="unavailable"):
        promote_existing_session("worker", ["alpha"])
    row = load_registry()[0]
    assert (row.role_level, row.role_scope, row.role_grantor) == (None, None, None)


def test_in_place_role_refuses_a_grantor_row_rebound_inside_the_lock_window(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """The grantor re-asserted under the lock is matched by name AND
    session too. A grantor name rebound to a row promoted over the same
    scope cannot grant what the calling session no longer holds."""
    import fno.agents.registry as registry_mod
    from fno.agents.role import RolePromotionError, promote_existing_session
    from fno.agents.registry import load_registry

    grantor = _entry(
        "lead",
        harness_session_id="cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        status="idle",
        role_level=1,
        role_scope="alpha,beta",
    )
    worker = _entry(
        "worker", harness_session_id="dddddddd-dddd-4ddd-8ddd-dddddddddddd", status="idle"
    )
    _prepare_role_cli(monkeypatch, tmp_path, [grantor, worker])
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "cccccccc-cccc-4ccc-8ccc-cccccccccccc")

    real_update = registry_mod.update_registry

    def _rebind_then_update(fn):
        # The grantor's row is dropped and re-registered under the same name
        # with a new session (keeping its role) between the CLI's caller
        # read and its stamp write.
        real_update(
            lambda rows: [
                replace(row, harness_session_id="eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee")
                if row.name == "lead"
                else row
                for row in rows
            ]
        )
        return real_update(fn)

    monkeypatch.setattr(registry_mod, "update_registry", _rebind_then_update)

    with pytest.raises(RolePromotionError, match="moved"):
        promote_existing_session("worker", ["alpha"])
    row = load_registry()[1]
    assert (row.role_level, row.role_scope, row.role_grantor) == (None, None, None), (
        "a rebound grantor's authority never lands"
    )




def test_spawn_role_refuses_before_launch_when_scope_already_occupied(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """A role-bearing spawn at an already-occupied scope, with no --succeed,
    refuses BEFORE launch rather than spawning unpromoted: launching a successor
    that holds no role is exactly the failure the pre-launch occupancy check
    exists to prevent."""
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    use_tmpdir(monkeypatch, tmp_path)
    # Pre-seed an existing promoted row over scope "epic-x"
    write_registry([AgentEntry(
        name="incumbent", harness="claude", cwd="/w", log_path="",
        harness_session_id="sess-incumbent",  # x-7bcd: needs a resolvable handle
        short_id="inc", status="live",
        role_level=1, role_scope="epic-x", role_grantor="human",
    )])
    # Spawn a new worker with --promote level=1,scope=epic-x (same scope). The
    # caller is an attended human (no agent identity), so it is authorized to
    # attempt the grant; the pre-launch check still refuses because the
    # incumbent holds the scope and no --succeed named the transfer.
    with pytest.raises(DispatchAskError, match="--hand-off"):
        _spawn_promoted(
            monkeypatch, tmp_path,
            grantor_env=None,
            role_level=1, role_scope="epic-x",
        )
    rows = load_registry()
    assert not [r for r in rows if r.name == "Avery"], "a refused role must launch nothing"
    # The incumbent's role is untouched
    inc = next(r for r in rows if r.name == "incumbent")
    assert inc.role_level == 1
    assert inc.role_scope == "epic-x"
    assert not _space_manifest(tmp_path, "epic-x").exists()


# --- the role types the verb (x-7b36 change 11): raw mail names the delivery


def test_in_place_role_mails_the_term_verb_and_names_the_delivery(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.agents.role as role_mod
    from fno.agents.role import promote_existing_session

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", status="idle")],
    )
    sent: list[tuple[str, str]] = []
    monkeypatch.setattr(
        role_mod, "_send_term_verb", lambda address, verb: sent.append((address, verb)) or "msg-1 delivered (hosted)"
    )

    receipt = promote_existing_session("worker", ["alpha"])

    assert receipt["term_delivery"] == "msg-1 delivered (hosted)"
    # AC28: the holder receives the plugin-qualified verb by raw mail,
    # addressed by the full session id (the ADDRESS, never the spawn label).
    assert sent == [("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "/fno:lead alpha")]


def test_in_place_role_renders_the_term_verb_for_codex(
    tmp_path: Path, monkeypatch
) -> None:
    """x-c976: the term verb renders through the one normalizer, so a codex
    holder receives the `$fno:` spelling."""
    import fno.agents.role as role_mod
    from fno.agents.role import promote_existing_session

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness="codex", harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", status="idle")],
    )
    sent: list[tuple[str, str]] = []
    monkeypatch.setattr(
        role_mod, "_send_term_verb", lambda address, verb: sent.append((address, verb)) or "msg-1 delivered (hosted)"
    )

    receipt = promote_existing_session("worker", ["alpha"])

    assert receipt["term_delivery"] == "msg-1 delivered (hosted)"
    assert sent == [("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "$fno:lead alpha")]


def test_term_verb_send_failure_is_named_not_silent(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.agents.role as role_mod
    from fno.agents.role import promote_existing_session

    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", status="idle")],
    )
    monkeypatch.setattr(
        role_mod, "_send_term_verb", lambda address, verb: "not delivered (rc=16: no such agent)"
    )

    receipt = promote_existing_session("worker", ["alpha"])

    # The role still commits; the receipt names the miss instead of silence.
    assert receipt["promoted"] == "worker"
    assert receipt["term_delivery"].startswith("not delivered")


def test_send_term_verb_reports_subprocess_failure(monkeypatch) -> None:
    import fno.agents.role as role_mod

    class _Proc:
        returncode = 16
        stdout = ""
        stderr = "resolve failed: no such agent"

    # _send_term_verb imports subprocess locally, so patch the module it
    # resolves from at call time.
    import subprocess as real_subprocess

    monkeypatch.setattr(real_subprocess, "run", lambda *a, **k: _Proc())
    verdict = role_mod._send_term_verb("worker", "/fno:lead alpha")
    assert verdict == "not delivered (rc=16: resolve failed: no such agent)"


# --- a role grant arms the epic's mission -----------------------------------
#
# Operator rule 2026-09-09: every epic with an owner is a mission. The drain
# keys one loop per epic with mission_active=true, so a role that left the
# flag unset ruled a territory no drain loop could see.


def _seed_role_graph(monkeypatch, tmp_path: Path, epics: list[dict]) -> None:
    # Pin the events journal into the tmp root: use_tmpdir redirects
    # state_dir but not project_events_json, and an unpinned journal is
    # shared by every test in the process.
    monkeypatch.setenv(
        "FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl")
    )
    from fno.paths import graph_json

    graph_path = graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    seed_graph(graph_path, json.dumps({"entries": epics}))


def _graph_entries() -> list[dict]:
    from fno.graph.store import read_graph_strict
    from fno.paths import graph_json

    return read_graph_strict(graph_json())


def _mission_events() -> list[dict]:
    """The journal's mission_activated rows (the pinned journal also carries
    agent_* rows from the role telemetry, which share the tmp root)."""
    from fno.paths import project_events_json

    from tests._event_rows import event_rows

    return [
        e for e in event_rows(project_events_json())
        if e.get("type") == "mission_activated"
    ]


def test_a_spawn_grant_over_an_unflagged_epic_arms_its_mission(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.role import journal_spawn_role

    _prepare_role_cli(monkeypatch, tmp_path, [])
    _seed_role_graph(
        monkeypatch,
        tmp_path,
        [
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress"},
            {"id": "c-1", "type": "task", "parent": "e-1"},
        ],
    )

    journal_spawn_role(
        "granted", [], name="w", level=2, scope="e-1", grantor="human"
    )

    entry = _graph_entries()[0]
    assert entry["mission_active"] is True
    events = [
        e for e in _mission_events() if e.get("type") == "mission_activated"
    ]
    assert len(events) == 1
    assert events[0]["data"] == {"epic_id": "e-1", "source": "role"}


def test_a_declined_spawn_arms_nothing(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.role import journal_spawn_role

    _prepare_role_cli(monkeypatch, tmp_path, [])
    _seed_role_graph(
        monkeypatch,
        tmp_path,
        [
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress"},
            {"id": "c-1", "type": "task", "parent": "e-1"},
        ],
    )

    journal_spawn_role(
        "declined", [], name="w", level=2, scope="e-1", grantor="human"
    )

    assert "mission_active" not in _graph_entries()[0]
    assert _mission_events() == []


def test_a_project_scope_and_a_done_epic_arm_nothing(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.role import journal_spawn_role

    _prepare_role_cli(monkeypatch, tmp_path, [])
    _seed_role_graph(
        monkeypatch,
        tmp_path,
        [
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "done"},
            {"id": "p-1", "type": "project", "status": "in_progress"},
        ],
    )

    journal_spawn_role(
        "granted", [], name="w", level=1, scope="e-1,p-1,alpha", grantor="human"
    )

    by_id = {e["id"]: e for e in _graph_entries()}
    assert "mission_active" not in by_id["e-1"]
    assert "mission_active" not in by_id["p-1"]
    assert _mission_events() == []


def test_a_two_epic_scope_arms_both(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.role import journal_spawn_role

    _prepare_role_cli(monkeypatch, tmp_path, [])
    _seed_role_graph(
        monkeypatch,
        tmp_path,
        [
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress"},
            {"id": "c-1", "type": "task", "parent": "e-1"},
            {"id": "e-2", "type": "epic", "project": "beta", "status": "in_progress"},
            {"id": "c-2", "type": "task", "parent": "e-2"},
        ],
    )

    journal_spawn_role(
        "granted", [], name="w", level=2, scope="e-2,e-1", grantor="human"
    )

    by_id = {e["id"]: e for e in _graph_entries()}
    assert by_id["e-1"]["mission_active"] is True
    assert by_id["e-2"]["mission_active"] is True
    armed = {
        e["data"]["epic_id"]
        for e in _mission_events()
        if e.get("type") == "mission_activated"
    }
    assert armed == {"e-1", "e-2"}


def test_a_graph_fault_leaves_the_role_committed(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.backlog.advance as advance_mod
    from fno.agents.role import journal_spawn_role, promote_existing_session
    from fno.agents.registry import load_registry

    def _raisers(epic_id, active):
        raise RuntimeError("graph write refused")

    monkeypatch.setattr(advance_mod, "_set_mission_active", _raisers)
    _prepare_role_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                status="idle",
            )
        ],
    )
    _seed_role_graph(
        monkeypatch,
        tmp_path,
        [
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress"},
            {"id": "c-1", "type": "task", "parent": "e-1"},
        ],
    )

    # The spawn leg returns normally even though the arming write raised.
    journal_spawn_role(
        "granted", [], name="w", level=2, scope="e-1", grantor="human"
    )

    receipt = promote_existing_session("worker", ["e-1"])
    assert receipt["missions_armed"] is None
    # The role row still stands.
    row = next(r for r in load_registry() if r.name == "worker")
    assert (row.role_level, row.role_scope) == (2, "e-1")


def test_recrowning_an_armed_epic_emits_nothing(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.role import journal_spawn_role

    _prepare_role_cli(monkeypatch, tmp_path, [])
    _seed_role_graph(
        monkeypatch,
        tmp_path,
        [
            {
                "id": "e-1",
                "type": "epic",
                "project": "alpha",
                "status": "in_progress",
                "mission_active": True,
            },
            {"id": "c-1", "type": "task", "parent": "e-1"},
        ],
    )

    journal_spawn_role(
        "granted", [], name="w", level=2, scope="e-1", grantor="human"
    )

    assert _mission_events() == []


def test_a_childless_epic_is_not_armed(tmp_path: Path, monkeypatch) -> None:
    """advance_epic refuses a childless epic as not-a-container and leaves the
    flag standing, which the drain would poll forever; arming waits for
    children, where the dispatch lever takes over."""
    from fno.agents.role import journal_spawn_role

    _prepare_role_cli(monkeypatch, tmp_path, [])
    _seed_role_graph(
        monkeypatch,
        tmp_path,
        [{"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress"}],
    )

    journal_spawn_role(
        "granted", [], name="w", level=2, scope="e-1", grantor="human"
    )

    assert "mission_active" not in _graph_entries()[0]
    assert _mission_events() == []


# --- the manifest arms when the successor identifies --------------------------
#
# Spawn-time succession carries the role in the registry while the successor
# child still has no harness session id, so the spawn lane's arm attempt
# refuses and the manifest keeps naming the stepping_down session. The
# SessionStart observation is the first moment the successor is addressable;
# that is where the manifest must rewrite.

SUCCESSOR_ID = "22222222-2222-4222-8222-222222222222"
PREDECESSOR_ID = "11111111-1111-4111-8111-111111111111"


def _promoted_row(tmp_path, monkeypatch, name="successor", **kw):
    from fno.agents.registry import AgentEntry, write_registry
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            AgentEntry(
                name=name,
                cwd=str(tmp_path),
                log_path=str(tmp_path / f"{name}.log"),
                harness="claude",
                short_id=kw.pop("short_id", "deadbeef"),
                role_level=2,
                role_scope="x-epic",
                **kw,
            )
        ]
    )


def _arm_enabled(monkeypatch):
    import fno.lead.state as lead_state

    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)


def _scope_manifest(tmp_path):
    from fno.paths import space_dir

    return space_dir(tmp_path) / "leads" / "x-epic.md"


def test_spawn_succession_arms_the_manifest_when_the_successor_identifies(
    tmp_path, monkeypatch
) -> None:
    from fno.agents.registry import record_session_observation
    from fno.lead.state import parse_manifest

    _promoted_row(tmp_path, monkeypatch, harness_session_id="")
    _arm_enabled(monkeypatch)

    entry, outcome = record_session_observation(
        name="successor", harness="claude", session_id=SUCCESSOR_ID
    )

    assert outcome == "primary"
    manifest = _scope_manifest(tmp_path)
    assert manifest.exists(), "the successor's first id must arm the wake gate"
    assert parse_manifest(manifest)["harness_session_id"] == SUCCESSOR_ID


def test_succession_in_place_repoints_the_manifest_at_the_new_session(
    tmp_path, monkeypatch
) -> None:
    from fno.agents.registry import record_session_observation
    from fno.lead.state import parse_manifest, write_manifest

    _promoted_row(
        tmp_path, monkeypatch, harness_session_id=PREDECESSOR_ID
    )
    _arm_enabled(monkeypatch)
    write_manifest(
        _scope_manifest(tmp_path),
        scope="x-epic",
        harness_session_id=PREDECESSOR_ID,
    )

    entry, outcome = record_session_observation(
        name="successor",
        harness="claude",
        session_id=SUCCESSOR_ID,
        predecessor_reachable=False,
        expected_predecessor_session_id=PREDECESSOR_ID,
    )

    assert outcome == "succession"
    assert parse_manifest(_scope_manifest(tmp_path))["harness_session_id"] == (
        SUCCESSOR_ID
    )


def test_a_branch_never_arms_the_role_manifest(tmp_path, monkeypatch) -> None:
    """A branch inherits no role and no claim; the predecessor's manifest
    authority stays exactly where it was."""
    from fno.agents.registry import record_session_observation

    _promoted_row(
        tmp_path, monkeypatch, harness_session_id=PREDECESSOR_ID
    )
    _arm_enabled(monkeypatch)

    entry, outcome = record_session_observation(
        name="successor",
        harness="claude",
        session_id=SUCCESSOR_ID,
        predecessor_reachable=True,
        expected_predecessor_session_id=PREDECESSOR_ID,
    )

    assert outcome == "branch"
    assert not _scope_manifest(tmp_path).exists()


def test_an_unpromoted_row_arms_nothing(tmp_path, monkeypatch) -> None:
    from fno.agents.registry import AgentEntry, record_session_observation, write_registry
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            AgentEntry(
                name="plain",
                cwd=str(tmp_path),
                log_path=str(tmp_path / "plain.log"),
                harness="claude",
            )
        ]
    )
    _arm_enabled(monkeypatch)

    entry, outcome = record_session_observation(
        name="plain", harness="claude", session_id=SUCCESSOR_ID
    )

    assert outcome == "primary"
    assert not _scope_manifest(tmp_path).exists()


def test_a_refused_arm_is_an_event_never_a_blocked_session_start(
    tmp_path, monkeypatch
) -> None:
    """The restamp hook is fail-soft: a manifest arming that cannot produce a
    transcript-matchable id surfaces as an event, never an exception."""
    import fno.agents.events as events
    from fno.agents.registry import record_session_observation

    _promoted_row(tmp_path, monkeypatch, harness_session_id="")
    _arm_enabled(monkeypatch)
    seen: dict[str, object] = {}
    monkeypatch.setattr(
        events, "emit", lambda kind, **data: seen.setdefault("kind", kind)
    )

    entry, outcome = record_session_observation(
        name="successor", harness="claude", session_id="shortid"
    )

    assert outcome == "primary", "the registry write itself still lands"
    assert seen.get("kind") == "role_manifest_arm_failed"
    assert not _scope_manifest(tmp_path).exists()
