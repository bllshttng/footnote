"""`fno backlog note` delivery: who gets reached, and what a refusal must say.

The defect this closes is a write that succeeds while the communication fails
and nothing reports it. So the tests that matter assert a POSITIVE receipt:
every bound reader class produces a named line, nobody-bound REFUSES before
the append with every arm reading, and a run where no send confirms costs the
verb its exit code, because a silent success is indistinguishable from
delivery from where the author stands.

Every prover is injected or patched. `claim_status` walks a real lockfile tree
and `resolve_to_king` reads the live agent registry, so reading either for real
would make these tests pass or fail on what else is running on the machine.
Tests always pass `rows` to `note_readers`, so no test reads this machine's
registry.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.backlog import note_notify
from fno.backlog.note_notify import NoteReaders, Refused, note_readers, pointer, readers_before_append


def _holders(**by_node: str):
    return lambda node_id: by_node.get(node_id)


def _graph(tmp_path: Path, entries: list[dict]) -> Path:
    path = tmp_path / "graph.json"
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return path


def _row(name: str, sid: str | None = None, status: str = "live", node: str | None = None):
    return SimpleNamespace(
        name=name,
        harness_session_id=sid,
        cc_session_id=None,
        session_id=None,
        status=status,
        node=node,
    )


def _free(monkeypatch) -> None:
    monkeypatch.setattr(
        "fno.claims.core.claim_status", lambda key: {"state": "free", "holder": None}
    )


# --- who gets reached --------------------------------------------------------


def test_holder_then_king_each_once_and_in_order() -> None:
    got = note_readers(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={"x-16b7": {"id": "x-16b7"}},
        rows=[],
        holder_of=_holders(**{"x-0d08": "sess-worker"}),
        kings_of=lambda scope: ["king-a", "king-a"],
    )
    assert got.recipients == [
        ("sess-worker", "holder of x-0d08"),
        ("king-a", "king of x-16b7"),
    ]


def test_a_contained_note_reaches_the_owners_holder() -> None:
    """A note on a contained node is material to whoever builds the owner."""
    got = note_readers(
        {"id": "x-0d08", "contained_in": "x-5a62", "parent": "x-16b7"},
        index={"x-5a62": {"id": "x-5a62", "parent": "x-16b7"}},
        rows=[],
        holder_of=_holders(**{"x-0d08": "sess-note-owner", "x-5a62": "sess-builder"}),
        kings_of=lambda scope: [],
    )
    assert got.recipients == [
        ("sess-note-owner", "holder of x-0d08"),
        ("sess-builder", "holder of owner x-5a62"),
    ]


def test_a_graph_session_reaches_a_worker_the_claim_misses(monkeypatch) -> None:
    """AC1-HP: locked_by_harness_session binds where the claim does not."""
    _free(monkeypatch)
    rows = [_row("t-d211-selfkill", sid="sess-live")]
    got = note_readers(
        {"id": "x-d211", "locked_by_harness_session": "sess-live"},
        index={},
        rows=rows,
        kings_of=lambda scope: [],
    )
    assert got.recipients == [
        ("t-d211-selfkill", "session bound to x-d211 (graph locked_by_harness_session)")
    ]
    assert "claim node:x-d211: free" in got.readings
    assert "graph locked_by_harness_session: sess-live -> t-d211-selfkill" in got.readings


def test_the_registry_arm_reaches_a_worker_no_graph_field_names(monkeypatch) -> None:
    """AC1-EDGE: a graph session_id naming no live row falls to the registry arm."""
    _free(monkeypatch)
    rows = [_row("bp-2e1f-crown-slot", sid="other", node="x-2e1f")]
    got = note_readers(
        {"id": "x-2e1f", "session_id": "sess-gone"},
        index={},
        rows=rows,
        kings_of=lambda scope: [],
    )
    assert got.recipients == [("bp-2e1f-crown-slot", "worker on x-2e1f (registry node)")]
    assert "graph session_id: sess-gone names no live row" in got.readings


def test_a_live_claim_stops_the_chain_before_the_registry_arm(monkeypatch) -> None:
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key: {"state": "live", "holder": "sess-worker"},
    )
    rows = [_row("bp-2e1f-crown-slot", node="x-2e1f")]
    got = note_readers({"id": "x-2e1f"}, index={}, rows=rows, kings_of=lambda scope: [])
    assert got.recipients == [("sess-worker", "holder of x-2e1f")]
    assert not any(reading.startswith("registry:") for reading in got.readings)


def test_an_epic_notes_its_own_crown() -> None:
    """AC2-HP: an epic's crown sits on the epic itself, not on a parent."""
    scopes: list[str] = []
    got = note_readers(
        {"id": "x-a792", "type": "epic"},
        index={},
        rows=[],
        holder_of=_holders(),
        kings_of=lambda scope: scopes.append(scope)
        or (["king-a792-control"] if scope == "x-a792" else []),
    )
    assert got.recipients == [("king-a792-control", "king of x-a792")]
    assert scopes == ["x-a792"]


