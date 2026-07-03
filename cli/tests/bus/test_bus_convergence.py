"""Task 3.2 - converge the inbox store onto the canonical bus log (US5/US8).

The bus log is the system of record; the per-recipient markdown thread file is
its render (written on every mutation). Every store write mirrors an envelope
into the global log, so:
  - AC5-UI: the agent inbox read verb is a cursor-filtered scan (to==me) and
            ack advances the cursor.
  - AC8-UI: a heads-up that lands on the bus is still triaged by drain into a
            backlog node carrying the same source_* provenance as today.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def inbox_and_bus(tmp_path, monkeypatch):
    """Co-isolate the md store (FNO_INBOX_ROOT) and the bus log under tmp."""
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    return tmp_path


# ---------------------------------------------------------------------------
# Dual-write: every store write lands a canonical envelope in the bus log
# ---------------------------------------------------------------------------

def test_write_new_thread_mirrors_to_bus_log(inbox_and_bus):
    from fno.inbox.store import write_new_thread, Kind
    from fno.bus.log import iter_messages

    h = write_new_thread("alice", "bob", Kind.HEADS_UP.value, "region column work")

    msgs = list(iter_messages())
    assert len(msgs) == 1
    env = msgs[0]
    assert env.id == h.thread_id
    assert env.from_ == "bob"
    assert env.to == "alice"
    assert env.kind == "heads-up"
    assert env.body == "region column work"
    # The render path is carried so drain can preserve provenance back to the md.
    assert env.meta.get("render_path", "").endswith(".md")


def test_append_to_thread_mirrors_reply_envelope(inbox_and_bus):
    from fno.inbox.store import write_new_thread, append_to_thread, Kind
    from fno.bus.log import iter_messages, iter_thread

    h = write_new_thread("alice", "bob", Kind.HEADS_UP.value, "root message body")
    new_id = append_to_thread(h.path, "alice", "a reply from alice")

    convo = list(iter_thread(h.thread_id))
    assert [m.id for m in convo] == [h.thread_id, new_id]
    # Reply correlates to the thread root and stays addressed to the thread owner.
    reply = convo[1]
    assert reply.in_reply_to == h.thread_id
    assert reply.to == "alice"
    assert reply.from_ == "alice"
    assert reply.kind == "heads-up"  # appended messages inherit the thread kind
    assert len(list(iter_messages())) == 2


def test_persist_to_memory_and_refs_carried_in_meta(inbox_and_bus):
    from fno.inbox.store import write_new_thread, Kind
    from fno.bus.log import iter_messages

    write_new_thread(
        "alice", "bob", Kind.FYI.value, "a lesson worth keeping",
        persist_to_memory=True, refs={"ref_pr": "459", "ref_node": "ab-deadbeef"},
    )
    env = list(iter_messages())[0]
    assert env.meta.get("persist_to_memory") is True
    assert env.meta.get("refs", {}).get("ref_pr") == "459"
    assert env.meta.get("refs", {}).get("ref_node") == "ab-deadbeef"


# ---------------------------------------------------------------------------
# AC5-UI: agent inbox read verb - cursor-filtered to==me; ack advances cursor
# ---------------------------------------------------------------------------

@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def bus(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths
    return paths.bus_dir()


def _bus_send(to, body):
    from fno.bus.log import Envelope, append
    env = Envelope.new(from_="peer", to=to, kind="send", body=body)
    append(env)
    return env


def test_ac5_ui_agents_inbox_shows_unread_to_me(bus, runner):
    from fno.mail.cli import mail_app

    _bus_send("me", "first")
    _bus_send("someone-else", "not mine")
    _bus_send("me", "second")

    res = runner.invoke(mail_app, ["unread", "--name", "me", "--json"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    bodies = [m["body"] for m in payload]
    assert bodies == ["first", "second"]


def test_ac5_ui_ack_advances_cursor(bus, runner):
    from fno.mail.cli import mail_app

    m1 = _bus_send("me", "alpha")
    _bus_send("me", "beta")

    res = runner.invoke(mail_app, ["ack", m1.id, "--name", "me"])
    assert res.exit_code == 0, res.output

    res2 = runner.invoke(mail_app, ["unread", "--name", "me", "--json"])
    payload = json.loads(res2.stdout)
    assert [m["body"] for m in payload] == ["beta"]


def test_ac5_ui_inbox_empty_when_nothing_unread(bus, runner):
    from fno.mail.cli import mail_app

    res = runner.invoke(mail_app, ["unread", "--name", "nobody", "--json"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.stdout) == []


def test_ack_unknown_id_fails_loudly(bus, runner):
    # Acking an id not in the log would leave a cursor scan_unread can't find,
    # silently re-surfacing everything. It must fail loudly, not report success.
    from fno.mail.cli import mail_app

    _bus_send("me", "real message")
    res = runner.invoke(mail_app, ["ack", "msg-doesnotexist", "--name", "me"])
    assert res.exit_code == 2, res.output
    assert "unknown message id" in (res.stdout + (res.stderr or "")).lower()


def test_ack_other_recipients_id_rejected(bus, runner):
    # Acking an id addressed to someone else would advance my cursor past my own
    # earlier unread (the cursor is a single global-log position). Reject it.
    from fno.mail.cli import mail_app

    mine = _bus_send("me", "for me, earlier")  # noqa: F841
    theirs = _bus_send("other", "for someone else, later")
    res = runner.invoke(mail_app, ["ack", theirs.id, "--name", "me"])
    assert res.exit_code == 2, res.output
    assert "addressed to" in (res.stdout + (res.stderr or "")).lower()
    # My earlier unread is still visible (cursor was NOT advanced).
    inbox = runner.invoke(mail_app, ["unread", "--name", "me", "--json"])
    assert [m["body"] for m in json.loads(inbox.stdout)] == ["for me, earlier"]


# ---------------------------------------------------------------------------
# AC8-UI: heads-up on the bus -> drain triages -> node keeps source_* provenance
# ---------------------------------------------------------------------------

def test_ac8_ui_heads_up_on_bus_triaged_with_provenance(inbox_and_bus, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_AUTO_MEMORY_DIR", str(tmp_path / "auto-memory"))

    from fno.inbox import drain as drain_mod
    from fno.inbox.store import write_new_thread, Kind
    from fno.inbox.triage import TriagePlan
    from fno.bus.log import iter_messages

    write_new_thread("alice", "bob", Kind.HEADS_UP.value, "add region column")

    # The heads-up is genuinely on the bus (canonical log), not only the md render.
    assert any(e.kind == "heads-up" for e in iter_messages())

    captured = {}

    def fake_triage_thread(h, **_kwargs):
        captured["from_project"] = h.from_project
        captured["root_msg_id"] = h.root_msg_id
        return TriagePlan(
            action="create_node", title="add region column",
            priority="p2", body="work", follow_up_question=None,
        )

    class _FakeRun:
        def __init__(self, returncode=0, stdout="", stderr=""):
            self.returncode = returncode
            self.stdout = stdout
            self.stderr = stderr

    def fake_run(cmd, *args, **kwargs):
        if cmd[:3] == ["fno-py", "new", "--help"]:
            return _FakeRun(stdout="--source-kind --source-project --source-inbox-msg\n")
        if cmd[:2] == ["fno-py", "new"]:
            captured["argv"] = cmd
            return _FakeRun(stdout="created node ab-abc123\n")
        raise AssertionError(f"unexpected subprocess: {cmd}")

    monkeypatch.setattr(drain_mod, "triage_thread", fake_triage_thread)
    monkeypatch.setattr(drain_mod.subprocess, "run", fake_run)

    results = drain_mod.drain_inbox(tmp_path, "alice")
    assert results[0].action == "created_node"
    assert results[0].node_id == "ab-abc123"
    # Provenance fields unchanged: source-kind/project/inbox-msg still passed.
    argv = captured["argv"]
    assert "--source-kind" in argv and "from_inbox" in argv
    assert "--source-project" in argv and "bob" in argv
    assert "--source-inbox-msg" in argv
