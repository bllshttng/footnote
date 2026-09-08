"""`fno backlog note` delivery: who gets reached, and what a failure must say.

The defect this module closes is a write that succeeds while the communication
fails and nothing reports it. So the tests that matter are the ones asserting a
POSITIVE receipt: every recipient class produces a named line, a failed send
produces a `notify FAILED` line naming the address and the cause, and no path
raises into the caller, because the note is already on disk when this code runs.

Every prover is injected. `claim_status` walks a real lockfile tree and
`resolve_to_king` reads the live agent registry, so reading either for real would
make these tests pass or fail on what else is running on the machine.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.graph.note_notify import note_recipients, notify_note, pointer


def _claims(**by_key: dict) -> "callable":
    return lambda key: by_key.get(key, {"key": key, "state": "free"})


def _live(holder: str) -> dict:
    return {"state": "live", "holder": holder}


def test_holder_then_king_each_once_and_in_order() -> None:
    entry = {"id": "x-0d08", "parent": "x-16b7"}
    got = note_recipients(
        entry,
        index={"x-16b7": {"id": "x-16b7"}},
        claim_reader=_claims(**{"node:x-0d08": _live("sess-worker")}),
        king_resolver=lambda scope: ["king-a", "king-a"],
    )
    assert got == [
        ("sess-worker", "holder of x-0d08"),
        ("king-a", "king of x-16b7"),
    ]


def test_a_contained_note_reaches_the_owners_holder() -> None:
    """A note on x-0d08 is material to whoever is building x-5a62."""
    entry = {"id": "x-0d08", "contained_in": "x-5a62", "parent": "x-16b7"}
    got = note_recipients(
        entry,
        index={"x-5a62": {"id": "x-5a62", "parent": "x-16b7"}},
        claim_reader=_claims(
            **{
                "node:x-0d08": _live("sess-note-owner"),
                "node:x-5a62": _live("sess-builder"),
            }
        ),
        king_resolver=lambda scope: [],
    )
    assert got == [
        ("sess-note-owner", "holder of x-0d08"),
        ("sess-builder", "holder of owner x-5a62"),
    ]


def test_a_suspect_claim_is_still_owned_and_still_reached() -> None:
    got = note_recipients(
        {"id": "x-0d08"},
        index={},
        claim_reader=_claims(**{"node:x-0d08": {"state": "suspect", "holder": "sess-s"}}),
        king_resolver=lambda scope: [],
    )
    assert got == [("sess-s", "holder of x-0d08")]


@pytest.mark.parametrize("state", ["stale", "free", "corrupted"])
def test_an_unowned_claim_reaches_nobody(state: str) -> None:
    got = note_recipients(
        {"id": "x-0d08"},
        index={},
        claim_reader=_claims(**{"node:x-0d08": {"state": state, "holder": "sess-dead"}}),
        king_resolver=lambda scope: [],
    )
    assert got == []


def test_the_author_is_never_mailed_its_own_note() -> None:
    got = note_recipients(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={},
        claim_reader=_claims(**{"node:x-0d08": _live("sess-me")}),
        king_resolver=lambda scope: ["sess-me"],
        self_session="sess-me",
    )
    assert got == []


def test_a_role_prefixed_holder_is_still_recognised_as_self() -> None:
    """`target-session:<id>` is the same session as the bare `<id>` prover."""
    got = note_recipients(
        {"id": "x-0d08"},
        index={},
        claim_reader=_claims(**{"node:x-0d08": _live("target-session:sess-me")}),
        king_resolver=lambda scope: [],
        self_session="sess-me",
    )
    assert got == []


def test_a_raising_king_resolver_costs_the_king_not_the_holder() -> None:
    def boom(scope: str) -> list[str]:
        raise RuntimeError("registry unreadable")

    got = note_recipients(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={},
        claim_reader=_claims(**{"node:x-0d08": _live("sess-worker")}),
        king_resolver=boom,
    )
    assert got == [("sess-worker", "holder of x-0d08")]


def test_the_pointer_carries_the_node_and_truncates_the_note() -> None:
    body = pointer("x-0d08", "the sideline glyph chain is wrong " + "word " * 40)
    assert body.startswith("note on x-0d08: the sideline glyph chain is wrong")
    assert "..." in body
    assert "fno backlog get x-0d08" in body
    assert len(body.split()) < 40  # well under the 80-word pair budget


def _graph(tmp_path: Path, entries: list[dict]) -> Path:
    path = tmp_path / "graph.json"
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return path


def test_a_failed_send_is_reported_and_never_raised(tmp_path: Path) -> None:
    def boom(address: str, body: str) -> str:
        raise RuntimeError("pair budget spent")

    receipts = notify_note(
        "x-0d08",
        "the finding",
        graph_path=_graph(tmp_path, [{"id": "x-0d08"}]),
        sender=boom,
        claim_reader=_claims(**{"node:x-0d08": _live("sess-worker")}),
        king_resolver=lambda scope: [],
        self_session="sess-me",
    )
    assert receipts == [
        "notify FAILED sess-worker (holder of x-0d08): pair budget spent"
    ]


def test_a_delivered_send_reports_the_transport(tmp_path: Path) -> None:
    receipts = notify_note(
        "x-0d08",
        "the finding",
        graph_path=_graph(tmp_path, [{"id": "x-0d08"}]),
        sender=lambda address, body: "hosted msg-abc12345",
        claim_reader=_claims(**{"node:x-0d08": _live("sess-worker")}),
        king_resolver=lambda scope: [],
        self_session="sess-me",
    )
    assert receipts == [
        "notified sess-worker (holder of x-0d08): hosted msg-abc12345"
    ]


def test_nobody_to_reach_returns_no_receipts(tmp_path: Path) -> None:
    sent: list[str] = []
    receipts = notify_note(
        "x-0d08",
        "the finding",
        graph_path=_graph(tmp_path, [{"id": "x-0d08"}]),
        sender=lambda address, body: sent.append(address) or "hosted msg-1",
        claim_reader=_claims(),
        king_resolver=lambda scope: [],
        self_session="sess-me",
    )
    assert receipts == []
    assert sent == []


def test_an_unknown_node_reports_rather_than_raising(tmp_path: Path) -> None:
    receipts = notify_note(
        "x-ffff",
        "the finding",
        graph_path=_graph(tmp_path, [{"id": "x-0d08"}]),
        sender=lambda address, body: "hosted msg-1",
        claim_reader=_claims(),
        king_resolver=lambda scope: [],
        self_session="sess-me",
    )
    assert receipts == ["notify FAILED x-ffff (no such node): nothing to resolve"]


# --- the verb itself: delivery is the default, and --quiet is the opt-out ---


def _run(monkeypatch, argv: list[str], receipts: list[str] | None = None, boom: bool = False):
    """Run `fno backlog note` with the graph write and the send both stubbed."""
    from typer.testing import CliRunner

    from fno.graph import cli as graph_cli
    from fno.graph import note_notify

    monkeypatch.setattr(graph_cli, "_graph_path", lambda *a, **k: Path("graph.json"))
    monkeypatch.setattr(
        "fno.graph.store.append_progress_note", lambda *a, **k: (True, None)
    )
    calls: list[tuple] = []

    def fake_notify(node_id, text, **kwargs):
        calls.append((node_id, text))
        if boom:
            raise RuntimeError("resolver exploded")
        return receipts or []

    monkeypatch.setattr(note_notify, "notify_note", fake_notify)
    result = CliRunner().invoke(graph_cli.cli, argv)
    return result, calls


def test_the_verb_delivers_by_default(monkeypatch) -> None:
    result, calls = _run(
        monkeypatch,
        ["note", "x-0d08", "the finding"],
        receipts=["notified sess-worker (holder of x-0d08): hosted msg-abc12345"],
    )
    assert result.exit_code == 0
    assert calls == [("x-0d08", "the finding")]
    assert "noted x-0d08: the finding" in result.stdout
    assert "notified sess-worker" in result.stdout


def test_quiet_writes_the_note_and_sends_nothing(monkeypatch) -> None:
    result, calls = _run(monkeypatch, ["note", "x-0d08", "the finding", "--quiet"])
    assert result.exit_code == 0
    assert calls == []
    assert "noted x-0d08: the finding" in result.stdout
    assert "notif" not in result.stdout


def test_nobody_to_reach_is_printed_rather_than_silent(monkeypatch) -> None:
    result, _ = _run(monkeypatch, ["note", "x-0d08", "the finding"], receipts=[])
    assert result.exit_code == 0
    assert "notify: no holder, owner or king to reach for x-0d08" in result.stdout


def test_a_failed_delivery_lands_on_stderr_and_keeps_the_note(monkeypatch) -> None:
    result, _ = _run(
        monkeypatch,
        ["note", "x-0d08", "the finding"],
        receipts=["notify FAILED sess-worker (holder of x-0d08): pair budget spent"],
    )
    assert result.exit_code == 0
    assert "noted x-0d08: the finding" in result.stdout
    assert "notify FAILED sess-worker" in result.stderr
    assert "notify FAILED" not in result.stdout


def test_a_raising_notifier_never_costs_the_note(monkeypatch) -> None:
    result, _ = _run(monkeypatch, ["note", "x-0d08", "the finding"], boom=True)
    assert result.exit_code == 0
    assert "noted x-0d08: the finding" in result.stdout
    assert "notify FAILED x-0d08: resolver exploded" in result.stderr


def test_a_wedged_send_is_bounded_and_reported_unconfirmed(tmp_path, monkeypatch) -> None:
    """A live inject can outlast any writer's patience; the note verb cannot."""
    import threading

    from fno.graph import note_notify

    monkeypatch.setattr(note_notify, "_SEND_TIMEOUT_SECONDS", 0.2)
    started = threading.Event()

    def wedge(address: str, body: str) -> str:
        started.set()
        threading.Event().wait(30)  # never returns within the bound
        return "hosted msg-never"

    receipts = notify_note(
        "x-0d08",
        "the finding",
        graph_path=_graph(tmp_path, [{"id": "x-0d08"}]),
        sender=wedge,
        claim_reader=_claims(**{"node:x-0d08": _live("sess-worker")}),
        king_resolver=lambda scope: [],
        self_session="sess-me",
    )
    assert started.is_set()
    assert receipts == [
        "notify UNCONFIRMED sess-worker (holder of x-0d08): no answer in 0s"
    ]