def test_a_parentless_node_walks_to_its_project_king() -> None:
    got = note_readers(
        {"id": "x-1b2c", "project": "fno"},
        index={},
        rows=[],
        holder_of=_holders(),
        kings_of=lambda scope: ["king-fno-g5"] if scope == "fno" else [],
    )
    assert got.recipients == [("king-fno-g5", "king of fno (project)")]
    assert "crown fno (project): king-fno-g5" in got.readings


def test_the_crown_walk_stops_at_the_first_live_scope() -> None:
    """AC2-EDGE: the epic crown wins, so the resolver never asks the project."""
    scopes: list[str] = []

    def kings(scope: str) -> list[str]:
        scopes.append(scope)
        return ["king-a792-control"] if scope == "x-a792" else ["king-fno-g5"]

    got = note_readers(
        {"id": "x-child", "parent": "x-a792", "project": "fno"},
        index={},
        rows=[],
        holder_of=_holders(),
        kings_of=kings,
    )
    assert got.recipients == [("king-a792-control", "king of x-a792")]
    assert scopes == ["x-a792"]


def test_an_unreadable_crown_scope_costs_itself_not_the_walk() -> None:
    def kings(scope: str) -> list[str]:
        if scope == "x-a792":
            raise RuntimeError("court unreadable")
        return ["king-fno-g5"]

    got = note_readers(
        {"id": "x-child", "parent": "x-a792", "project": "fno"},
        index={},
        rows=[],
        holder_of=_holders(),
        kings_of=kings,
    )
    assert got.recipients == [("king-fno-g5", "king of fno (project)")]
    assert "crown x-a792: unreadable (court unreadable)" in got.readings


def test_the_crown_scope_is_the_epic_not_the_grandparent() -> None:
    """An ordinary child's epic is its own parent, whatever sits above that."""
    scopes: list[str] = []
    got = note_readers(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={"x-16b7": {"id": "x-16b7", "parent": "x-mission"}},
        rows=[],
        holder_of=_holders(),
        kings_of=lambda scope: scopes.append(scope) or ["king-of-the-epic"],
    )
    assert scopes == ["x-16b7"]
    assert got.recipients == [("king-of-the-epic", "king of x-16b7")]


def test_a_contained_node_looks_one_level_further_out_for_the_crown() -> None:
    """Its parent carries its PR, so the epic is that node's parent."""
    scopes: list[str] = []
    got = note_readers(
        {"id": "x-0d08", "contained_in": "x-5a62", "parent": "x-5a62"},
        index={"x-5a62": {"id": "x-5a62", "parent": "x-16b7"}},
        rows=[],
        holder_of=_holders(),
        kings_of=lambda scope: scopes.append(scope) or ["king-of-the-epic"],
    )
    assert scopes == ["x-16b7"]
    assert got.recipients == [("king-of-the-epic", "king of x-16b7")]


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


