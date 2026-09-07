"""Unit tests for the operator capture path.

Covers ``fno inbox operator`` (the derived queue, the classifier, the ack
ledger) and the ``--source-kind operator_request`` writer surface on the
graph side (``idea``, ``new``, ``capture promote``, ``find``).
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


@pytest.fixture
def tmp_ledger(tmp_path, monkeypatch) -> Path:
    d = tmp_path / "operator-capture"
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(d))
    return d


def _transcript(tmp_path: Path, rows: list[dict]) -> Path:
    p = tmp_path / "transcript.jsonl"
    p.write_text("\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8")
    return p


def _user_row(text, uuid: str, ts: str = "2026-09-06T21:00:00.000Z") -> dict:
    return {
        "type": "user",
        "uuid": uuid,
        "timestamp": ts,
        "message": {"role": "user", "content": text},
    }


# -- the classifier and the queue --


def test_prose_turn_queues_and_mail_turn_does_not(tmp_path, tmp_ledger):
    """AC: a fixture holding one prose and one <fno_mail> turn lists exactly the prose turn."""
    tp = _transcript(
        tmp_path,
        [
            _user_row("please widen the review gate", "u-prose-1"),
            _user_row(
                ['<fno_mail from="peer" harness="claude">run the sweep</fno_mail>'],
                "u-mail-1",
                ts="2026-09-06T21:01:00.000Z",
            ),
        ],
    )
    result = runner.invoke(
        app,
        ["inbox", "operator", "list", "--json", "--session-id", "s-test", "--transcript", str(tp)],
    )
    assert result.exit_code == 0, result.output
    rows = json.loads(result.stdout)
    assert [r["turn_id"] for r in rows] == ["u-prose-1"]


def test_bare_command_and_system_only_turns_never_queue(tmp_path, tmp_ledger):
    """A bare slash command, a bare $fno: verb, and system-reminder-only content are not turns."""
    tp = _transcript(
        tmp_path,
        [
            _user_row("/fno:setup", "u-cmd-1"),
            _user_row("$fno:review medium", "u-cmd-2"),
            _user_row(
                [{"type": "text", "text": "<system-reminder>hook output</system-reminder>"}],
                "u-hook-1",
            ),
            _user_row(
                [{"type": "tool_result", "tool_use_id": "t1", "content": "out"}],
                "u-tool-1",
            ),
        ],
    )
    result = runner.invoke(
        app,
        ["inbox", "operator", "list", "--json", "--session-id", "s-test", "--transcript", str(tp)],
    )
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout) == []


def test_command_with_following_prose_still_queues(tmp_path, tmp_ledger):
    """A slash invocation carrying prose after it is an operator turn (fail toward the queue)."""
    tp = _transcript(
        tmp_path,
        [_user_row("/fno:target x-1. A plan already exists at /tmp/plan.md, execute it", "u-arg-1")],
    )
    result = runner.invoke(
        app,
        ["inbox", "operator", "list", "--json", "--session-id", "s-test", "--transcript", str(tp)],
    )
    assert result.exit_code == 0, result.output
    assert [r["turn_id"] for r in json.loads(result.stdout)] == ["u-arg-1"]


def test_status_counts_pending_and_ack_disposes(tmp_path, tmp_ledger):
    """AC: depth reads 3 with zero acks; after one ack it reads 2 and the ledger holds a row."""
    tp = _transcript(
        tmp_path,
        [
            _user_row("first ask", "u-1", ts="2026-09-06T20:00:00.000Z"),
            _user_row("second ask", "u-2", ts="2026-09-06T21:00:00.000Z"),
            _user_row("third ask", "u-3", ts="2026-09-06T22:00:00.000Z"),
        ],
    )
    args = ["inbox", "operator", "status", "--json", "--session-id", "s-test", "--transcript", str(tp)]
    result = runner.invoke(app, args)
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["depth"] == 3
    assert payload["oldest_turn_id"] == "u-1"

    ack = runner.invoke(
        app,
        ["inbox", "operator", "ack", "u-1", "--outcome", "nothing", "--session-id", "s-test"],
    )
    assert ack.exit_code == 0, ack.output

    result = runner.invoke(app, args)
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["depth"] == 2
    ledger = tmp_ledger / "s-test.jsonl"
    rows = [json.loads(ln) for ln in ledger.read_text().splitlines() if ln.strip()]
    assert len(rows) == 1
    assert rows[0]["turn_id"] == "u-1"
    assert rows[0]["outcome"] == "nothing"


def test_ack_with_ref_names_the_artifact(tmp_path, tmp_ledger):
    tp = _transcript(tmp_path, [_user_row("record this as law", "u-law-1")])
    ack = runner.invoke(
        app,
        [
            "inbox", "operator", "ack", "u-law-1",
            "--outcome", "law:da1b2c3d",
            "--why", "operator said so",
            "--session-id", "s-test",
        ],
    )
    assert ack.exit_code == 0, ack.output
    row = json.loads(ack.stdout)
    assert row["outcome"] == "law:da1b2c3d"
    assert row["ref"] == "da1b2c3d"


def test_invalid_outcome_refused_naming_legal_values(tmp_path, tmp_ledger):
    """AC: a nonsense --outcome exits non-zero and names the legal forms."""
    ack = runner.invoke(
        app,
        ["inbox", "operator", "ack", "u-x", "--outcome", "nonsense", "--session-id", "s-test"],
    )
    assert ack.exit_code != 0
    assert "law:" in ack.output and "capture:" in ack.output and "node:" in ack.output


def test_status_without_session_refuses(tmp_path, tmp_ledger, monkeypatch):
    """AC: no resolvable session exits non-zero and names what it read, never depth 0."""
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    result = runner.invoke(app, ["inbox", "operator", "status", "--json"])
    assert result.exit_code != 0
    assert "session" in result.output


def test_status_with_missing_transcript_refuses(tmp_path, tmp_ledger):
    """AC: an unreadable transcript exits non-zero and names the store, never depth 0."""
    result = runner.invoke(
        app,
        [
            "inbox", "operator", "status", "--json",
            "--session-id", "s-ghost",
            "--transcript", str(tmp_path / "absent.jsonl"),
        ],
    )
    assert result.exit_code != 0
    assert "transcript" in result.output
    assert "s-ghost" in result.output


# -- the source_kind writer surface --


def _entries(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def test_idea_operator_request_reads_back(tmp_graph):
    """AC: idea --source-kind operator_request lands a node the field reads operator_request."""
    result = runner.invoke(
        app,
        ["backlog", "idea", "operator asked for a capture path",
         "--source-kind", "operator_request", "--difficulty", "low"],
    )
    assert result.exit_code == 0, result.output
    (node,) = _entries(tmp_graph)
    assert node["source_kind"] == "operator_request"
    nid = node["id"]

    read = runner.invoke(app, ["backlog", "get", nid, "--field", "source_kind"])
    assert read.exit_code == 0, read.output
    assert "operator_request" in read.stdout


def test_idea_rejects_unknown_source_kind(tmp_graph):
    """AC: an out-of-vocabulary --source-kind exits non-zero naming the five legal values."""
    result = runner.invoke(
        app,
        ["backlog", "idea", "x", "--source-kind", "nonsense", "--difficulty", "low"],
    )
    assert result.exit_code != 0
    for value in ("organic", "from_inbox", "from_observation", "from_supervisor", "operator_request"):
        assert value in result.output


def test_new_operator_request_via_shared_builder(tmp_graph):
    """The collapsed `new` writer carries the field through the shared builder."""
    result = runner.invoke(
        app,
        ["backlog", "new", "inbox-fed item", "--source-kind", "from_inbox",
         "--source-inbox-msg", "msg-a4f1b2", "--force-domain"],
    )
    assert result.exit_code == 0, result.output
    (node,) = _entries(tmp_graph)
    assert node["source_kind"] == "from_inbox"
    assert node["source_inbox_msg"] == "msg-a4f1b2"
    assert node["source"] == "fno-new"


def test_capture_promote_carries_source_kind(tmp_graph, tmp_path, monkeypatch):
    """AC: capture promote --source-kind keeps the item's origin on the minted node."""
    inbox = tmp_path / "inbox.md"
    inbox.write_text("- [ ] fu-aa11bb - widen the review gate (p2)\n", encoding="utf-8")
    monkeypatch.setattr("fno.backlog.capture._inbox_path", lambda: inbox)
    result = runner.invoke(
        app,
        ["backlog", "capture", "promote", "fu-aa11bb",
         "--difficulty", "low", "--source-kind", "operator_request"],
    )
    assert result.exit_code == 0, result.output
    (node,) = _entries(tmp_graph)
    assert node["source_kind"] == "operator_request"


def test_find_filters_by_source_kind(tmp_graph):
    """AC: find --source-kind operator_request returns only nodes carrying that value."""
    import fno.graph.store as gs

    def seed(entries):
        entries.append({"id": "ab-opr000001", "title": "operator ask", "status": "idea",
                        "source_kind": "operator_request"})
        entries.append({"id": "ab-org000001", "title": "worker idea", "status": "idea"})
        return entries

    gs.locked_mutate_graph(tmp_graph, seed)

    result = runner.invoke(
        app,
        ["backlog", "find", "ask", "--source-kind", "operator_request", "--json"],
    )
    assert result.exit_code == 0, result.output
    rows = json.loads(result.stdout)
    assert [r["id"] for r in rows] == ["ab-opr000001"]
