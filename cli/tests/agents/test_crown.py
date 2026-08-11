"""US9 (KFAD squad court): orchestrator crown visibility.

A crown is stamped on the spawned worker's registry row by the SPAWN path
(grantor derived from the spawning session, never self-declared), survives a
round-trip, and surfaces in `fno whoami` and `fno agents list`.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional

import pytest

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
    monkeypatch.delenv("FNO_SESSION", raising=False)
    for var in ("CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-sess-abc")
    # The grantor is a REGISTERED king over epic-x, so the child spawn is a
    # succession (it hands its own scope to the heir). An agent identity with no
    # registry row is now refused at the grantor check, so the agent must be
    # seated - this is the corrected opposite of the fail-open these tests rode.
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

    from fno.agents.mux_spawn import dispatch_spawn_pane

    dispatch_spawn_pane(
        name="king-epic",
        message="reign",
        provider="claude",
        cwd=tmp_path,
        runner=_FakeRunner(),
        crown_level=1,
        crown_scope="epic-x",
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

    w = LiveWorker(
        source="fno", name="king-epic", harness="claude",
        substrate="pane", pid=1, status="live",
    )
    assert _rows([w], {"king-epic": "L1 epic-x"})[0]["crown"] == "L1 epic-x"
    assert _rows([w], {})[0]["crown"] is None


# --- US10: `fno agents crown` promotion verb ---------------------------------

from types import SimpleNamespace


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry
    return AgentEntry(name=name, cwd="/w", log_path="", harness="claude", **kw)


def _seed(monkeypatch, tmp_path, rows) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import write_registry
    write_registry(rows)




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