# --- the author is named, never mailed ---------------------------------------


def test_the_author_is_never_mailed_its_own_note() -> None:
    got = note_readers(
        {"id": "x-0d08", "parent": "x-16b7"},
        index={},
        rows=[],
        holder_of=_holders(**{"x-0d08": "sess-me"}),
        kings_of=lambda scope: ["sess-me"],
        self_session="sess-me",
    )
    assert got.recipients == []


def test_a_role_prefixed_holder_is_still_recognised_as_self() -> None:
    """`target-session:<id>` is the same session as the bare `<id>` prover."""
    got = note_readers(
        {"id": "x-0d08"},
        index={},
        rows=[_row("sess-me", sid="sess-me")],
        holder_of=_holders(**{"x-0d08": "target-session:sess-me"}),
        kings_of=lambda scope: [],
        self_session="sess-me",
    )
    assert got.recipients == []
    assert got.author_bound == "holder of x-0d08"


def test_a_king_is_never_mailed_its_own_note_on_its_own_epic() -> None:
    """AC3-HP: identity key, not name shape, names the author."""
    self_sid = "d88ad3a3-b820-440e-9654-70fad39cd7d8"
    rows = [_row("king-a792-control", sid=self_sid)]
    got = note_readers(
        {"id": "x-child", "parent": "x-a792"},
        index={},
        rows=rows,
        holder_of=_holders(),
        kings_of=lambda scope: ["king-a792-control"],
        self_session=self_sid,
    )
    assert got.recipients == []
    assert got.author_bound == "king of x-a792"


def test_a_role_holder_backed_by_the_own_row_is_the_author() -> None:
    self_sid = "01a08dab-7d3a"
    rows = [_row("bp-0b97-note-notify", sid=self_sid)]
    got = note_readers(
        {"id": "x-0b97"},
        index={},
        rows=rows,
        holder_of=_holders(**{"x-0b97": "spawn-handover:bp-0b97-note-notify"}),
        kings_of=lambda scope: [],
        self_session=self_sid,
    )
    assert got.recipients == []
    assert got.author_bound == "holder of x-0b97"


def test_a_row_with_a_different_session_is_a_reader_not_the_author() -> None:
    rows = [_row("sess-worker", sid="another-session")]
    got = note_readers(
        {"id": "x-0d08"},
        index={},
        rows=rows,
        holder_of=_holders(**{"x-0d08": "sess-worker"}),
        kings_of=lambda scope: [],
        self_session="sess-me-uuid",
    )
    assert got.recipients == [("sess-worker", "holder of x-0d08")]
    assert got.author_bound is None


# --- role holders resolve once, where the address is born ---------------------


class _Row:
    name = "t-ae54-worker"
    harness_session_id = "01a08dab-7d3a"
    cc_session_id = None
    session_id = None


def test_holder_agent_name_resolves_all_three_role_prefixes() -> None:
    from fno.claims.core import (
        BLUEPRINT_HOLDER_PREFIX,
        TARGET_SESSION_HOLDER_PREFIX,
        holder_agent_name,
    )

    assert holder_agent_name("spawn-handover:t-ae54-worker", [_Row()]) == "t-ae54-worker"
    assert (
        holder_agent_name(f"{TARGET_SESSION_HOLDER_PREFIX}01a08dab-7d3a", [_Row()])
        == "t-ae54-worker"
    )
    assert (
        holder_agent_name(f"{BLUEPRINT_HOLDER_PREFIX}01a08dab-7d3a", [_Row()])
        == "t-ae54-worker"
    )
    assert holder_agent_name(f"{BLUEPRINT_HOLDER_PREFIX}gone", []) is None
    assert holder_agent_name("spawn-handover:bp-gone", []) is None
    assert holder_agent_name("sess-plain-holder", []) == "sess-plain-holder"
    assert holder_agent_name(None, []) is None


