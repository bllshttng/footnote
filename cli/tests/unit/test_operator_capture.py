"""Unit tests for the operator capture path.

Covers ``fno inbox operator`` (the derived queue, the classifier, the ack
ledger) and the ``--source-kind operator_request`` writer surface on the
graph side (``idea``, ``new``, ``capture promote``, ``find``).
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

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
    seed_graph(g, '{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    # The native read-backs resolve the store through FNO_CONFIG's state_dir.
    (tmp_path / "config.toml").write_text(f'state_dir = "{tmp_path}"\n')
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "config.toml"))
    return g


def _native_backlog(*args: str) -> tuple[int, str, str]:
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    proc = _sp.run(
        [str(binary), "backlog", *args],
        capture_output=True,
        text=True,
        env={**_os.environ, "FNO_TRACKER_BACKEND": "graph"},
    )
    return proc.returncode, proc.stdout, proc.stderr


@pytest.fixture
def tmp_ledger(tmp_path, monkeypatch) -> Path:
    d = tmp_path / "operator-capture"
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(d))
    return d


def _transcript(tmp_path: Path, rows: list[dict]) -> Path:
    p = tmp_path / "transcript.jsonl"
    p.write_text("\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8")
    return p


@pytest.fixture
def operator_turn(tmp_path: Path, monkeypatch, native_backlog_door):
    transcript = _transcript(tmp_path, [{
        "type": "user", "uuid": "turn-1", "timestamp": "2026-09-24T00:00:00Z",
        "message": {"role": "user", "content": "status on your nodes?"},
    }])
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "fixture-session")
    monkeypatch.setenv("FNO_OPERATOR_HARNESS", "claude")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(transcript))
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(tmp_path / "operator-capture"))


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
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)


def test_idea_operator_request_reads_back(tmp_graph, operator_turn):
    """AC: idea --source-kind operator_request lands a node the field reads operator_request."""
    result = runner.invoke(
        app,
        ["backlog", "idea", "operator asked for a capture path",
         "--source-kind", "operator_request", "--difficulty", "low"],
    )
    assert result.exit_code == 0, result.output
    (node,) = _entries(tmp_graph)
    assert node["source_kind"] == "operator_request"
    assert node["request_origin"] == "operator_request"
    nid = node["id"]

    read_code, read_out, read_err = _native_backlog("get", nid, "--field", "source_kind")
    assert read_code == 0, read_err
    assert "operator_request" in read_out


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


def test_capture_promote_carries_source_kind(tmp_graph, tmp_path, monkeypatch, operator_turn):
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


def test_operator_request_refuses_unreadable_queue(
    tmp_graph, tmp_path, monkeypatch, native_backlog_door
):
    absent = tmp_path / "missing-transcript.jsonl"
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "fixture-session")
    monkeypatch.setenv("FNO_OPERATOR_HARNESS", "claude")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(absent))
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(tmp_path / "operator-capture"))
    refused = runner.invoke(
        app,
        ["backlog", "idea", "rejected ask", "--source-kind", "operator_request", "--difficulty", "low"],
    )
    assert refused.exit_code == 1
    assert str(absent) in refused.output
    assert _entries(tmp_graph) == []
    organic = runner.invoke(
        app, ["backlog", "idea", "organic idea", "--source-kind", "organic", "--difficulty", "low"]
    )
    assert organic.exit_code == 0, organic.output
    assert _entries(tmp_graph)[0]["request_origin"] == "unknown"


def test_operator_request_refuses_empty_queue(
    tmp_graph, tmp_path, monkeypatch, native_backlog_door
):
    empty = tmp_path / "empty-transcript.jsonl"
    empty.write_text("", encoding="utf-8")
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "fixture-session")
    monkeypatch.setenv("FNO_OPERATOR_HARNESS", "claude")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(empty))
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(tmp_path / "operator-capture"))
    refused = runner.invoke(
        app,
        [
            "backlog", "idea", "rejected ask", "--source-kind", "operator_request",
            "--difficulty", "low",
        ],
    )
    assert refused.exit_code == 1
    assert "queue is empty" in refused.output
    assert "as organic" in refused.output
    assert _entries(tmp_graph) == []


def test_operator_request_refuses_when_native_binary_is_unavailable(
    tmp_graph, tmp_path, monkeypatch
):
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "fixture-session")
    monkeypatch.setenv("FNO_OPERATOR_HARNESS", "claude")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(tmp_path / "planted-transcript.jsonl"))
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(tmp_path / "operator-capture"))
    refused = runner.invoke(
        app,
        ["backlog", "idea", "unverified ask", "--source-kind", "operator_request", "--difficulty", "low"],
    )
    assert refused.exit_code == 1
    assert "could not be verified" in refused.output
    assert _entries(tmp_graph) == []


def test_find_filters_by_source_kind(tmp_graph):
    """AC: find --source-kind operator_request returns only nodes carrying that value."""
    import fno.graph.store as gs

    def seed(entries):
        entries.append({"id": "ab-opr000001", "title": "operator ask", "status": "idea",
                        "source_kind": "operator_request"})
        entries.append({"id": "ab-org000001", "title": "worker idea", "status": "idea"})
        return entries

    gs.commit_rows_via_store(tmp_graph, seed)

    find_code, find_out, find_err = _native_backlog(
        "find", "ask", "--source-kind", "operator_request", "--json",
    )
    assert find_code == 0, find_err
    rows = json.loads(find_out)
    assert [r["id"] for r in rows] == ["ab-opr000001"]


def test_queue_verbs_delegate_to_the_rust_reader(tmp_path, tmp_ledger, monkeypatch):
    """AC: both queue verbs hand the resolved ids and paths to the Rust reader door,
    status prints the payload without `turns`, list prints the turns, a set
    `cursor_error` warns on stderr, and a door error exits 1 naming the reader
    instead of ever reading as depth 0."""
    import fno.rust_binary as rb

    captured: dict = {}

    def stub(verb, args=()):
        captured["verb"] = verb
        captured["args"] = args
        return (
            None,
            {
                "depth": 1,
                "oldest_age_s": 42,
                "oldest_excerpt": "hello",
                "oldest_turn_id": "u-1",
                "skipped": {"task_notification": 1},
                "cursor_error": "/ro/scan.json: permission denied",
                "turns": [
                    {"turn_id": "u-1", "ts_epoch": 1.0, "text": "hello", "excerpt": "hello"}
                ],
            },
        )

    monkeypatch.setattr(rb, "call_binary_json", stub)
    _pin(monkeypatch, tmp_path, [_user_row("old spelling", "u-alias-1")])

    status = runner.invoke(app, ["inbox", "user", "status", "--json"])
    assert status.exit_code == 0, status.output
    assert captured["verb"] == "compaction"
    assert captured["args"][:2] == ["operator-turns", "--session"]
    assert "s-test" in captured["args"]
    assert str(tmp_path / "transcript.jsonl") in captured["args"]
    assert str(tmp_ledger) in captured["args"]
    payload = json.loads(status.stdout)
    assert payload["depth"] == 1
    assert "turns" not in payload
    assert payload["cursor_error"] == "/ro/scan.json: permission denied"
    assert "scan cursor not saved" in status.output

    listed = runner.invoke(app, ["inbox", "user", "list", "--json"])
    assert listed.exit_code == 0, listed.output
    rows = json.loads(listed.stdout)
    assert [r["turn_id"] for r in rows] == ["u-1"]
    assert rows[0]["excerpt"] == "hello"
    assert "scan cursor not saved" in listed.output

    monkeypatch.setattr(rb, "call_binary_json", lambda verb, args=(): ("reader gone", None))
    failed = runner.invoke(app, ["inbox", "user", "status", "--json"])
    assert failed.exit_code == 1
    assert "operator turn reader" in failed.output


def test_operator_turn_list_marks_stand_down_rows(tmp_path, tmp_ledger, monkeypatch):
    import fno.rust_binary as rb

    monkeypatch.setattr(
        rb,
        "call_binary_json",
        lambda verb, args=(): (
            None,
            {
                "turns": [
                    {"turn_id": "u-stand", "ts_epoch": 1.0, "excerpt": "overstayed", "stand_down": True},
                    {"turn_id": "u-ordinary", "ts_epoch": 1.0, "excerpt": "keep going", "stand_down": False},
                ]
            },
        ),
    )
    _pin(monkeypatch, tmp_path, [_user_row("overstayed", "u-stand")])

    result = runner.invoke(app, ["inbox", "user", "list"])

    assert result.exit_code == 0, result.output
    lines = result.stdout.splitlines()
    assert lines[0].endswith("[stand-down] overstayed")
    assert lines[1].endswith("keep going")
    assert "[stand-down]" not in lines[1]

    monkeypatch.setattr(
        rb,
        "call_binary_json",
        lambda verb, args=(): (
            None,
            {"turns": [{"turn_id": "u-legacy", "ts_epoch": 1.0, "excerpt": "old binary"}]},
        ),
    )
    legacy = runner.invoke(app, ["inbox", "user", "list"])
    assert legacy.exit_code == 0, legacy.output
    assert "[stand-down]" not in legacy.stdout
