"""US9 (KFAD squad court): orchestrator crown visibility.

A crown is stamped on the spawned worker's registry row by the SPAWN path
(grantor derived from the spawning session, never self-declared), survives a
round-trip, and surfaces in `fno whoami` and `fno agents list`.
"""
from __future__ import annotations

import json
import subprocess
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict
from pathlib import Path
from typing import Optional

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


# --- the stored scope encoding ----------------------------------------------
#
# There is no `--crown` spec parser any more. The flag takes scopes directly and
# the rung is derived (see test_crown_level_derivation), so what remains to pin
# here is the ENCODING: a portfolio has to reduce to one canonical string,
# because the one-live-crown guard compares scopes by equality and would
# otherwise miss a duplicate spelled in a different order.


def test_a_portfolio_canonicalizes_regardless_of_spelling() -> None:
    from fno.agents.crown import canonical_scope

    assert canonical_scope(["web", "etl"]) == canonical_scope(["etl", "web"])
    assert canonical_scope(["etl", "web", "etl"]) == "etl,web"
    assert canonical_scope([" etl ", "web"]) == "etl,web"


def test_split_is_the_inverse_of_canonical() -> None:
    from fno.agents.crown import canonical_scope, split_scope

    assert split_scope(canonical_scope(["web", "etl"])) == ["etl", "web"]
    assert split_scope("epic-x") == ["epic-x"]
    assert split_scope(None) == []


def test_scope_contains_canonicalizes_an_alias_project(monkeypatch, tmp_path) -> None:
    """scope_contains must canonicalize the graph node's project field before
    comparing it to the canonicalized crown scope. Graph intake stores the
    project field RAW (the short_name alias a node was filed under), so without
    canonicalization a king over 'alpha' is falsely refused an epic filed as
    'a' - a legitimate delegation blocked."""
    import fno.projects.resolve as proj_resolve

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\nprojects = [{ name = "alpha", short_name = "a" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()

    from fno.agents import crown

    # An epic filed under the alias 'a' (the raw spelling intake stores).
    monkeypatch.setattr(
        crown, "_graph_entry", lambda nid: {"id": nid, "type": "epic", "project": "a"}
    )
    assert crown.scope_contains("alpha", "epic-1") is True

    # A genuinely different project is still not contained.
    monkeypatch.setattr(
        crown, "_graph_entry", lambda nid: {"id": nid, "type": "epic", "project": "beta"}
    )
    assert crown.scope_contains("alpha", "epic-1") is False


@pytest.mark.parametrize(
    "level,scope",
    [
        (-1, "epic-x"),          # cannot deserialize into the Rust Option<u32>
        (3, "epic-x"),           # over the ladder ceiling
        (True, "epic-x"),        # bool is an int subclass; serializes as JSON true
        (1, ""),                 # blank scope
        (1, None),               # a crown that rules nothing
        (None, "epic-x"),        # a scope with no rung
        (0, "web,etl"),          # not canonical: unsorted
        (1, "etl,web"),          # a portfolio is level 0, not 1
    ],
)
def test_the_store_gate_rejects_unstampable_pairs(level, scope) -> None:
    """The last check before the shared registry. Values arrive here from
    in-process callers that never touch the CLI, so this cannot live in the flag
    layer."""
    from fno.agents.crown import crown_validation_error

    assert crown_validation_error(level, scope) is not None


def test_the_store_gate_passes_the_two_legal_shapes() -> None:
    from fno.agents.crown import crown_validation_error

    assert crown_validation_error(None, None) is None      # an uncrowned spawn
    assert crown_validation_error(2, "epic-x") is None     # a Director
    assert crown_validation_error(0, "etl,web") is None    # a portfolio


# --- spawn stamps the crown, grantor is provenance not self-declared ---------


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


def _spawn_crowned(monkeypatch, tmp_path, *, grantor_env: Optional[str], **crown):
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
        name="king-epic",
        message="reign",
        provider="claude",
        cwd=tmp_path,
        runner=_FakeRunner(),
        **crown,
    )


def test_crown_stamped_grantor_is_the_spawning_session(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import AgentEntry, load_registry, update_registry

    use_tmpdir(monkeypatch, tmp_path)
    # Seat the grantor as a registered king over epic-x; the spawn is then a
    # succession. An agent identity with no registry row is now refused at the
    # grantor check, so the agent must be seated - the corrected opposite of the
    # fail-open these tests rode. Reuse _spawn_crowned so the provider axis
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
                crown_level=1,
                crown_scope="epic-x",
                crown_grantor="human",
            )
        ]
    )
    _spawn_crowned(
        monkeypatch, tmp_path,
        grantor_env="parent-sess-abc",
        crown_level=1, crown_scope="epic-x",
    )
    heir = next(e for e in load_registry() if e.name == "king-epic")
    assert heir.crown_level == 1
    assert heir.crown_scope == "epic-x"
    # Provenance, not self-declared: the grantor is who actually spawned it.
    assert heir.crown_grantor == "parent-sess-abc"
    assert heir.crown_label == "L1 epic-x"