def test_a_resolvable_role_holder_reaches_the_worker_behind_it() -> None:
    got = note_readers(
        {"id": "x-0d08"},
        index={},
        rows=[_Row()],
        holder_of=_holders(**{"x-0d08": "spawn-handover:t-ae54-worker"}),
        kings_of=lambda scope: [],
    )
    assert got.recipients == [("t-ae54-worker", "holder of x-0d08")]


def test_an_unresolvable_role_holder_skips_instead_of_failing() -> None:
    """The night's notify leg failed on role markers handed to the mail
    resolver verbatim; a marker with no row behind it is nobody to reach."""
    got = note_readers(
        {"id": "x-0d08"},
        index={},
        rows=[],
        holder_of=_holders(**{"x-0d08": "spawn-handover:bp-c79d-prwatch-deadline"}),
        kings_of=lambda scope: [],
    )
    assert got.recipients == []


# --- the sender is a resolvable handle, never a literal ----------------------


def test_send_pointer_stamps_the_callers_own_handle(monkeypatch) -> None:
    """Provenance is looked up by from_name, so the sender must be this
    session's own handle: a literal that matches no registry row ships
    harness=unknown with no from_session."""
    import fno.agents.dispatch as dispatch_mod
    from fno.harness_identity import canonical_handle

    session = "a1535d0b88424e4dbcafd733b8defc9c"
    seen: dict = {}

    def fake_send(address, body, provider, **kwargs):
        seen.update(kwargs)
        return SimpleNamespace(delivery="hosted", msg_id="msg-abc12345")

    monkeypatch.setattr(dispatch_mod, "dispatch_send", fake_send)
    monkeypatch.setattr(note_notify, "own_session", lambda: session)
    assert note_notify.send_pointer("sess-worker", "body") == "hosted msg-abc12345"
    assert seen["from_name"] == canonical_handle(session)


def test_send_pointer_without_identity_keeps_the_default_and_still_sends(
    monkeypatch,
) -> None:
    import fno.agents.dispatch as dispatch_mod

    seen: dict = {}

    def fake_send(address, body, provider, **kwargs):
        seen.update(kwargs)
        return SimpleNamespace(delivery="durable", msg_id="msg-abc12345")

    monkeypatch.setattr(dispatch_mod, "dispatch_send", fake_send)
    monkeypatch.setattr(note_notify, "own_session", lambda: None)
    assert note_notify.send_pointer("sess-worker", "body") == "durable msg-abc12345"
    assert seen["from_name"] == "fno"


# --- the refusal: resolution runs before the append ---------------------------


def _forbid_append(monkeypatch) -> None:
    def no_append(*a, **k):
        raise AssertionError("append_progress_note must not run on a refusal")

    monkeypatch.setattr("fno.graph.store.append_progress_note", no_append)


def test_nobody_bound_refuses_with_every_arm_reading(tmp_path, monkeypatch) -> None:
    """AC4-HP: silence is the defect; the refusal names every arm it read."""
    _free(monkeypatch)
    monkeypatch.setattr(note_notify, "own_session", lambda: "sess-me")
    monkeypatch.setattr(note_notify, "crowned_over", lambda scope: [])
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
    _forbid_append(monkeypatch)
    got = readers_before_append(
        "x-d211", _graph(tmp_path, [{"id": "x-d211", "project": "fno"}])
    )
    assert isinstance(got, Refused)
    assert got.exit_code == 3
    assert got.message == (
        "note refused: nobody bound to x-d211 would be told, so nothing was written.\n"
        "  claim node:x-d211: free\n"
        "  graph locked_by_harness_session: none\n"
        "  graph session_id: none\n"
        "  graph locked_by: none\n"
        "  registry: no live row names x-d211\n"
        "  crown fno (project): vacant\n"
        "Write it anyway with --quiet, or find a reader with fno agents court "
        "and mail them by name."
    )


