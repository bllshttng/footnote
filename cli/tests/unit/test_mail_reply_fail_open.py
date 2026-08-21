"""A reply must survive an unreadable registry, because its handle was never typed.

`fno agents mail reply` exists so a worker never re-types a handle: the address comes
off the answered message. Validating that handle against a store this reader
cannot parse defeats the one verb built for the case.

The uniqueness check guards an ambiguous TYPED name. There is no typed name on
the reply path, so an unreadable store must not take the reply down with it.
The cost of being wrong is total rather than degraded: a worker whose thread
arrived live has no id on the durable bus, so losing the reply path removes its
only way to coordinate.

`require_resolution` keeps its refusal. There the handle IS a guess, either a
mutable alias on a session-addressed record or a migrated legacy token.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents import registry as reg
from fno.cli import app
from fno.paths_testing import use_tmpdir

SENDER = "9a063cd3"
SENDER_SID = "9a063cd3-69d4-415a-ada5-649b0164189c"


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


def _isolate_empty_discovery(monkeypatch, tmp_path):
    """Every discovery source empty, so only the registry has anything to say."""
    from fno.agents import discover

    empty = tmp_path / "empty"
    empty.mkdir(exist_ok=True)
    monkeypatch.setenv(discover.SESSIONS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(empty))
    daemon = tmp_path / "daemon-empty"
    daemon.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))


def _seed_source_ahead_registry(tmp_path: Path) -> Path:
    """The incident itself: a writer one schema ahead of this reader.

    Read-forward makes this readable again, so it exercises the end-to-end
    recovery rather than the fail-open branch. Use `_seed_torn_registry` for a
    store that stays genuinely unreadable by design.
    """
    from fno.paths import agents_registry_path

    path = agents_registry_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"schema_version": reg.SCHEMA_VERSION + 1, "agents": []}, indent=2),
        encoding="utf-8",
    )
    return path


def _seed_torn_registry(tmp_path: Path) -> Path:
    """A store that is unreadable for a reason read-forward must never paper over.

    A version gap is recoverable. A torn file is not, so this is the input that
    keeps exercising the reply path's fail-open branch after read-forward lands.
    """
    from fno.paths import agents_registry_path

    path = agents_registry_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{ not json", encoding="utf-8")
    return path


def _seed_inbound(*, from_: str) -> str:
    from fno.inbox.store import write_new_thread

    return write_new_thread(
        recipient="claude-meeeeeee", sender=from_, kind="send", body="ping", to_kind="name"
    ).thread_id


def _bus_msgs():
    from fno.bus.log import iter_messages

    return list(iter_messages())


def test_reply_delivers_though_the_registry_is_torn(
    runner, mailbox, monkeypatch, tmp_path
):
    """The fail-open branch proper, on a store that stays unreadable by design."""
    _isolate_empty_discovery(monkeypatch, tmp_path)
    _seed_torn_registry(tmp_path)
    msg = _seed_inbound(from_=SENDER)

    result = runner.invoke(app, ["agents", "mail", "reply", "--to", msg, "--body", "ack"])

    assert result.exit_code == 0, result.output
    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 1, f"reply never reached the bus: {result.output}"
    assert replies[0].to == SENDER


def test_the_refusal_never_names_the_handle_as_the_problem(
    runner, mailbox, monkeypatch, tmp_path
):
    """The old message blamed the handle, which sent readers hunting the wrong thing."""
    _isolate_empty_discovery(monkeypatch, tmp_path)
    _seed_torn_registry(tmp_path)
    msg = _seed_inbound(from_=SENDER)

    result = runner.invoke(app, ["agents", "mail", "reply", "--to", msg, "--body", "ack"])

    assert "cannot be checked uniquely" not in result.output


def test_the_reported_incident_end_to_end(runner, mailbox, monkeypatch, tmp_path):
    """A source-ahead writer must not cost this machine its reply path.

    This is the pair of fixes meeting: read-forward makes the store readable
    again, and fail-open covers it if anything else about it is unreadable.
    """
    _isolate_empty_discovery(monkeypatch, tmp_path)
    _seed_source_ahead_registry(tmp_path)
    msg = _seed_inbound(from_=SENDER)

    result = runner.invoke(app, ["agents", "mail", "reply", "--to", msg, "--body", "ack"])

    assert result.exit_code == 0, result.output
    assert [m.to for m in _bus_msgs() if m.in_reply_to == msg] == [SENDER]


def test_an_ambiguous_handle_is_still_refused(runner, mailbox, monkeypatch, tmp_path):
    """Failing open on an unreadable store must not fail open on a real collision."""
    from fno.agents import discover as discover_mod

    _isolate_empty_discovery(monkeypatch, tmp_path)
    msg = _seed_inbound(from_=SENDER)

    monkeypatch.setattr(
        discover_mod, "resolve_or_suggest", lambda *_a, **_k: (None, [])
    )
    monkeypatch.setattr(
        discover_mod,
        "resolve_reachable",
        lambda *_a, **_k: (None, ["sid-one", "sid-two"]),
    )

    result = runner.invoke(app, ["agents", "mail", "reply", "--to", msg, "--body", "ack"])

    assert result.exit_code != 0
    assert "ambiguous" in result.output


def test_a_lone_visible_candidate_still_refuses(runner, mailbox, monkeypatch, tmp_path):
    """The discriminator, stated on its own because it reads backwards.

    A visible candidate whose uniqueness went unproven is the wake-a-stranger
    case: an unreadable store may hold a session colliding on the same short id,
    so choosing the one row we can see is the guess. Failing open is safe only
    when there is no candidate to choose between at all.
    """
    from fno.agents import discover as discover_mod

    _isolate_empty_discovery(monkeypatch, tmp_path)
    msg = _seed_inbound(from_=SENDER)

    class _Candidate:
        session_id = SENDER_SID

    def _raise(*_a, **_k):
        raise discover_mod.StoreReadError(["transcript"], resolved=_Candidate())

    monkeypatch.setattr(discover_mod, "resolve_or_suggest", lambda *_a, **_k: (None, []))
    monkeypatch.setattr(discover_mod, "resolve_reachable", _raise)

    result = runner.invoke(app, ["agents", "mail", "reply", "--to", msg, "--body", "ack"])

    assert result.exit_code != 0
    assert [m for m in _bus_msgs() if m.in_reply_to == msg] == []


def test_a_session_lane_alias_still_refuses_on_an_unreadable_store(
    runner, mailbox, monkeypatch, tmp_path
):
    """require_resolution keeps its wall: that handle is a guess, not a record."""
    from fno.inbox.store import write_new_thread

    _isolate_empty_discovery(monkeypatch, tmp_path)
    _seed_source_ahead_registry(tmp_path)
    msg = write_new_thread(
        recipient="recipient",
        sender="mutable-alias",
        kind="send",
        body="ping",
        to_kind="session",
    ).thread_id

    result = runner.invoke(app, ["agents", "mail", "reply", "--to", msg, "--body", "ack"])

    assert result.exit_code != 0
