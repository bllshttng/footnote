"""/blueprint refreshes the node's graph status so doc and graph agree.

The graph derives `status` FROM the plan doc, but `read_graph` never
recomputes - only a graph mutation does. So a doc that /blueprint just moved
design -> ready would keep reading `design` on every board until something
unrelated touched the graph. The blueprint write triggers the recompute itself.
"""
from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

_SCRIPT = (
    Path(__file__).resolve().parents[3]
    / "skills" / "blueprint" / "scripts" / "mutate_doc.py"
)


def _load():
    spec = importlib.util.spec_from_file_location("_blueprint_mutate_doc", _SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


mod = _load()


@pytest.fixture
def calls(monkeypatch):
    seen: list[list[str]] = []

    class _Proc:
        returncode = 0
        stderr = ""

    monkeypatch.setattr(mod.shutil, "which", lambda _: "/usr/bin/fno")
    monkeypatch.setattr(
        mod.subprocess, "run", lambda cmd, **kw: (seen.append(cmd), _Proc())[1]
    )
    return seen


def test_bound_node_is_refreshed(calls, tmp_path):
    plan = tmp_path / "p.md"
    mod._sync_graph_status("x-abcd", plan)
    assert calls == [["fno", "backlog", "update", "x-abcd", "--plan-path", str(plan)]]


@pytest.mark.parametrize("node_id", [None, "", "   ", 42, {"id": "x-a"}])
def test_unbound_design_doc_touches_nothing(calls, tmp_path, node_id):
    """A design doc with no node yet has nothing on the graph to sync."""
    mod._sync_graph_status(node_id, tmp_path / "p.md")
    assert calls == []


def test_missing_cli_is_a_silent_noop(monkeypatch, tmp_path):
    monkeypatch.setattr(mod.shutil, "which", lambda _: None)

    def explode(*a, **k):  # pragma: no cover - must never be reached
        raise AssertionError("must not shell out when fno is absent")

    monkeypatch.setattr(mod.subprocess, "run", explode)
    mod._sync_graph_status("x-abcd", tmp_path / "p.md")


def test_refresh_failure_never_fails_the_written_doc(monkeypatch, capsys, tmp_path):
    """The doc is already durably written; a refresh error only warns."""
    monkeypatch.setattr(mod.shutil, "which", lambda _: "/usr/bin/fno")

    def boom(*a, **k):
        raise OSError("graph locked")

    monkeypatch.setattr(mod.subprocess, "run", boom)
    mod._sync_graph_status("x-abcd", tmp_path / "p.md")  # must not raise
    assert "graph status refresh failed" in capsys.readouterr().err


def test_nonzero_exit_warns_with_the_node_id(monkeypatch, capsys, tmp_path):
    class _Proc:
        returncode = 1
        stderr = "no such node"

    monkeypatch.setattr(mod.shutil, "which", lambda _: "/usr/bin/fno")
    monkeypatch.setattr(mod.subprocess, "run", lambda *a, **k: _Proc())
    mod._sync_graph_status("x-abcd", tmp_path / "p.md")
    err = capsys.readouterr().err
    assert "x-abcd" in err and "no such node" in err