def test_an_unreadable_registry_refuses_before_the_append(tmp_path, monkeypatch) -> None:
    """AC4-ERR: a fault cannot prove anyone would be told, so nothing writes."""
    from fno.agents.registry import RegistryVersionError

    def boom():
        raise RegistryVersionError("registry at /tmp/r.json is malformed JSON")

    monkeypatch.setattr("fno.agents.registry.load_registry", boom)
    _forbid_append(monkeypatch)
    got = readers_before_append("x-d211", _graph(tmp_path, [{"id": "x-d211"}]))
    assert isinstance(got, Refused)
    assert got.exit_code == 3
    assert got.message.startswith(
        "note refused: could not read who is bound to x-d211 "
        "(registry at /tmp/r.json is malformed JSON), so nothing was written."
    )
    assert got.message.endswith(
        "Write it anyway with --quiet, or find a reader with fno agents court "
        "and mail them by name."
    )


def test_an_unknown_node_refuses_with_the_verb_text(tmp_path) -> None:
    got = readers_before_append("x-ffff", _graph(tmp_path, [{"id": "x-0d08"}]))
    assert isinstance(got, Refused)
    assert got.message == "Error: no node resolves to 'x-ffff'"
    assert got.exit_code == 1


def test_a_bound_author_alone_is_not_a_refusal(monkeypatch, tmp_path) -> None:
    """Decision 1: the author being the only bound reader still writes."""
    _free(monkeypatch)
    monkeypatch.setattr(note_notify, "own_session", lambda: "sess-me")
    monkeypatch.setattr(note_notify, "crowned_over", lambda scope: [])
    monkeypatch.setattr(
        "fno.agents.registry.load_registry",
        lambda: [_row("sess-me", sid="sess-me", node="x-d211")],
    )
    got = readers_before_append(
        "x-d211", _graph(tmp_path, [{"id": "x-d211", "project": "fno"}])
    )
    assert isinstance(got, NoteReaders)
    assert got.recipients == []
    assert got.author_bound is not None


# --- what the delivery reports ------------------------------------------------


def test_a_delivered_send_reports_the_transport(monkeypatch) -> None:
    monkeypatch.setattr(note_notify, "send_pointer", lambda a, b: "hosted msg-abc12345")
    readers = NoteReaders("x-0d08", [("sess-worker", "holder of x-0d08")], None, [])
    assert note_notify.send_note(readers, "the finding") == [
        ("notified sess-worker (holder of x-0d08): hosted msg-abc12345", False)
    ]


def test_a_failed_send_is_reported_and_never_raised(monkeypatch) -> None:
    def boom(address: str, body: str) -> str:
        raise RuntimeError("pair budget spent")

    monkeypatch.setattr(note_notify, "send_pointer", boom)
    readers = NoteReaders("x-0d08", [("sess-worker", "holder of x-0d08")], None, [])
    assert note_notify.send_note(readers, "the finding") == [
        ("notify FAILED sess-worker (holder of x-0d08): pair budget spent", True)
    ]


def test_a_wedged_send_is_bounded_and_reported_unconfirmed(monkeypatch) -> None:
    """A live inject can outlast any writer's patience; the note verb cannot."""
    import threading

    monkeypatch.setattr(note_notify, "_SEND_TIMEOUT_SECONDS", 0.2)
    started = threading.Event()

    def wedge(address: str, body: str) -> str:
        started.set()
        threading.Event().wait(30)  # never returns within the bound
        return "hosted msg-never"

    monkeypatch.setattr(note_notify, "send_pointer", wedge)
    readers = NoteReaders("x-0d08", [("sess-worker", "holder of x-0d08")], None, [])
    receipts = note_notify.send_note(readers, "the finding")
    assert started.is_set()
    assert receipts == [
        ("notify UNCONFIRMED sess-worker (holder of x-0d08): no answer in 0s", True)
    ]


