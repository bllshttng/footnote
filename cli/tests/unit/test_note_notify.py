"""`fno backlog note` delivery: who gets reached, and what a failure must say.

The defect this closes is a write that succeeds while the communication fails
and nothing reports it. So the tests that matter assert a POSITIVE receipt:
every recipient class produces a named line, a failed send produces a
`notify FAILED` line naming the address and the cause, and no path raises into
the caller, because the note is already on disk when this code runs.

Every prover is injected or patched. `claim_status` walks a real lockfile tree
and `resolve_to_king` reads the live agent registry, so reading either for real
would make these tests pass or fail on what else is running on the machine.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.graph import note_notify
from fno.graph.note_notify import deliver_note, note_recipients, pointer


def _holders(**by_node: str):
    return lambda node_id: by_node.get(node_id)


def _graph(tmp_path: Path, entries: list[dict]) -> Path:
    path = tmp_path / "graph.json"
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return path


# --- who gets reached --------------------------------------------------------


def test_holder_then_king_each_once_and_in_order() -> None:
    got = note_recipients(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={"x-16b7": {"id": "x-16b7"}},
        holder_of=_holders(**{"x-0d08": "sess-worker"}),
        kings_of=lambda scope: ["king-a", "king-a"],
    )
    assert got == [
        ("sess-worker", "holder of x-0d08"),
        ("king-a", "king of x-16b7"),
    ]


def test_a_contained_note_reaches_the_owners_holder() -> None:
    """A note on a contained node is material to whoever builds the owner."""
    got = note_recipients(
        {"id": "x-0d08", "contained_in": "x-5a62", "parent": "x-16b7"},
        index={"x-5a62": {"id": "x-5a62", "parent": "x-16b7"}},
        holder_of=_holders(**{"x-0d08": "sess-note-owner", "x-5a62": "sess-builder"}),
        kings_of=lambda scope: [],
    )
    assert got == [
        ("sess-note-owner", "holder of x-0d08"),
        ("sess-builder", "holder of owner x-5a62"),
    ]


def test_the_author_is_never_mailed_its_own_note() -> None:
    got = note_recipients(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={},
        holder_of=_holders(**{"x-0d08": "sess-me"}),
        kings_of=lambda scope: ["sess-me"],
        self_session="sess-me",
    )
    assert got == []


def test_a_role_prefixed_holder_is_still_recognised_as_self() -> None:
    """`target-session:<id>` is the same session as the bare `<id>` prover."""
    got = note_recipients(
        {"id": "x-0d08"},
        index={},
        holder_of=_holders(**{"x-0d08": "target-session:sess-me"}),
        kings_of=lambda scope: [],
        self_session="sess-me",
    )
    assert got == []


def test_a_raising_king_resolver_costs_the_king_not_the_holder() -> None:
    def boom(scope: str) -> list[str]:
        raise RuntimeError("registry unreadable")

    got = note_recipients(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={},
        holder_of=_holders(**{"x-0d08": "sess-worker"}),
        kings_of=boom,
    )
    assert got == [("sess-worker", "holder of x-0d08")]


def test_the_crown_scope_is_the_epic_not_the_grandparent() -> None:
    """An ordinary child's epic is its own parent, whatever sits above that."""
    scopes: list[str] = []
    got = note_recipients(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={"x-16b7": {"id": "x-16b7", "parent": "x-mission"}},
        holder_of=_holders(),
        kings_of=lambda scope: scopes.append(scope) or ["king-of-the-epic"],
    )
    assert scopes == ["x-16b7"]
    assert got == [("king-of-the-epic", "king of x-16b7")]


def test_a_contained_node_looks_one_level_further_out_for_the_crown() -> None:
    """Its parent carries its PR, so the epic is that node's parent."""
    scopes: list[str] = []
    got = note_recipients(
        {"id": "x-0d08", "contained_in": "x-5a62", "parent": "x-5a62"},
        index={"x-5a62": {"id": "x-5a62", "parent": "x-16b7"}},
        holder_of=_holders(),
        kings_of=lambda scope: scopes.append(scope) or ["king-of-the-epic"],
    )
    assert scopes == ["x-16b7"]
    assert got == [("king-of-the-epic", "king of x-16b7")]


@pytest.mark.parametrize("state", ["stale", "free", "corrupted"])
def test_an_unowned_claim_reaches_nobody(state: str, monkeypatch) -> None:
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key: {"state": state, "holder": "sess-dead"},
    )
    assert note_notify.claim_holder("x-0d08") is None


def test_a_suspect_claim_is_still_owned_and_still_reached(monkeypatch) -> None:
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key: {"state": "suspect", "holder": "sess-s"},
    )
    assert note_notify.claim_holder("x-0d08") == "sess-s"


def test_the_pointer_carries_the_node_and_truncates_the_note() -> None:
    body = pointer("x-0d08", "the sideline glyph chain is wrong " + "word " * 40)
    assert body.startswith("note on x-0d08: the sideline glyph chain is wrong")
    assert "..." in body
    assert "fno backlog get x-0d08" in body
    assert len(body.split()) < 40  # well under the 80-word pair budget


# --- what the delivery reports ----------------------------------------------


@pytest.fixture()
def one_holder(monkeypatch):
    """A graph of one node whose holder is another session."""
    monkeypatch.setattr(note_notify, "own_session", lambda: "sess-me")
    monkeypatch.setattr(note_notify, "crowned_over", lambda scope: [])
    monkeypatch.setattr(note_notify, "claim_holder", lambda node_id: "sess-worker")


