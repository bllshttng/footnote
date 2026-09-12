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


def _pin(monkeypatch, tmp_path: Path, rows: list[dict], session: str = "s-test") -> None:
    """Point the verb at a fixture transcript through the env pins."""
    tp = _transcript(tmp_path, rows)
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", session)
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(tp))


def _user_row(text, uuid: str, ts: str = "2026-09-06T21:00:00.000Z") -> dict:
    return {
        "type": "user",
        "uuid": uuid,
        "timestamp": ts,
        "message": {"role": "user", "content": text},
    }


def test_naive_timestamp_reads_as_utc_not_local(tmp_path, tmp_ledger, monkeypatch):
    """A naive transcript timestamp must not skew the age by the local offset."""
    from datetime import datetime, timezone

    from fno.inbox import operator_turns as ot

    _pin(monkeypatch, tmp_path, [_user_row("ask", "u-naive", ts="2026-09-06T21:00:00.000000")])
    result = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert result.exit_code == 0, result.output
    rows = json.loads(result.stdout)
    expected = datetime(2026, 9, 6, 21, 0, tzinfo=timezone.utc).timestamp()
    assert rows[0]["ts_epoch"] == expected


def test_duplicate_rows_derive_distinct_ids(tmp_path, tmp_ledger, monkeypatch):
    """Two id-less identical rows get distinct derived ids, so one ack disposes one turn."""
    tp = _transcript(
        tmp_path,
        [
            {"type": "user", "timestamp": "2026-09-06T21:00:00.000Z",
             "message": {"role": "user", "content": "same text"}},
            {"type": "user", "timestamp": "2026-09-06T21:00:00.000Z",
             "message": {"role": "user", "content": "same text"}},
        ],
    )
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "s-test")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(tp))
    result = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert result.exit_code == 0, result.output
    rows = json.loads(result.stdout)
    assert len(rows) == 2
    assert rows[0]["turn_id"] != rows[1]["turn_id"]


def test_tail_window_drops_no_prose_turn(tmp_path, tmp_ledger, monkeypatch):
    """A transcript larger than the tail window still reads its newest turns cleanly."""
    from fno.inbox import operator_turns as ot

    tp = tmp_path / "transcript.jsonl"
    pad = b'{"type":"user","uuid":"pad"}\n' * (ot._TAIL_BYTES // 28 + 1)
    tp.write_bytes(
        _transcript(tmp_path, [_user_row("old turn beyond the window", "u-old",
                                         ts="2026-09-01T00:00:00.000Z")]).read_bytes()
        + pad
        + _transcript(tmp_path, [_user_row("fresh turn inside the window", "u-new",
                                          ts="2026-09-06T21:00:00.000Z")]).read_bytes()
    )
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "s-test")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(tp))
    result = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert result.exit_code == 0, result.output
    rows = json.loads(result.stdout)
    assert [r["turn_id"] for r in rows] == ["u-new"]


# -- the classifier and the queue --