def test_the_author_as_the_only_reader_sends_nothing_and_exits_zero(
    monkeypatch, capsys
) -> None:
    """AC5-HP: the author-only line names the binding, on stdout."""
    def no_send(address: str, body: str) -> str:
        raise AssertionError("nothing to send")

    monkeypatch.setattr(note_notify, "send_pointer", no_send)
    readers = NoteReaders("x-a792", [], "king of x-a792", [])
    assert note_notify.deliver(readers, "the ruling", json_output=False) == 0
    assert (
        "notify: you are the only reader bound to x-a792 (king of x-a792); "
        "nobody else to tell"
    ) in capsys.readouterr().out


def test_no_confirmed_delivery_costs_the_exit_code(monkeypatch, capsys) -> None:
    """AC5-ERR: all receipts unconfirmed or failed, exit 4, summary on stderr."""
    def boom(address: str, body: str) -> str:
        raise RuntimeError("pair budget spent")

    monkeypatch.setattr(note_notify, "send_pointer", boom)
    readers = NoteReaders(
        "x-0d08",
        [("sess-a", "holder of x-0d08"), ("sess-b", "king of x-16b7")],
        None,
        [],
    )
    assert note_notify.deliver(readers, "the finding", json_output=False) == 4
    err = capsys.readouterr().err
    assert err.count("notify FAILED") == 2
    assert err.strip().splitlines()[-1] == (
        "notify: x-0d08 is noted, but no reader confirmed delivery "
        "(0 UNCONFIRMED, 2 FAILED). An UNCONFIRMED send may still land, "
        "so check before you re-send."
    )


def test_one_confirmed_delivery_among_failures_still_exits_zero(monkeypatch) -> None:
    def send(address: str, body: str) -> str:
        if address == "sess-a":
            return "hosted msg-ok"
        raise RuntimeError("pair budget spent")

    monkeypatch.setattr(note_notify, "send_pointer", send)
    readers = NoteReaders(
        "x-0d08", [("sess-a", "holder of x-0d08"), ("sess-b", "king of x-16b7")], None, []
    )
    assert note_notify.deliver(readers, "the finding", json_output=False) == 0


# --- the verb: refuse before the append, deliver by default --------------------


def _run(monkeypatch, argv: list[str], readers=None, refused=None, send=None):
    """Run `fno backlog note` with resolution and the graph write stubbed.

    `refused` short-circuits resolution; `readers` stands in for a resolved
    binding. The append stub records the node id it was handed.
    """
    from typer.testing import CliRunner

    from fno.graph import cli as graph_cli

    monkeypatch.setattr(graph_cli, "_graph_path", lambda *a, **k: Path("graph.json"))
    appended: list[str] = []

    def fake_append(path, node_id, note):
        appended.append(node_id)
        return True, None

    monkeypatch.setattr("fno.graph.store.append_progress_note", fake_append)

    def fake_readers(task_id, graph_path):
        if refused is not None:
            return refused
        return readers

    monkeypatch.setattr(note_notify, "readers_before_append", fake_readers)
    if send is not None:
        monkeypatch.setattr(note_notify, "send_pointer", send)
    return CliRunner().invoke(graph_cli.cli, argv), appended


def test_the_verb_delivers_by_default(monkeypatch) -> None:
    readers = NoteReaders("x-0d08", [("sess-worker", "holder of x-0d08")], None, [])
    result, appended = _run(
        monkeypatch,
        ["note", "x-0d08", "the finding"],
        readers=readers,
        send=lambda a, b: "hosted msg-abc",
    )
    assert result.exit_code == 0
    assert appended == ["x-0d08"]
    assert "noted x-0d08: the finding" in result.stdout
    assert "notified sess-worker" in result.stdout