def test_crown_grantor_defaults_to_human_for_a_direct_spawn(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    _spawn_crowned(
        monkeypatch, tmp_path,
        grantor_env=None,  # no parent session env == a human's own shell
        crown_level=0, crown_scope="proj-a",
    )
    row = load_registry()[0]
    assert row.crown_grantor == "human"
    assert row.crown_level == 0


def test_uncrowned_spawn_leaves_crown_none(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    _spawn_crowned(monkeypatch, tmp_path, grantor_env="parent-x")  # no crown args
    row = load_registry()[0]
    assert row.crown_level is None
    assert row.crown_scope is None
    assert row.crown_grantor is None
    assert row.crown_label is None


# --- registry round-trip (write -> read preserves the crown) -----------------


def test_crown_round_trips_through_the_registry(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        write_registry,
    )

    entry = AgentEntry(
        name="king-epic",
        cwd="/w",
        log_path="",
        harness="claude",
        harness_session_id="sess-king-epic",  # x-7bcd: needs a resolvable handle
        short_id="deadbeef",
        crown_level=2,
        crown_scope="proj-a",
        crown_grantor="vp-sess",
    )
    write_registry([entry])
    back = load_registry()[0]
    assert (back.crown_level, back.crown_scope, back.crown_grantor) == (2, "proj-a", "vp-sess")


# --- whoami surfaces the crown -----------------------------------------------


def test_whoami_renders_a_crown_line() -> None:
    from fno.agents.registry import AgentEntry
    from fno.agents.whoami import render_human, resolve_self

    row = AgentEntry(
        name="king-epic", cwd="/w", log_path="", harness="claude",
        short_id="deadbeef", crown_level=1, crown_scope="epic-x",
        crown_grantor="human",
    )
    result = resolve_self(env={"FNO_AGENT_SELF": "king-epic"}, registry=[row])
    assert result.crown == "L1 epic-x (by human)"
    assert "crown:       L1 epic-x (by human)" in render_human(result)


def test_whoami_no_crown_line_for_uncrowned() -> None:
    from fno.agents.registry import AgentEntry
    from fno.agents.whoami import render_human, resolve_self

    row = AgentEntry(name="worker", cwd="/w", log_path="", harness="claude", short_id="abc")
    result = resolve_self(env={"FNO_AGENT_SELF": "worker"}, registry=[row])
    assert result.crown is None
    assert "crown:" not in render_human(result)


# --- list marks crowned rows -------------------------------------------------


def test_list_serialize_and_table_mark_the_crown() -> None:
    from fno.agents.format import render_table, serialize_entry
    from fno.agents.registry import AgentEntry

    crowned = AgentEntry(
        name="king-epic", cwd="/w", log_path="", harness="claude", short_id="a",
        crown_level=1, crown_scope="epic-x", crown_grantor="human",
    )
    plain = AgentEntry(name="worker", cwd="/w", log_path="", harness="claude", short_id="b")

    js = serialize_entry(crowned, None)
    assert js["crown"] == "L1 epic-x"
    assert js["crown_level"] == 1 and js["crown_grantor"] == "human"
    assert serialize_entry(plain, None)["crown"] is None

    table = render_table([serialize_entry(crowned, None), serialize_entry(plain, None)])
    assert "king-epic [L1 epic-x]" in table  # crowned row carries the marker
    # the uncrowned row's name is unadorned
    assert any(line.startswith("worker ") or line.strip().startswith("worker")
               for line in table.splitlines())


def test_top_rows_join_the_crown_by_name() -> None:
    from fno.agents.spawn_gate import LiveWorker
    from fno.agents.top import _rows

    def worker(name: str) -> "LiveWorker":
        return LiveWorker(
            source="fno", name=name, harness="claude",
            substrate="pane", pid=1, status="live",
        )

    # TWO workers and TWO crowns, deliberately: with one of each, a _rows that
    # ignored the name entirely and handed back the only crown present would
    # still pass. The join is only observable when a wrong one is available to
    # pick.
    rows = _rows(
        [worker("king-epic"), worker("king-other"), worker("plain")],
        {"king-epic": "L1 epic-x", "king-other": "L2 epic-y"},
    )
    by_name = {row["name"]: row["crown"] for row in rows}
    assert by_name["king-epic"] == "L1 epic-x"
    assert by_name["king-other"] == "L2 epic-y"
    assert by_name["plain"] is None
    assert _rows([worker("king-epic")], {})[0]["crown"] is None


# --- attended in-place crown promotion --------------------------------------


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry
    harness = kw.pop("harness", "claude")
    return AgentEntry(name=name, cwd="/w", log_path="", harness=harness, **kw)


def _seed(monkeypatch, tmp_path, rows) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import write_registry
    write_registry(rows)


def _prepare_crown_cli(monkeypatch, tmp_path, rows) -> None:
    from fno.harness_identity import AMBIENT_IDENTITY_ENV
    from fno.projects import resolve as proj_resolve

    _seed(monkeypatch, tmp_path, rows)
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


def _invoke_crown(*args: str):
    from fno.agents.cli import agents_app

    return CliRunner().invoke(agents_app, ["crown", *args])


def test_attended_shell_crowns_an_existing_live_session(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="session-worker",
                status="idle",
                mux={"session": "main", "pane_id": 7},
            )
        ],
    )
    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout) == {
        "crowned": "worker",
        "level": 1,
        "scope": "alpha",
        "grantor": "human",
        "vacated_scope": None,
        "vacated_level": None,
        "stranded_subordinates": [],
    }
    row = load_registry()[0]
    assert (row.crown_level, row.crown_scope, row.crown_grantor) == (
        1,
        "alpha",
        "human",
    )