def test_a_delivered_send_reports_the_transport(tmp_path, one_holder, monkeypatch) -> None:
    monkeypatch.setattr(note_notify, "send_pointer", lambda a, b: "hosted msg-abc12345")
    assert deliver_note(
        "x-0d08", "the finding", _graph(tmp_path, [{"id": "x-0d08"}])
    ) == [("notified sess-worker (holder of x-0d08): hosted msg-abc12345", False)]


def test_a_failed_send_is_reported_and_never_raised(tmp_path, one_holder, monkeypatch) -> None:
    def boom(address: str, body: str) -> str:
        raise RuntimeError("pair budget spent")

    monkeypatch.setattr(note_notify, "send_pointer", boom)
    assert deliver_note(
        "x-0d08", "the finding", _graph(tmp_path, [{"id": "x-0d08"}])
    ) == [("notify FAILED sess-worker (holder of x-0d08): pair budget spent", True)]


def test_a_wedged_send_is_bounded_and_reported_unconfirmed(
    tmp_path, one_holder, monkeypatch
) -> None:
    """A live inject can outlast any writer's patience; the note verb cannot."""
    import threading

    monkeypatch.setattr(note_notify, "_SEND_TIMEOUT_SECONDS", 0.2)
    started = threading.Event()

    def wedge(address: str, body: str) -> str:
        started.set()
        threading.Event().wait(30)  # never returns within the bound
        return "hosted msg-never"

    monkeypatch.setattr(note_notify, "send_pointer", wedge)
    receipts = deliver_note("x-0d08", "the finding", _graph(tmp_path, [{"id": "x-0d08"}]))
    assert started.is_set()
    assert receipts == [
        ("notify UNCONFIRMED sess-worker (holder of x-0d08): no answer in 0s", True)
    ]


def test_nobody_to_reach_is_still_a_receipt(tmp_path, monkeypatch) -> None:
    """Silence would read the same as delivery."""
    monkeypatch.setattr(note_notify, "own_session", lambda: "sess-me")
    monkeypatch.setattr(note_notify, "crowned_over", lambda scope: [])
    monkeypatch.setattr(note_notify, "claim_holder", lambda node_id: None)
    monkeypatch.setattr(
        note_notify, "send_pointer", lambda a, b: pytest.fail("nothing to send")
    )
    assert deliver_note(
        "x-0d08", "the finding", _graph(tmp_path, [{"id": "x-0d08"}])
    ) == [("notify: no holder, owner or king to reach for x-0d08", False)]


def test_an_unknown_node_reports_rather_than_raising(tmp_path) -> None:
    assert deliver_note(
        "x-ffff", "the finding", _graph(tmp_path, [{"id": "x-0d08"}])
    ) == [("notify FAILED x-ffff: no node resolves to it", True)]


def test_a_supplied_snapshot_is_used_instead_of_a_second_graph_read(
    tmp_path, one_holder, monkeypatch
) -> None:
    """The note write already read the graph; the delivery must not read again."""
    from fno.graph import store

    def explode(*a, **k):
        raise AssertionError("read_graph must not run when entries are supplied")

    monkeypatch.setattr(store, "read_graph", explode)
    monkeypatch.setattr(note_notify, "send_pointer", lambda a, b: "hosted msg-1")
    assert deliver_note(
        "x-0d08", "the finding", tmp_path / "absent.json", [{"id": "x-0d08"}]
    ) == [("notified sess-worker (holder of x-0d08): hosted msg-1", False)]


def test_the_store_hands_back_the_snapshot_it_read(tmp_path) -> None:
    """entries_out is what lets the note verb skip the second read."""
    from fno.graph.store import append_progress_note

    graph = _graph(tmp_path, [{"id": "x-0d08", "parent": "x-16b7"}])
    seen: list[dict] = []
    found, _plan = append_progress_note(
        graph, "x-0d08", {"ts": "T1", "text": "hi"}, entries_out=seen
    )
    assert found
    assert [e.get("id") for e in seen] == ["x-0d08"]


# --- the verb: delivery is the default, --quiet is the opt-out ---------------


def _run(monkeypatch, argv: list[str], receipts=None, boom: bool = False):
    """Run `fno backlog note` with the graph write and the delivery stubbed."""
    from typer.testing import CliRunner

    from fno.graph import cli as graph_cli

    monkeypatch.setattr(graph_cli, "_graph_path", lambda *a, **k: Path("graph.json"))
    monkeypatch.setattr(
        "fno.graph.store.append_progress_note", lambda *a, **k: (True, None)
    )
    calls: list[tuple] = []

    def fake_deliver(node_id, text, graph_path, entries=None):
        calls.append((node_id, text))
        if boom:
            raise RuntimeError("resolver exploded")
        return receipts or []

    monkeypatch.setattr(note_notify, "deliver_note", fake_deliver)
    return CliRunner().invoke(graph_cli.cli, argv), calls


def test_the_verb_delivers_by_default(monkeypatch) -> None:
    result, calls = _run(
        monkeypatch,
        ["note", "x-0d08", "the finding"],
        receipts=[("notified sess-worker (holder of x-0d08): hosted msg-abc", False)],
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


def test_an_undelivered_receipt_lands_on_stderr(monkeypatch) -> None:
    result, _ = _run(
        monkeypatch,
        ["note", "x-0d08", "the finding"],
        receipts=[("notify FAILED sess-worker (holder of x-0d08): budget spent", True)],
    )
    assert result.exit_code == 0
    assert "noted x-0d08: the finding" in result.stdout
    assert "notify FAILED sess-worker" in result.stderr
    assert "notify FAILED" not in result.stdout


def test_a_raising_delivery_never_costs_the_note(monkeypatch) -> None:
    result, _ = _run(monkeypatch, ["note", "x-0d08", "the finding"], boom=True)
    assert result.exit_code == 0
    assert "noted x-0d08: the finding" in result.stdout
