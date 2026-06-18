"""Move A (G1, ab-cee91152) - flip the inbox store canon to jsonl-first.

Before this change, ``store.py`` wrote the per-recipient markdown thread
durable-first and mirrored to the bus log best-effort - the inverse of the
intended design. This suite pins the flipped contract:

  - AC1-ERR: a bus-log append failure FAILS the send (the log is the durable
    write the caller depends on), surfacing the error - never a silent loss.
  - AC1-FR:  a markdown-render write failure does NOT fail the send (the log
    append already landed; the render is derived and regenerable).
  - AC1-EDGE: a deleted render is regenerated from the log by
    ``rebuild_render`` with no message lost - proving the log, not the
    markdown, is the source of truth.
"""
from __future__ import annotations

import pytest

from fno.paths_testing import use_tmpdir


@pytest.fixture
def inbox_and_bus(tmp_path, monkeypatch):
    """Co-isolate the md render (FNO_INBOX_ROOT) and the bus log under tmp."""
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


# ---------------------------------------------------------------------------
# AC1-ERR: a bus-log append failure fails the send (durable-first)
# ---------------------------------------------------------------------------

def test_send_fails_when_bus_append_fails(inbox_and_bus, monkeypatch):
    from fno.inbox import store
    from fno.bus.log import iter_messages

    def boom(_env):
        raise OSError("disk full")

    # The store imports append lazily inside the helper; patch the source.
    monkeypatch.setattr("fno.bus.log.append", boom)

    with pytest.raises(OSError):
        store.write_new_thread("alice", "bob", store.Kind.HEADS_UP.value, "region work")

    # Nothing leaked to the log (it raised) and no half-written render is left
    # masquerading as a durable message the log never saw.
    assert list(iter_messages()) == []
    inbox = store.inbox_dir_for("alice")
    leftover = list(inbox.glob("*.md")) if inbox.exists() else []
    assert leftover == [], f"render left behind after a failed durable send: {leftover}"


# ---------------------------------------------------------------------------
# AC1-FR: a render-write failure does NOT fail the send (the inverse contract)
# ---------------------------------------------------------------------------

def test_send_succeeds_when_render_write_fails(inbox_and_bus, monkeypatch, capsys):
    from fno.inbox import store
    from fno.bus.log import iter_messages

    def boom(_target, _content):
        raise OSError("readonly fs")

    monkeypatch.setattr(store, "_atomic_write_text", boom)

    # The durable write (bus append) landed, so the send SUCCEEDS even though
    # the derived render could not be written.
    handle = store.write_new_thread("alice", "bob", store.Kind.HEADS_UP.value, "still durable")

    msgs = list(iter_messages())
    assert len(msgs) == 1
    assert msgs[0].id == handle.thread_id
    assert msgs[0].body == "still durable"

    # The failure is logged loudly, not silent.
    err = capsys.readouterr().err
    assert "render" in err.lower()


# ---------------------------------------------------------------------------
# AC1-EDGE: a deleted render is regenerated from the log, no message lost
# ---------------------------------------------------------------------------

def test_message_survives_render_delete_via_rebuild(inbox_and_bus):
    from fno.inbox import store

    h = store.write_new_thread("alice", "bob", store.Kind.HEADS_UP.value, "do not lose me")
    render = h.path
    assert render.exists()

    # Simulate render loss/corruption.
    render.unlink()
    assert not render.exists()

    rebuilt = store.rebuild_render("alice")
    assert rebuilt >= 1  # at least one thread regenerated

    # The message is recoverable: a fresh render exists carrying the body.
    threads = store.read_all_threads("alice")
    assert any(
        any(m.body == "do not lose me" for m in t.messages) for t in threads
    ), "message lost after render delete + rebuild"


def test_rebuild_render_reconstructs_multiple_threads(inbox_and_bus):
    from fno.inbox import store

    store.write_new_thread("alice", "bob", store.Kind.HEADS_UP.value, "first message")
    store.write_new_thread("alice", "carol", store.Kind.QUESTION.value, "second message")

    # Wipe the whole render dir; the log is canon.
    inbox = store.inbox_dir_for("alice")
    for p in inbox.glob("*.md"):
        p.unlink()
    assert list(inbox.glob("*.md")) == []

    n = store.rebuild_render("alice")
    assert n == 2

    bodies = {
        m.body for t in store.read_all_threads("alice") for m in t.messages
    }
    assert {"first message", "second message"} <= bodies


def test_rebuild_render_is_idempotent(inbox_and_bus):
    from fno.inbox import store

    store.write_new_thread("alice", "bob", store.Kind.FYI.value, "once")
    # Re-rendering when the render already matches the log must not duplicate.
    store.rebuild_render("alice")
    store.rebuild_render("alice")
    threads = store.read_all_threads("alice")
    total_msgs = sum(len(t.messages) for t in threads)
    assert total_msgs == 1, f"rebuild duplicated messages: {total_msgs}"
