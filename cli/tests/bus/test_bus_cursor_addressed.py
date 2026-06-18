"""Group 1 (ab-ba91b807) - addressed read + sender-exclusion (Option A, cv-d54ddd45).

The by-name read already works (scan_unread(name) returns to==name after the
recipient's cursor). These cover the genuinely-missing read-side pieces:

  - AC2-HP:  a worker drains its own by-name mail; peers and the sender do not.
  - AC2-EDGE: a message addressed before the recipient is live is still drained
              when the recipient next scans (durable, cursor-absent).
  - AC2-FR:  the project-broadcast read excludes the sender (no self-echo), the
              real cv-d54ddd45 footgun; the per-recipient cursor advances once.
"""
from __future__ import annotations

import pytest

from fno.paths_testing import use_tmpdir


@pytest.fixture
def bus(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths
    return paths.bus_dir()


def _send(to, body, *, from_="someone", to_kind=None, from_session=None):
    from fno.bus.log import Envelope, append
    env = Envelope.new(
        from_=from_, to=to, kind="send", body=body,
        to_kind=to_kind, from_session=from_session,
    )
    append(env)
    return env


# ---------------------------------------------------------------------------
# AC2-HP: by-name delivery reaches the recipient, not peers, not the sender
# ---------------------------------------------------------------------------

def test_by_name_reaches_only_recipient(bus):
    from fno.bus.cursor import scan_unread

    _send("B", "for bob", from_="S", to_kind="name")

    assert [m.body for m in scan_unread("B")] == ["for bob"]
    assert scan_unread("A") == []   # a peer never sees B's mail
    assert scan_unread("S") == []   # the sender never sees its own send


# ---------------------------------------------------------------------------
# AC2-EDGE: durable - addressed before recipient ever scanned, still delivered
# ---------------------------------------------------------------------------

def test_durable_to_never_seen_recipient(bus):
    from fno.bus.cursor import scan_unread

    _send("late", "queued while you were away", from_="S", to_kind="name")
    # `late` has no cursor yet (never live); the message is still drained.
    assert [m.body for m in scan_unread("late")] == ["queued while you were away"]


# ---------------------------------------------------------------------------
# AC2-FR: project broadcast excludes the sender (no self-echo)
# ---------------------------------------------------------------------------

def test_project_broadcast_excludes_sender_by_name(bus):
    from fno.bus.cursor import scan_unread

    _send("projA", "from S", from_="S", to_kind="project")
    _send("projA", "from X", from_="X", to_kind="project")

    # A member reading the project channel, excluding itself (S), sees only X's.
    got = [m.body for m in scan_unread("projA", exclude_from={"S"})]
    assert got == ["from X"]


def test_project_broadcast_excludes_sender_by_session(bus):
    from fno.bus.cursor import scan_unread

    _send("projA", "mine", from_="S", to_kind="project", from_session="sess-S")
    _send("projA", "theirs", from_="X", to_kind="project", from_session="sess-X")

    got = [m.body for m in scan_unread("projA", exclude_from={"sess-S"})]
    assert got == ["theirs"]


def test_exclude_from_none_is_backcompat(bus):
    # Default (no exclusion) is unchanged: every to==name line is returned.
    from fno.bus.cursor import scan_unread

    _send("projA", "a", from_="S", to_kind="project")
    _send("projA", "b", from_="X", to_kind="project")
    assert [m.body for m in scan_unread("projA")] == ["a", "b"]