def test_an_unconfirmed_receipt_lands_on_stderr(monkeypatch) -> None:
    result, _ = _run(
        monkeypatch,
        ["note", "x-0d08", "the finding"],
        receipts=["notify UNCONFIRMED sess-worker (holder of x-0d08): no answer in 30s"],
    )
    assert result.exit_code == 0
    assert "noted x-0d08: the finding" in result.stdout
    assert "notify UNCONFIRMED sess-worker" in result.stderr


def test_the_crown_scope_is_the_epic_not_the_grandparent() -> None:
    """An ordinary child's epic is its own parent, whatever sits above that."""
    scopes: list[str] = []

    def record(scope: str) -> list[str]:
        scopes.append(scope)
        return ["king-of-the-epic"]

    got = note_recipients(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={"x-16b7": {"id": "x-16b7", "parent": "x-mission"}},
        claim_reader=_claims(),
        king_resolver=record,
    )
    assert scopes == ["x-16b7"]
    assert got == [("king-of-the-epic", "king of x-16b7")]


def test_a_contained_node_looks_one_level_further_out_for_the_crown() -> None:
    """Its parent is the node carrying its PR, so the epic is that node's parent."""
    scopes: list[str] = []

    def record(scope: str) -> list[str]:
        scopes.append(scope)
        return ["king-of-the-epic"]

    got = note_recipients(
        {"id": "x-0d08", "contained_in": "x-5a62", "parent": "x-5a62"},
        index={"x-5a62": {"id": "x-5a62", "parent": "x-16b7"}},
        claim_reader=_claims(),
        king_resolver=record,
    )
    assert scopes == ["x-16b7"]
    assert got == [("king-of-the-epic", "king of x-16b7")]


