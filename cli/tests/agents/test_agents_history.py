"""`fno agents history` - one verb over live rows, reap receipts, ledger.

Positive markers per the pitfalls corpus: assertions land on the printed
field lines, never on "the command ran". The receipt's resume line is
asserted byte-identical to a form the capability table does NOT declare, so
a re-derivation fails the test instead of passing as a match.

Fixtures mirror the real ReapReceipt shape written by
crates/fno-agents/src/daemon.rs (row_name, short_id, harness,
harness_session_id, cwd, log_path, created_at, reaped_at, resume, optional
ledger enrichment) - a receipt the Rust writer would not produce is not a
fixture this reader deserves.
"""

from __future__ import annotations

import json
import re

import pytest
import typer
from typer.testing import CliRunner

from fno.agents.history import history_command
from fno.agents.registry import AgentEntry

_SID = "11111111-2222-3333-4444-555555555555"
_REAPED_SID = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

# Not any harness's declared interactive_resume form: if the verb re-derived
# the command from the capability table, this byte-exact assert would fail.
_REASUME_VERBATIM = "harness-quine --revive " + _REAPED_SID


def _receipt(node: str | None = None, sid: str = _REAPED_SID) -> dict:
    receipt = {
        "row_name": "t-x6db9-worker",
        "short_id": "tx6db9wor",
        "harness": "claude",
        "harness_session_id": sid,
        "cwd": "/repo/wt",
        "log_path": "/repo/wt/.fno/log",
        "created_at": "2026-08-25T10:00:00Z",
        "reaped_at": "2026-08-26T10:00:00Z",
        "resume": _REASUME_VERBATIM,
    }
    if node is not None:
        receipt["ledger"] = {"graph_node_id": node, "pr_number": 507}
    return receipt


ROWS = [
    {
        "type": "execution",
        "status": "done",
        "graph_node_id": "x-3344",
        "pr_number": 507,
        "pr_url": "https://github.com/o/r/pull/507",
        "plan_path": "/plans/x.md",
        "root_path": "/wt/x",
        "sessions": [_SID],
        "provider": "zai",
        "model": "glm-5.3[1m]",
    },
]

GRAPH = {
    "entries": [
        {"id": "x-3344", "sessions": [{"session_id": _SID, "harness": "claude"}]},
    ]
}


def _paths(tmp_path, rows: list[dict], receipts: list[dict]):
    ledger = tmp_path / "ledger.json"
    graph = tmp_path / "graph.json"
    ledger.write_text(json.dumps({"entries": rows}))
    graph.write_text(json.dumps(GRAPH))
    home = tmp_path / "agents-home"
    if receipts:
        (home / "reap-receipts").mkdir(parents=True)
        for receipt in receipts:
            name = f"{receipt['harness']}-{receipt['harness_session_id']}.json"
            (home / "reap-receipts" / name).write_text(json.dumps(receipt))

    class _P:
        ledger_json = staticmethod(lambda: ledger)
        graph_json = staticmethod(lambda: graph)
        agents_home_dir = staticmethod(lambda: home)

    return _P


@pytest.fixture
def history(tmp_path, monkeypatch, capsys):
    def _install(rows=ROWS, receipts: list[dict] | None = None, entries=None, graph=None):
        if graph is not None:
            (tmp_path / "graph.json").write_text(json.dumps(graph))
        monkeypatch.setattr(
            "fno.agents.history._paths", _paths(tmp_path, rows, receipts or [])
        )
        monkeypatch.setattr(
            "fno.agents.registry.load_registry", lambda path=None: entries or []
        )

        def _run(arg: str) -> str:
            code = 0
            try:
                history_command(arg)
            except typer.Exit as exc:
                code = exc.exit_code
            out = capsys.readouterr().out
            return out + f"\nEXIT={code}"

        return _run

    return _install


def test_receipt_session_prints_resume_verbatim(history):
    run = history(receipts=[_receipt()])
    out = run(_REAPED_SID)
    assert "resume:   " + _REASUME_VERBATIM in out
    assert "harness:  claude" in out
    assert "cwd:      /repo/wt" in out
    assert "created_at:  2026-08-25T10:00:00Z" in out
    assert "reaped_at:   2026-08-26T10:00:00Z" in out
    assert "EXIT=0" in out


def test_node_finds_receipt_through_ledger_enrichment(history):
    # No ledger row for the node at all: the receipt's own enrichment is
    # the join from work to session.
    run = history(receipts=[_receipt(node="x-9f2e")])
    out = run("x-9f2e")
    assert _REAPED_SID in out
    assert "resume:   " + _REASUME_VERBATIM in out
    assert "EXIT=0" in out


