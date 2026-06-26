"""Group 1 (x-7060 / US1 / AC1): a proven human-out-of-loop round-trip over the
send-keys vehicle (Locked Decision #2, revised).

Two layers:

- Fast structural tests (CI) monkeypatch the per-hop deliver/capture to assert
  the two-hop orchestration, the peer-provenance framing direction, and the
  human-out-of-loop lineage (hop 2's message IS hop 1's captured reply) without
  spawning anything.
- The live test (``slow_e2e`` + ``FNO_LIVE_RELAY=1``) is the AC1-HP proof: it
  spawns two REAL interactive claude sessions in footnote-owned PTYs, drives
  A->B->A via send-keys, and reads BOTH panes to prove B's reply reached A with
  no human action. Excluded from per-PR CI by the marker; opt-in only.
"""
from __future__ import annotations

import os

import pytest

from fno.relay import roundtrip as rt_mod
from fno.relay import round_trip


def _fake_peer(name: str) -> rt_mod.Peer:
    # proc/master_fd are unused when _deliver_and_capture is monkeypatched.
    return rt_mod.Peer(name=name, proc=None, master_fd=-1)


# ---------------------------------------------------------------------------
# Fast structural tests (CI) -- the round-trip is two send-keys hops.
# ---------------------------------------------------------------------------

def test_round_trip_two_hops_with_correct_framing(monkeypatch):
    """Hop 1 delivers the seed to B framed as from A; hop 2 delivers B's reply
    to A framed as from B. Lineage: hop-2's delivered text IS hop-1's reply."""
    calls = []

    def fake_deliver(peer, framed, timeout):
        calls.append({"peer": peer.name, "framed": framed})
        return "sure, lunch sounds great" if peer.name == "bob" else "see you at noon"

    monkeypatch.setattr(rt_mod, "_deliver_and_capture", fake_deliver)

    a, b = _fake_peer("alice"), _fake_peer("bob")
    res = round_trip(a, b, "want lunch?")

    assert len(calls) == 2
    # Hop 1: seed -> B, framed as from A (peer-provenance, LD#3).
    assert calls[0]["peer"] == "bob"
    assert "want lunch?" in calls[0]["framed"]
    assert 'peer "alice"' in calls[0]["framed"]
    # Hop 2: B's reply -> A, framed as from B. The delivered text is exactly B's
    # reply -- no human re-typed it (the human-out-of-loop lineage).
    assert calls[1]["peer"] == "alice"
    assert "sure, lunch sounds great" in calls[1]["framed"]
    assert 'peer "bob"' in calls[1]["framed"]
    # Both replies surface, each read from its recipient's pane.
    assert res.b_reply == "sure, lunch sounds great"
    assert res.a_reply == "see you at noon"
    assert res.a_name == "alice" and res.b_name == "bob"


def test_frame_is_single_line_and_peer_provenanced():
    """Framing must be one line (an embedded newline would submit the turn early
    in the claude TUI) and must mark the sender as a peer, not the user."""
    framed = rt_mod._frame("bob", "multi\nline\nbody")
    assert "\n" not in framed
    assert 'peer "bob"' in framed
    assert "not your user" in framed


def test_sentinel_capture_extracts_last_reply():
    """Replies are extracted from the (ANSI-stripped) pane by the sentinel
    pair, newest wins, whitespace normalized."""
    buf = bytearray(
        (
            "\x1b[36mnoise\x1b[0m "
            f"{rt_mod._S_OPEN}first reply{rt_mod._S_CLOSE} more noise "
            f"{rt_mod._S_OPEN}second  reply{rt_mod._S_CLOSE}\x1b[0m"
        ).encode()
    )
    reps = rt_mod._replies(buf)
    assert reps == ["first reply", "second reply"]


# ---------------------------------------------------------------------------
# AC1-HP live proof -- spawns two real claude PTY sessions. Excluded from CI.
# ---------------------------------------------------------------------------

@pytest.mark.slow_e2e
@pytest.mark.skipif(
    not os.environ.get("FNO_LIVE_RELAY"),
    reason="spawns two real interactive claude PTY sessions; set FNO_LIVE_RELAY=1 to run",
)
def test_live_round_trip_human_out_of_loop():
    """AC1-HP: two autonomous claude sessions round-trip with no human, proven by
    reading BOTH panes.

    Reliability note: pane.read capture is timing-sensitive against a
    plugin-heavy default claude config (a SessionStart banner churns the pane and
    can drop injected keystrokes). For a robust green, point ``spawn_peer`` at a
    dedicated, clean, pre-authed config via ``CLAUDE_CONFIG_DIR`` (no personal
    plugins -> a standard transcript jsonl is also written, enabling the
    design-preferred jsonl capture). See the PR for the one-time-login setup.
    """
    a = rt_mod.spawn_peer("alice", model="haiku")
    b = rt_mod.spawn_peer("bob", model="haiku")
    try:
        rt_mod.wait_ready(a)
        rt_mod.wait_ready(b)

        res = round_trip(a, b, "Hey, want to plan a picnic this Saturday?")

        # B replied (read from B's pane) and A replied to it (read from A's pane).
        assert res.b_reply.strip(), "B produced no reply"
        assert res.a_reply.strip(), "A produced no reply"

        # Lineage proof from A's pane: B's reply was injected into A (hop 2), so
        # it must appear in A's pane output -- B's reply reached A, no human.
        a_pane = rt_mod._clean(a.buf)
        assert res.b_reply in a_pane, (
            "B's reply did not reach A's pane (human-out-of-loop lineage broken)"
        )
    finally:
        rt_mod.close_peer(a)
        rt_mod.close_peer(b)