def test_a_supplied_snapshot_is_used_instead_of_a_second_graph_read(tmp_path, monkeypatch) -> None:
    """The note write already read the graph; the delivery must not read it again."""
    from fno.graph import store

    def explode(*a, **k):
        raise AssertionError("read_graph must not run when entries are supplied")

    monkeypatch.setattr(store, "read_graph", explode)
    receipts = notify_note(
        "x-0d08",
        "the finding",
        graph_path=tmp_path / "absent.json",
        entries=[{"id": "x-0d08"}],
        sender=lambda address, body: "hosted msg-1",
        claim_reader=_claims(**{"node:x-0d08": _live("sess-worker")}),
        king_resolver=lambda scope: [],
        self_session="sess-me",
    )
    assert receipts == ["notified sess-worker (holder of x-0d08): hosted msg-1"]


def test_the_store_hands_back_the_snapshot_it_read(tmp_path) -> None:
    """entries_out is what lets the note verb skip the second read."""
    import json as _json

    from fno.graph.store import append_progress_note

    graph = tmp_path / "graph.json"
    graph.write_text(_json.dumps({"entries": [{"id": "x-0d08", "parent": "x-16b7"}]}), encoding="utf-8")
    seen: list[dict] = []
    found, _plan = append_progress_note(graph, "x-0d08", {"ts": "T1", "text": "hi"}, entries_out=seen)
    assert found
    assert [e.get("id") for e in seen] == ["x-0d08"]