def test_node_finds_receipt_even_when_the_ledger_is_unreadable(history, tmp_path):
    # A broken ledger must not downgrade a node arg to session kind: the
    # receipt's own enrichment still answers, and the receipt section must
    # never print a false absence for a file that is on disk.
    (tmp_path / "ledger.json").write_text("{not json")
    run = history(receipts=[_receipt(node="x-9f2e")])
    out = run("x-9f2e")
    assert "resume:   " + _REASUME_VERBATIM in out
    assert "no reap receipt on disk" not in out


def test_coverage_notes_survive_unhashable_session_elements(history, tmp_path):
    # A corrupt row's sessions list can hold anything; the coverage notes
    # degrade to 'not recorded' instead of crashing the whole verb.
    corrupt = [dict(ROWS[0])]
    corrupt[0] = {k: v for k, v in corrupt[0].items() if k != "sessions"}
    corrupt[0]["sessions"] = [{"bad": 1}]
    run = history(rows=corrupt)
    out = run("x-3344")
    assert "EXIT=0" in out


def test_ledger_fields_match_whoami_ledger(history):
    run = history()
    out = run("x-3344")
    assert "node:     x-3344" in out
    assert "#507" in out
    assert "/plans/x.md" in out
    assert "/wt/x" in out
    assert "status:   done" in out
    assert "provider: zai" in out


def test_live_row_reports_and_suppresses_its_receipt(history, tmp_path):
    entry = AgentEntry(
        name="t-live",
        cwd="/repo/live",
        log_path="/repo/live/.fno/log",
        harness="claude",
        harness_session_id=_SID,
    )
    run = history(
        receipts=[_receipt(sid=_SID)],
        entries=[entry],
    )
    out = run(_SID)
    assert "live:" in out
    assert "name:     t-live" in out
    assert "no receipt is expected" in out
    assert "from:     " not in out  # the stale receipt is not reported


def test_total_miss_names_all_three_sources_and_exits_1(history):
    run = history()
    out = run("zz-not-a-thing")
    assert "not recorded" in out
    assert "no live row" in out
    assert "no reap receipt on disk" in out
    assert "no ledger row" in out
    assert "EXIT=1" in out


def test_legacy_row_session_says_not_recorded(history, tmp_path):
    legacy = [dict(ROWS[0])]
    legacy[0] = {k: v for k, v in legacy[0].items() if k != "sessions"}
    run = history(rows=legacy)
    out = run("x-3344")
    assert "not recorded (ledger uuid coverage is write-path only; this row predates it)" in out
    assert "node:     x-3344" in out
    assert "#507" in out


def test_legacy_node_key_resolves(history, tmp_path):
    legacy = [dict(ROWS[0])]
    legacy[0] = {k: v for k, v in legacy[0].items() if k != "graph_node_id"}
    legacy[0]["node"] = "x-3344"
    legacy[0]["node_id_unrecoverable"] = False
    run = history(rows=legacy)
    out = run("x-3344")
    assert "node:     x-3344" in out


def test_node_id_unrecoverable_named_not_dashed(history, tmp_path):
    legacy = [dict(ROWS[0])]
    legacy[0] = {k: v for k, v in legacy[0].items() if k != "graph_node_id"}
    legacy[0]["node_id_unrecoverable"] = True
    run = history(rows=legacy)
    out = run(_SID)
    assert "node:     not recorded (this row says node_id_unrecoverable)" in out


def test_coverage_notes_name_missing_fields(history):
    run = history()
    out = run("x-3344")
    assert "provider: zai" in out
    assert "model:    glm-5.3[1m]" in out
    bare = [dict(ROWS[0])]
    bare[0] = {k: v for k, v in bare[0].items() if k not in ("provider", "model")}
    run2 = history(rows=bare)
    out2 = run2("x-3344")
    assert "provider: not recorded" in out2
    assert "model:    not recorded" in out2


# --- AC7: the alias is hidden from help and still resolves -----------------


_ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def test_whoami_ledger_hidden_but_working(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    graph = tmp_path / "graph.json"
    ledger.write_text(json.dumps({"entries": ROWS}))
    graph.write_text(json.dumps(GRAPH))

    class _P:
        ledger_json = staticmethod(lambda: ledger)
        graph_json = staticmethod(lambda: graph)

    monkeypatch.setattr("fno.ledger_show._paths", _P)
    from fno.agent.cli import whoami_app

    runner = CliRunner()
    help_text = _ANSI_RE.sub("", runner.invoke(whoami_app, ["--help"]).output)
    assert "ledger" not in help_text
    result = runner.invoke(whoami_app, ["ledger", "x-3344"])
    assert result.exit_code == 0
    assert "#507" in result.output


def test_agents_help_advertises_history():
    from fno.agents.cli import agents_app

    runner = CliRunner()
    help_text = _ANSI_RE.sub("", runner.invoke(agents_app, ["--help"]).output)
    assert "history" in help_text
