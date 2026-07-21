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


# --- --crown flag parsing ----------------------------------------------------


def test_parse_crown_valid_orderfree() -> None:
    from fno.agents.cli import _parse_crown

    assert _parse_crown("level=1,scope=epic-x") == (1, "epic-x")
    # order-free
    assert _parse_crown("scope=proj-a,level=0") == (0, "proj-a")


@pytest.mark.parametrize(
    "spec",
    [
        "level=1",             # missing scope
        "scope=x",             # missing level
        "level=notanint,scope=x",
        "level=-1,scope=x",    # negative
        "level=1,scope=",      # blank scope
        "garbage",             # no k=v
    ],
)
def test_parse_crown_rejects_malformed(spec: str) -> None:
    import typer

    from fno.agents.cli import _parse_crown

    with pytest.raises(typer.Exit) as exc:
        _parse_crown(spec)
    assert exc.value.exit_code == 2


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
    from fno.agents.registry import load_registry

    _spawn_crowned(
        monkeypatch, tmp_path,
        grantor_env="parent-sess-abc",
        crown_level=1, crown_scope="epic-x",
    )
    row = load_registry()[0]
    assert row.crown_level == 1
    assert row.crown_scope == "epic-x"
    # Provenance, not self-declared: the grantor is who actually spawned it.
    assert row.crown_grantor == "parent-sess-abc"
    assert row.crown_label == "L1 epic-x"


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
        source="fno", name="king-epic", provider="claude",
        substrate="pane", pid=1, status="live",
    )
    assert _rows([w], {"king-epic": "L1 epic-x"})[0]["crown"] == "L1 epic-x"
    assert _rows([w], {})[0]["crown"] is None