def test_prose_turn_queues_and_mail_turn_does_not(tmp_path, tmp_ledger, monkeypatch):
    """AC: a fixture holding one prose and one <fno_mail> turn lists exactly the prose turn."""
    _pin(
        monkeypatch,
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
    result = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert result.exit_code == 0, result.output
    rows = json.loads(result.stdout)
    assert [r["turn_id"] for r in rows] == ["u-prose-1"]


def test_bare_command_and_system_only_turns_never_queue(tmp_path, tmp_ledger, monkeypatch):
    """A bare slash command, a bare $fno: verb, and system-reminder-only content are not turns."""
    _pin(
        monkeypatch,
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
    result = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout) == []


def test_command_with_following_prose_still_queues(tmp_path, tmp_ledger, monkeypatch):
    """A slash invocation carrying prose after it is an operator turn (fail toward the queue)."""
    _pin(
        monkeypatch,
        tmp_path,
        [_user_row("/fno:target x-1. A plan already exists at /tmp/plan.md, execute it", "u-arg-1")],
    )
    result = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert result.exit_code == 0, result.output
    assert [r["turn_id"] for r in json.loads(result.stdout)] == ["u-arg-1"]


def test_status_counts_pending_and_ack_disposes(tmp_path, tmp_ledger, monkeypatch):
    """AC: depth reads 3 with zero acks; after one ack it reads 2 and the ledger holds a row."""
    _pin(
        monkeypatch,
        tmp_path,
        [
            _user_row("first ask", "u-1", ts="2026-09-06T20:00:00.000Z"),
            _user_row("second ask", "u-2", ts="2026-09-06T21:00:00.000Z"),
            _user_row("third ask", "u-3", ts="2026-09-06T22:00:00.000Z"),
        ],
    )
    result = runner.invoke(app, ["inbox", "user", "status", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["depth"] == 3
    assert payload["oldest_turn_id"] == "u-1"

    ack = runner.invoke(
        app, ["inbox", "user", "ack", "u-1", "--outcome", "nothing"]
    )
    assert ack.exit_code == 0, ack.output

    result = runner.invoke(app, ["inbox", "user", "status", "--json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["depth"] == 2
    ledger = tmp_ledger / "s-test.jsonl"
    rows = [json.loads(ln) for ln in ledger.read_text().splitlines() if ln.strip()]
    assert len(rows) == 1
    assert rows[0]["turn_id"] == "u-1"
    assert rows[0]["outcome"] == "nothing"


def test_ack_with_ref_names_the_artifact(tmp_path, tmp_ledger, monkeypatch):
    _pin(monkeypatch, tmp_path, [_user_row("record this as law", "u-law-1")])
    ack = runner.invoke(
        app,
        ["inbox", "user", "ack", "u-law-1", "--outcome", "law:da1b2c3d", "--why", "operator said so"],
    )
    assert ack.exit_code == 0, ack.output
    row = json.loads(ack.stdout)
    assert row["outcome"] == "law:da1b2c3d"
    assert row["ref"] == "da1b2c3d"


def test_invalid_outcome_refused_naming_legal_values(tmp_path, tmp_ledger, monkeypatch):
    """AC: a nonsense --outcome exits non-zero and names the legal forms."""
    _pin(monkeypatch, tmp_path, [_user_row("ask", "u-x")])
    ack = runner.invoke(app, ["inbox", "user", "ack", "u-x", "--outcome", "nonsense"])
    assert ack.exit_code != 0
    assert "law:" in ack.output and "capture:" in ack.output and "node:" in ack.output


def test_status_without_session_refuses(tmp_path, tmp_ledger, monkeypatch):
    """AC: no resolvable session exits non-zero and names what it read, never depth 0."""
    monkeypatch.delenv("FNO_OPERATOR_SESSION_ID", raising=False)
    monkeypatch.delenv("FNO_OPERATOR_TRANSCRIPT", raising=False)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    result = runner.invoke(app, ["inbox", "user", "status", "--json"])
    assert result.exit_code != 0
    assert "session" in result.output


def test_status_with_missing_transcript_refuses(tmp_path, tmp_ledger, monkeypatch):
    """AC: an unreadable transcript exits non-zero and names the store, never depth 0."""
    absent = tmp_path / "absent.jsonl"
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "s-ghost")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(absent))
    result = runner.invoke(app, ["inbox", "user", "status", "--json"])
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


def test_operator_spelling_still_reaches_the_queue(tmp_path, tmp_ledger, monkeypatch):
    """The pre-rename `fno inbox operator` spelling is a hidden alias, not a removal."""
    _pin(monkeypatch, tmp_path, [_user_row("old spelling", "u-alias-1")])
    result = runner.invoke(app, ["inbox", "operator", "status", "--json"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["depth"] == 1


# -- machine shapes never queue --


def test_machine_shapes_never_queue_and_are_counted(tmp_path, tmp_ledger, monkeypatch):
    """AC: one row per measured machine shape plus prose queues only the prose,
    and the skip line names every refused shape and count."""
    _pin(
        monkeypatch,
        tmp_path,
        [
            _user_row(
                "<task-notification><task-id>b55cj2z2z</task-id>"
                "<output-file>/tmp/out</output-file></task-notification>",
                "u-tn",
            ),
            _user_row(
                'Another Claude session sent a message:\n'
                '<teammate-message teammate_id="t1" color="blue">{}</teammate-message>',
                "u-tm",
            ),
            _user_row(
                "This session is being continued from a previous conversation "
                "that ran out of context. The summary below covers the work.",
                "u-cp",
            ),
            _user_row("[Request interrupted by user]", "u-int1"),
            _user_row("[Request interrupted by user for tool use]", "u-int2"),
            _user_row("<bash-input>git status</bash-input>", "u-bi"),
            _user_row("<bash-stdout>nothing to commit</bash-stdout>", "u-bs"),
            _user_row(
                "<command-message>fno:target</command-message>\n"
                "<command-name>/fno:target</command-name>",
                "u-cm",
            ),
            _user_row("status on your nodes?", "u-prose"),
        ],
    )
    result = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert result.exit_code == 0, result.output
    assert [r["turn_id"] for r in json.loads(result.stdout)] == ["u-prose"]
    assert "skipped 8 machine turn(s)" in result.output
    for shape in (
        "task_notification=1",
        "teammate_message=1",
        "compaction_preamble=1",
        "interrupt_marker=2",
        "bash_echo=2",
        "command_invocation=1",
    ):
        assert shape in result.output


def test_status_depth_excludes_machine_turns_and_carries_skipped(tmp_path, tmp_ledger, monkeypatch):
    """AC: a task-notification turn does not raise depth; the JSON names the skip."""
    _pin(
        monkeypatch,
        tmp_path,
        [
            _user_row(
                "<task-notification><task-id>t9</task-id></task-notification>",
                "u-tn",
                ts="2026-09-06T20:30:00.000Z",
            ),
            _user_row("status on your nodes?", "u-real", ts="2026-09-06T21:00:00.000Z"),
        ],
    )
    result = runner.invoke(app, ["inbox", "user", "status", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["depth"] == 1
    assert payload["oldest_turn_id"] == "u-real"
    assert payload["skipped"] == {"task_notification": 1}


def test_status_human_path_names_skips_even_at_depth_zero(tmp_path, tmp_ledger, monkeypatch):
    """A queue that is all machine noise reads depth 0 AND says what was skipped."""
    _pin(
        monkeypatch,
        tmp_path,
        [
            _user_row("<task-notification><task-id>t1</task-id></task-notification>", "u-tn"),
            _user_row("[Request interrupted by user]", "u-int"),
        ],
    )
    result = runner.invoke(app, ["inbox", "user", "status"])
    assert result.exit_code == 0, result.output
    assert "user queue: 0" in result.output
    assert "skipped 2 machine turn(s)" in result.output
    assert "interrupt_marker=1" in result.output
    assert "task_notification=1" in result.output