def test_in_place_crown_preserves_every_non_crown_field(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    target = _entry(
        "worker",
        harness="codex",
        harness_session_id="session-worker",
        status="idle",
        mux={"session": "main", "pane_id": 7},
        delivery_policy="bus-only",
        spawned_by_session="parent-session",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [target])
    before = asdict(load_registry()[0])

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    after = asdict(load_registry()[0])
    for field in ("crown_level", "crown_scope", "crown_grantor"):
        before.pop(field)
        after.pop(field)
    assert after == before


def test_uncrowned_agent_caller_is_refused_without_mutation(
    tmp_path: Path, monkeypatch
) -> None:
    """An agent caller holding no crown has nothing to hand down - grant_error's
    uncrowned branch, reached now that identity alone no longer refuses first."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="idle",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [caller, target])
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "holds none" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_agent_caller_whose_crown_strictly_contains_scope_is_accepted(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP: an L1-equivalent caller crowned over a portfolio may re-scope a
    live subordinate into a project it strictly contains, and the registry
    records the CALLER as grantor rather than the literal 'human'."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
        crown_level=0,
        crown_scope="alpha,beta",
        crown_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="idle",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [caller, target])
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["grantor"] == "caller"
    row = next(r for r in load_registry() if r.name == "worker")
    assert (row.crown_level, row.crown_scope, row.crown_grantor) == (
        1,
        "alpha",
        "caller",
    )


def test_agent_caller_whose_crown_does_not_contain_scope_is_refused_without_mutation(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-EDGE: a crown that neither contains nor equals the request is
    refused, and the registry is not mutated."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="idle",
        crown_level=1,
        crown_scope="beta",
        crown_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="idle",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [caller, target])
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "neither contains nor equals" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


@pytest.mark.parametrize("status", ["exited", "orphaned", "failed", "permanent_dead"])
def test_in_place_crown_refuses_a_terminal_target_without_mutation(
    tmp_path: Path, monkeypatch, status: str
) -> None:
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="worker-session", status=status)],
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert status in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_in_place_crown_refuses_an_unknown_target_without_mutation(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="worker-session", status="idle")],
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_crown("ghost", "--scope", "alpha")

    assert result.exit_code == 2
    assert "no agent" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_in_place_crown_rescopes_an_already_crowned_target(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="worker-session",
                status="idle",
                crown_level=1,
                crown_scope="beta",
                crown_grantor="human",
            )
        ],
    )

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["vacated_scope"] == "beta"
    assert json.loads(result.stdout)["vacated_level"] == 1
    row = load_registry()[0]
    assert (row.crown_level, row.crown_scope, row.crown_grantor) == (
        1,
        "alpha",
        "human",
    )


def test_in_place_crown_rescopes_a_live_row_from_project_to_epic(
    tmp_path: Path, monkeypatch
) -> None:
    from fno import paths
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="worker-session",
                status="busy",
                crown_level=1,
                crown_scope="alpha",
                crown_grantor="human",
            )
        ],
    )
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(
        json.dumps({"entries": [{"id": "e-1", "type": "epic", "project": "alpha"}]}),
        encoding="utf-8",
    )

    result = _invoke_crown("worker", "--scope", "e-1")

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["vacated_scope"] == "alpha"
    assert receipt["vacated_level"] == 1
    rows = load_registry()
    row = rows[0]
    # The level was DERIVED from the epic, not carried over from the old crown.
    assert (row.crown_level, row.crown_scope, row.crown_grantor) == (
        2,
        "e-1",
        "human",
    )
    assert not any(r.crown_scope == "alpha" for r in rows)


def test_rescope_into_a_scope_another_live_row_holds_is_still_refused(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    incumbent = _entry(
        "incumbent",
        harness_session_id="incumbent-session",
        status="busy",
        crown_level=1,
        crown_scope="beta",
        crown_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="human",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [incumbent, target])
    before = [asdict(row) for row in load_registry()]

    result = _invoke_crown("worker", "--scope", "beta")

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
        crown_level=1,
        crown_scope="beta",
        crown_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="human",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [incumbent, target])

    result = _invoke_crown("worker", "--scope", "beta")

    assert result.exit_code == 2
    assert "incumbent" in result.output
    assert "fno agents crown incumbent --scope" in result.output
    assert "reconcile" in result.output
    assert "stop" in result.output
    assert "--force" not in result.output
    assert "-F" not in result.output


def test_rescope_onto_the_scope_already_held_is_an_idempotent_no_op(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="worker-session",
                status="busy",
                crown_level=1,
                crown_scope="alpha",
                crown_grantor="human",
            )
        ],
    )

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    row = load_registry()[0]
    assert (row.crown_level, row.crown_scope, row.crown_grantor) == (
        1,
        "alpha",
        "human",
    )


def test_rescope_emits_the_vacated_pair_on_the_event(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import events

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="worker-session",
                status="busy",
                crown_level=1,
                crown_scope="beta",
                crown_grantor="human",
            )
        ],
    )
    emitted: list[tuple[str, dict]] = []
    monkeypatch.setattr(events, "emit", lambda kind, **data: emitted.append((kind, data)))

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert emitted == [
        (
            "agent_crowned",
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

    king = _entry(
        "king",
        harness_session_id="king-session",
        status="busy",
        crown_level=0,
        crown_scope="alpha,beta",
        crown_grantor="human",
    )
    sub = _entry(
        "sub",
        harness_session_id="sub-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="king",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [king, sub])

    result = _invoke_crown("king", "--scope", "beta")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == ["sub"]
    # The subordinate keeps reigning; the report is the only trace the move
    # out-ran its grant.
    row = next(r for r in load_registry() if r.name == "sub")
    assert (row.crown_level, row.crown_scope) == (1, "alpha")


def test_no_op_rescope_reports_no_strands(tmp_path: Path, monkeypatch) -> None:
    """A re-scope onto the same territory strands nobody: vacated and new are
    the same territory, so the report is [] even with a live subordinate
    inside it."""
    king = _entry(
        "king",
        harness_session_id="king-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="human",
    )
    sub = _entry(
        "sub",
        harness_session_id="sub-session",
        status="busy",
        crown_level=2,
        crown_scope="e-1",
        crown_grantor="king",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [king, sub])

    result = _invoke_crown("king", "--scope", "a")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == []


def test_widening_rescope_keeps_contained_subordinates_out_of_the_report(
    tmp_path: Path, monkeypatch
) -> None:
    """A widened scope still contains what the old one did, so a subordinate
    inside the old territory is not stranded by the move."""
    from fno import paths

    king = _entry(
        "king",
        harness_session_id="king-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="human",
    )
    sub = _entry(
        "sub",
        harness_session_id="sub-session",
        status="busy",
        crown_level=2,
        crown_scope="e-1",
        crown_grantor="king",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [king, sub])
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(
        json.dumps({"entries": [{"id": "e-1", "type": "epic", "project": "alpha"}]}),
        encoding="utf-8",
    )

    result = _invoke_crown("king", "--scope", "alpha", "--scope", "beta")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == []


@pytest.mark.parametrize(
    "half",
    [
        {"crown_level": 1},
        {"crown_scope": "alpha"},
    ],
)
def test_in_place_crown_refuses_half_a_crown(
    tmp_path: Path, monkeypatch, half: dict
) -> None:
    """Level without scope or scope without level is unstampable by
    crown_validation_error, so no legal writer produces either shape; the
    re-scope must surface the corruption, not silently overwrite it."""
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="worker-session",
                status="idle",
                **half,
            )
        ],
    )
    before = [asdict(row) for row in load_registry()]

    result = _invoke_crown("worker", "--scope", "beta")

    assert result.exit_code == 2
    assert "half a crown" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_unreadable_graph_reads_null_on_the_strand_check(
    tmp_path: Path, monkeypatch
) -> None:
    """None is the could-not-check answer and must never collapse to []: a
    regression there would print verified-no-strands on machines whose graph
    the scan cannot read."""
    from fno.tracker import metadata

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "worker",
                harness_session_id="worker-session",
                status="busy",
                crown_level=1,
                crown_scope="beta",
                crown_grantor="human",
            )
        ],
    )
    monkeypatch.setattr(
        metadata,
        "read_entries",
        lambda *a, **k: (_ for _ in ()).throw(RuntimeError("unreadable")),
    )

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] is None


def test_stale_epic_row_is_listed_conservatively(
    tmp_path: Path, monkeypatch
) -> None:
    """A live row crowned over an epic the graph no longer holds is listed
    rather than nulled or dropped: containment for it is unknowable, and one
    stale row must not silence the determinate answers for other rows."""
    king = _entry(
        "king",
        harness_session_id="king-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="human",
    )
    stale = _entry(
        "stale",
        harness_session_id="stale-session",
        status="busy",
        crown_level=2,
        crown_scope="e-gone",
        crown_grantor="king",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [king, stale])

    result = _invoke_crown("king", "--scope", "beta")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["stranded_subordinates"] == ["stale"]


def test_in_place_crown_refuses_a_second_live_holder_for_the_scope(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    incumbent = _entry(
        "incumbent",
        harness_session_id="incumbent-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="idle",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [incumbent, target])
    before = [asdict(row) for row in load_registry()]

    result = _invoke_crown("worker", "--scope", "a")

    assert result.exit_code == 2
    assert "already held" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_succession_by_an_agent_caller_still_refuses_on_the_live_holder_scan(
    tmp_path: Path, monkeypatch
) -> None:
    """AC6-EDGE: grant_error's succession branch accepts an equal scope, but
    the caller is itself a live holder of that scope, so the one-live-crown
    scan still refuses - unchanged - naming its three ways out."""
    from fno.agents.registry import load_registry

    caller = _entry(
        "caller",
        harness="codex",
        harness_session_id="caller-session",
        status="busy",
        crown_level=1,
        crown_scope="alpha",
        crown_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="idle",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [caller, target])
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_crown("worker", "--scope", "alpha")

    assert result.exit_code == 2
    assert "already held" in result.output.lower()
    assert [asdict(row) for row in load_registry()] == before


def test_agent_caller_with_a_wrongly_typed_scope_sees_the_type_refusal_not_identity(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP ordering: resolve_crown runs BEFORE the authority check, so an
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
        crown_level=0,
        crown_scope="alpha,beta",
        crown_grantor="human",
    )
    target = _entry(
        "worker",
        harness_session_id="worker-session",
        status="idle",
    )
    _prepare_crown_cli(monkeypatch, tmp_path, [caller, target])
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(
        json.dumps({"entries": [{"id": "n-1", "type": "feature", "project": "alpha"}]}),
        encoding="utf-8",
    )
    before = [asdict(row) for row in load_registry()]
    monkeypatch.setenv("CODEX_THREAD_ID", "caller-session")

    result = _invoke_crown("worker", "--scope", "n-1")

    assert result.exit_code == 2
    assert "fno backlog update n-1 --type epic" in result.output
    assert [asdict(row) for row in load_registry()] == before


def test_in_place_crown_canonicalizes_a_portfolio_and_derives_its_level(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [_entry("worker", harness_session_id="worker-session", status="idle")],
    )

    result = _invoke_crown(
        "worker",
        "--scope",
        "beta",
        "--scope",
        "a",
    )

    assert result.exit_code == 0, result.output
    row = load_registry()[0]
    assert (row.crown_level, row.crown_scope) == (0, "alpha,beta")


def test_in_place_crown_help_teaches_the_attended_workflow() -> None:
    result = _invoke_crown("--help")

    assert result.exit_code == 0, result.output
    assert "attended shell" in result.output.lower()
    assert "fno agents register" in result.output
    assert "strictly contains" in result.output.lower()
    assert "re-scope" in result.output.lower()
    assert "--level" not in result.output
    assert "--succeed" not in result.output


def test_in_place_crown_emits_one_success_event_only_after_commit(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import events

    _prepare_crown_cli(
        monkeypatch,
        tmp_path,
        [
            _entry("worker", harness_session_id="worker-session", status="idle"),
            _entry(
                "incumbent",
                harness_session_id="incumbent-session",
                status="busy",
                crown_level=1,
                crown_scope="beta",
                crown_grantor="human",
            ),
        ],
    )
    emitted: list[tuple[str, dict]] = []
    monkeypatch.setattr(events, "emit", lambda kind, **data: emitted.append((kind, data)))

    success = _invoke_crown("worker", "--scope", "alpha")
    refused = _invoke_crown("worker", "--scope", "beta")

    assert success.exit_code == 0, success.output
    assert refused.exit_code == 2
    assert emitted == [
        (
            "agent_crowned",
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


def test_racing_in_place_crowns_leave_exactly_one_live_holder(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.crown import CrownPromotionError, promote_existing_session
    from fno.agents.registry import load_registry

    _prepare_crown_cli(
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
        except CrownPromotionError:
            return "refused"
        return "crowned"

    with ThreadPoolExecutor(max_workers=2) as pool:
        outcomes = list(pool.map(promote, ["one", "two"]))

    assert sorted(outcomes) == ["crowned", "refused"]
    holders = [row for row in load_registry() if row.crown_scope == "alpha"]
    assert len(holders) == 1




def test_spawn_crown_declined_when_scope_already_occupied(tmp_path: Path, monkeypatch) -> None:
    """A second crown-bearing spawn at an already-occupied scope spawns UNCROWNED,
    not a duplicate crown. The one-live-crown guard inside _append (mux_spawn.py)
    declines the crown atomically, under the registry write lock, so two racing
    spawns cannot both stamp."""
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    use_tmpdir(monkeypatch, tmp_path)
    # Pre-seed an existing crowned row over scope "epic-x"
    write_registry([AgentEntry(
        name="incumbent", harness="claude", cwd="/w", log_path="",
        harness_session_id="sess-incumbent",  # x-7bcd: needs a resolvable handle
        short_id="inc", status="live",
        crown_level=1, crown_scope="epic-x", crown_grantor="human",
    )])
    # Spawn a new worker with --crown level=1,scope=epic-x (same scope). The
    # caller is an attended human (no agent identity), so it is authorized to
    # attempt the grant; the guard declines it because the incumbent holds it.
    _spawn_crowned(
        monkeypatch, tmp_path,
        grantor_env=None,
        crown_level=1, crown_scope="epic-x",
    )
    rows = load_registry()
    new = next(r for r in rows if r.name == "king-epic")
    # The worker launched (exists in the registry) but WITHOUT a crown
    assert new.crown_level is None
    assert new.crown_scope is None
    assert new.crown_grantor is None
    # The incumbent's crown is untouched
    inc = next(r for r in rows if r.name == "incumbent")
    assert inc.crown_level == 1
    assert inc.crown_scope == "epic-x"
