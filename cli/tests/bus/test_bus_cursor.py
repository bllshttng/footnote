"""Task 3.1 - per-agent read cursors over the bus log (US5).

Covers AC5-UI (only messages after my cursor with to==me; ack advances),
AC5-EDGE (cursor resolves across a rotation), AC5-FR (deleted cursor rescans
retained segments rather than silently losing unprocessed mail).

The cursor is keyed by last-seen message-id (never a raw byte offset) so a
rotation cannot silently reset or skip a read position (locked decision 7).
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


@pytest.fixture
def bus(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths
    return paths.bus_dir()


def _send(to, body):
    from fno.bus.log import Envelope, append
    env = Envelope.new(from_="x", to=to, kind="send", body=body)
    append(env)
    return env


# ---------------------------------------------------------------------------
# AC5-UI: scan returns only my unseen messages; ack advances the cursor
# ---------------------------------------------------------------------------

def test_ac5_ui_scan_unread_filters_to_me_and_after_cursor(bus):
    from fno.bus.cursor import scan_unread, advance_cursor

    _send("me", "a")
    other = _send("someone-else", "not mine")  # noqa: F841
    m2 = _send("me", "b")

    unread = scan_unread("me")
    assert [m.body for m in unread] == ["a", "b"]

    # Ack up through the first message; only later ones remain.
    advance_cursor("me", unread[0].id)
    unread2 = scan_unread("me")
    assert [m.body for m in unread2] == ["b"]

    advance_cursor("me", m2.id)
    assert scan_unread("me") == []


def test_advance_cursor_is_forward_only(bus):
    # Acking an older (own) message must not rewind the cursor and re-surface
    # already-consumed mail.
    from fno.bus.cursor import advance_cursor, scan_unread, read_cursor

    m1 = _send("me", "first")
    m2 = _send("me", "second")
    m3 = _send("me", "third")

    assert advance_cursor("me", m3.id) is True
    assert scan_unread("me") == []

    # Backward ack to an earlier own message is a no-op (returns False).
    assert advance_cursor("me", m1.id) is False
    assert read_cursor("me") == m3.id  # cursor unchanged
    assert scan_unread("me") == []     # nothing re-surfaced

    # Re-acking the exact current id is also a no-op.
    assert advance_cursor("me", m3.id) is False
    assert read_cursor("me") == m3.id
    _ = m2  # m2 referenced for clarity of ordering


def test_ack_is_visible_via_cursor_file(bus):
    from fno.bus.cursor import advance_cursor, read_cursor, cursor_path

    m = _send("me", "hello")
    advance_cursor("me", m.id)
    assert read_cursor("me") == m.id
    assert cursor_path("me").exists()


# ---------------------------------------------------------------------------
# AC5-FR: deleted cursor file rescans retained segments (no silent loss)
# ---------------------------------------------------------------------------

def test_ac5_fr_deleted_cursor_rescans_from_start(bus):
    from fno.bus.cursor import scan_unread, advance_cursor, cursor_path

    m1 = _send("me", "one")
    _send("me", "two")
    advance_cursor("me", m1.id)
    assert [m.body for m in scan_unread("me")] == ["two"]

    # Operator deletes the cursor; a never-seen consumer must re-receive all.
    cursor_path("me").unlink()
    assert [m.body for m in scan_unread("me")] == ["one", "two"]


def test_absent_cursor_scans_from_start(bus):
    from fno.bus.cursor import scan_unread

    _send("fresh", "first ever")
    # No cursor written yet: a never-seen peer still receives durable mail.
    assert [m.body for m in scan_unread("fresh")] == ["first ever"]


# ---------------------------------------------------------------------------
# AC5-EDGE: cursor resolves correctly across a rotation
# ---------------------------------------------------------------------------

def test_ac5_edge_cursor_resolves_across_rotation(bus, monkeypatch):
    monkeypatch.setenv("FNO_BUS_MAX_BYTES", "300")
    from fno.bus.cursor import scan_unread, advance_cursor
    from fno.bus.log import bus_log_path

    first = _send("me", "early-0")
    advance_cursor("me", first.id)
    for i in range(1, 8):
        _send("me", f"early-{i}")

    # Force enough volume that the cursor's message rotated into a .N segment.
    seg1 = Path(str(bus_log_path()) + ".1")
    assert seg1.exists()

    unread = [m.body for m in scan_unread("me")]
    # Everything after the cursor's (now-rotated) message is still found, in order.
    assert unread == [f"early-{i}" for i in range(1, 8)]


def test_corrupt_cursor_file_treated_as_absent(bus):
    from fno.bus.cursor import scan_unread, cursor_path

    _send("me", "msg-a")
    p = cursor_path("me")
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("not json{{{", encoding="utf-8")
    # A corrupt cursor must not crash the scan; fail-open to "rescan retained".
    assert [m.body for m in scan_unread("me")] == ["msg-a"]
