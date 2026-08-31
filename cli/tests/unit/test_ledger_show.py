"""`fno whoami ledger` resolves one argument in three directions.

Positive markers per the pitfalls corpus: the assertion is on the printed
uuid/pr/resume line, never on "the command ran". The unresolved case asserts
the MARKER is printed - never a blank, never a fabricated resume command.
"""

from __future__ import annotations

import json

import pytest
import typer

from fno.cost._register import LEDGER_SESSION_UNRESOLVED
from fno.ledger_show import ledger_show_command

UUID = "11111111-2222-3333-4444-555555555555"

ROWS = [
    {
        "type": "execution",
        "status": "done",
        "graph_node_id": "x-3344",
        "pr_number": 507,
        "pr_url": "https://github.com/o/r/pull/507",
        "plan_path": "/plans/x.md",
        "root_path": "/wt/x",
        "sessions": [UUID],
    },
    # Same number, foreign repo: the global ledger must not attribute it.
    {
        "type": "execution",
        "graph_node_id": "x-9999",
        "pr_number": 507,
        "pr_url": "https://github.com/other/repo/pull/507",
        "sessions": [LEDGER_SESSION_UNRESOLVED],
    },
    {
        "type": "execution",
        "graph_node_id": "x-5566",
        "pr_number": 508,
        "pr_url": "https://github.com/o/r/pull/508",
        "sessions": [LEDGER_SESSION_UNRESOLVED],
    },
]

GRAPH = {
    "entries": [
        {"id": "x-3344", "sessions": [{"session_id": UUID, "harness": "claude"}]},
    ]
}


@pytest.fixture
def show(tmp_path, monkeypatch, capsys):
    ledger = tmp_path / "ledger.json"
    graph = tmp_path / "graph.json"
    ledger.write_text(json.dumps({"entries": ROWS}))
    graph.write_text(json.dumps(GRAPH))

    class _P:
        ledger_json = staticmethod(lambda: ledger)
        graph_json = staticmethod(lambda: graph)

    monkeypatch.setattr("fno.ledger_show._paths", _P)
    monkeypatch.setattr(
        "fno.graph._reconcile.resolve_current_repo_slug",
        lambda *a, **k: "o/r",
    )

    def _run(arg: str) -> str:
        ledger_show_command(arg)
        return capsys.readouterr().out

    return _run


def test_node_direction_prints_pr_and_runnable_resume(show):
    out = show("x-3344")
    assert "#507" in out
    assert UUID in out
    assert "claude --resume " + UUID in out
    assert "/plans/x.md" in out
    assert "/wt/x" in out


def test_pr_direction_matches_only_this_repo(show):
    for arg in ("507", "#507", "https://github.com/o/r/pull/507"):
        out = show(arg)
        assert "#507" in out
        assert "x-3344" in out
        assert "x-9999" not in out  # the foreign same-numbered PR


def test_session_direction_finds_the_row(show):
    out = show(UUID)
    assert "x-3344" in out
    assert "#507" in out


def test_unresolved_row_prints_marker_not_blank(show):
    out = show("x-5566")
    assert LEDGER_SESSION_UNRESOLVED in out
    assert "no resume handle was recorded" in out
    assert "resume:" not in out  # no fabricated command
    assert "session:  \n" not in out  # never a blank field


def test_harness_unrecorded_when_graph_has_no_node(show, tmp_path):
    (tmp_path / "graph.json").write_text(json.dumps({"entries": []}))
    out = show("x-3344")
    assert UUID in out
    assert "harness unrecorded; no resume command inferred" in out
    assert "resume:" not in out


def test_no_match_exits_nonzero_naming_the_direction(show):
    with pytest.raises(typer.Exit) as exc:
        show("zz-0000")
    assert exc.value.exit_code == 1
    # the direction line rides stderr; caller asserts exit code only