def test_quiet_writes_the_note_and_resolves_nobody(monkeypatch) -> None:
    """AC4-EDGE: --quiet skips resolution entirely and writes."""
    from typer.testing import CliRunner

    from fno.graph import cli as graph_cli

    monkeypatch.setattr(graph_cli, "_graph_path", lambda *a, **k: Path("graph.json"))
    monkeypatch.setattr(
        "fno.graph.store.append_progress_note", lambda *a, **k: (True, None)
    )

    def no_resolve(task_id, graph_path):
        raise AssertionError("the resolver must not run under --quiet")

    monkeypatch.setattr(note_notify, "readers_before_append", no_resolve)
    result = CliRunner().invoke(
        graph_cli.cli, ["note", "x-0d08", "the finding", "--quiet"]
    )
    assert result.exit_code == 0
    assert "noted x-0d08: the finding" in result.stdout
    assert "notif" not in result.stdout


def test_a_refusal_writes_nothing_and_exits_three(monkeypatch) -> None:
    def no_send(address: str, body: str) -> str:
        raise AssertionError("a refused note must not send")

    refused = Refused(
        "note refused: nobody bound to x-d211 would be told, so nothing was written.\n"
        "Write it anyway with --quiet, or find a reader with fno agents court "
        "and mail them by name.",
        3,
    )
    result, appended = _run(
        monkeypatch, ["note", "x-d211", "the finding"], refused=refused, send=no_send
    )
    assert result.exit_code == 3
    assert appended == []
    assert result.stdout == ""
    assert "note refused: nobody bound to x-d211" in result.stderr


def test_an_unknown_node_refuses_before_the_append(tmp_path, monkeypatch) -> None:
    """The real resolver against a real graph file: exit 1, nothing appended."""
    from typer.testing import CliRunner

    from fno.graph import cli as graph_cli

    graph = _graph(tmp_path, [{"id": "x-0d08"}])
    monkeypatch.setattr(graph_cli, "_graph_path", lambda *a, **k: graph)
    appended: list[str] = []

    def fake_append(path, node_id, note):
        appended.append(node_id)
        return True, None

    monkeypatch.setattr("fno.graph.store.append_progress_note", fake_append)
    result = CliRunner().invoke(graph_cli.cli, ["note", "x-ffff", "the finding"])
    assert result.exit_code == 1
    assert appended == []
    assert "no node resolves to 'x-ffff'" in result.stderr


def test_the_author_only_reader_prints_the_binding_line(monkeypatch) -> None:
    def no_send(address: str, body: str) -> str:
        raise AssertionError("nothing to send")

    readers = NoteReaders("x-a792", [], "king of x-a792", [])
    result, _ = _run(
        monkeypatch, ["note", "x-a792", "the ruling"], readers=readers, send=no_send
    )
    assert result.exit_code == 0
    assert (
        "notify: you are the only reader bound to x-a792 (king of x-a792)"
        in result.stdout
    )


def test_failed_sends_land_on_stderr_and_exit_four(monkeypatch) -> None:
    def boom(address: str, body: str) -> str:
        raise RuntimeError("pair budget spent")

    readers = NoteReaders(
        "x-0d08", [("sess-a", "holder of x-0d08"), ("sess-b", "king of x-16b7")], None, []
    )
    result, _ = _run(
        monkeypatch, ["note", "x-0d08", "the finding"], readers=readers, send=boom
    )
    assert result.exit_code == 4
    assert "noted x-0d08: the finding" in result.stdout
    assert "notify FAILED sess-a" in result.stderr
    assert "no reader confirmed delivery (0 UNCONFIRMED, 2 FAILED)" in result.stderr


def test_an_undelivered_receipt_never_reaches_stdout(monkeypatch) -> None:
    def boom(address: str, body: str) -> str:
        raise RuntimeError("budget spent")

    readers = NoteReaders("x-0d08", [("sess-worker", "holder of x-0d08")], None, [])
    result, _ = _run(
        monkeypatch, ["note", "x-0d08", "the finding"], readers=readers, send=boom
    )
    assert result.exit_code == 4
    assert "noted x-0d08: the finding" in result.stdout
    assert "notify FAILED sess-worker" in result.stderr
    assert "notify FAILED" not in result.stdout
