"""US9 (x-f07d): wake-on-message daemon rung.

A `heads-up` to a resumable-but-asleep addressee is drained by a spawned
headless provider-routed agent (rung 3 of the inbox-daemon ladder). This makes
`wake-daemon` a real owner class rather than a hopeful label. `question`-kind is
excluded by construction (US8 owns it). Two concurrent wakes of one asleep
session collapse to a single revival because the spawn name is derived from the
session uuid, and the envelope msg-id never leaks into that derivation.
"""
from __future__ import annotations

import pytest


@pytest.fixture
def inbox_root(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    return tmp_path


@pytest.fixture
def repo_root(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_AUTO_MEMORY_DIR", str(tmp_path / "auto-memory"))
    return tmp_path


def _reachable(session_id: str, agent: str = "claude"):
    from fno.agents.discover import ReachableSession

    return ReachableSession(session_id=session_id, source="transcript", agent=agent)


def test_heads_up_to_asleep_session_wakes_drain_agent(inbox_root, repo_root, monkeypatch):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import Kind, read_unread_threads, write_new_thread

    calls: list[str] = []
    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable",
        lambda token, **kw: (_reachable("abc12345-0000-0000-0000-000000000000"), []),
    )

    def fake_wake(session_uuid, **kw):
        calls.append(session_uuid)
        return True, "wake-abc12345"

    monkeypatch.setattr("fno.agents.dispatch.wake_drain_agent", fake_wake)

    h = write_new_thread("abc12345", "bob", Kind.HEADS_UP.value, "PR merged, take a look")
    results = drain_inbox(repo_root, "abc12345")

    assert calls == ["abc12345-0000-0000-0000-000000000000"]
    assert results[0].action == "woke_drain_agent"
    # The woken agent drains its own inbox; the thread must stay unread here.
    assert read_unread_threads("abc12345")[0].path == h.path


def test_wake_refusal_falls_through_to_triage(inbox_root, repo_root, monkeypatch):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import Kind, write_new_thread

    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable",
        lambda token, **kw: (_reachable("live-0000"), []),
    )
    # Live/in-flight: the wake refuses, so the daemon triages as usual.
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_drain_agent",
        lambda session_uuid, **kw: (False, "writer-possibly-live"),
    )
    triaged: list[str] = []
    monkeypatch.setattr(
        "fno.inbox.drain._handle_heads_up_triage",
        lambda repo_root, project, h: triaged.append(h.thread_id) or _ignored(h),
    )

    write_new_thread("live", "bob", Kind.HEADS_UP.value, "look at this")
    drain_inbox(repo_root, "live")
    assert len(triaged) == 1


def test_heads_up_to_plain_project_triages_never_wakes(inbox_root, repo_root, monkeypatch):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import Kind, write_new_thread

    # A plain project name resolves to no session.
    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable", lambda token, **kw: (None, [])
    )
    waked: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_drain_agent",
        lambda session_uuid, **kw: waked.append(session_uuid) or (True, "x"),
    )

    write_new_thread("web", "bob", Kind.HEADS_UP.value, "deploy note")
    drain_inbox(repo_root, "web")
    assert waked == [], "a project heads-up triages; it never wakes a session"


def test_question_never_wakes_a_drain_agent(inbox_root, repo_root, monkeypatch):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import Kind, write_new_thread

    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable",
        lambda token, **kw: (_reachable("abc12345-x"), []),
    )
    waked: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_drain_agent",
        lambda session_uuid, **kw: waked.append(session_uuid) or (True, "x"),
    )

    write_new_thread("abc12345", "bob", Kind.QUESTION.value, "which one?")
    results = drain_inbox(repo_root, "abc12345")
    assert waked == [], "question never gets an autonomous responder (Locked Decision 7)"
    assert results[0].action == "wake_signal_dropped"


def test_non_claude_session_is_not_woken(inbox_root, repo_root, monkeypatch):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import Kind, write_new_thread

    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable",
        lambda token, **kw: (_reachable("cdx-1", agent="codex"), []),
    )
    waked: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_drain_agent",
        lambda session_uuid, **kw: waked.append(session_uuid) or (True, "x"),
    )

    write_new_thread("cdx-1", "bob", Kind.HEADS_UP.value, "note")
    drain_inbox(repo_root, "cdx-1")
    assert waked == [], "wake is claude-only (resume substrate is claude)"


def test_wake_name_derived_from_uuid_not_msgid(monkeypatch):
    """Concurrency AC: the spawn name is derived from the session uuid so two
    concurrent wakes collide on one flock. The msg-id must never leak in."""
    from fno.agents import dispatch as d

    captured: dict = {}

    def fake_spawn(**kw):
        captured.update(kw)

        class R:
            short_id = "wake-abcdef12"
            name = "wake-abcdef12"

        return R()

    monkeypatch.setattr(d, "dispatch_spawn", fake_spawn)
    ok, short = d.wake_drain_agent("abcdef12-9999-8888-7777-666655554444")
    assert ok is True
    assert captured["name"] == "wake-abcdef12"
    assert captured.get("resume_session_id") == "abcdef12-9999-8888-7777-666655554444"


def _ignored(h):
    from fno.inbox.drain import DrainResult

    return DrainResult(thread_id=h.thread_id, kind="heads-up", action="ignored",
                       thread_path=str(h.path))
