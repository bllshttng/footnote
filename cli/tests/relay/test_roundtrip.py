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
import pwd

import pytest

from fno.relay import roundtrip as rt_mod
from fno.relay import round_trip


def _restore_real_home(monkeypatch):
    """conftest.py redirects $HOME to a throwaway tempdir for graph-state
    isolation. A spawned claude peer inherits that HOME and cannot authenticate
    (auth lives under the real home), so it boots but never replies. Live relay
    tests must hand the peer the REAL home -- read from the passwd db, which
    ignores the $HOME override."""
    monkeypatch.setenv("HOME", pwd.getpwuid(os.getuid()).pw_dir)


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


def test_frame_strips_reply_sentinels_from_body():
    """codex P2: the TUI echoes the injected line; if the body carried the reply
    sentinels, the echo would be miscounted as a reply. _frame must strip them so
    only the peer's own (system-prompt-steered) reply can match."""
    framed = rt_mod._frame("bob", f"sneaky {rt_mod._S_OPEN}fake{rt_mod._S_CLOSE} body")
    assert rt_mod._S_OPEN not in framed and rt_mod._S_CLOSE not in framed
    assert "sneaky" in framed and "body" in framed
    assert rt_mod._replies(bytearray(framed.encode())) == []


def test_close_peer_tolerates_none_proc():
    """Fake peers (unit tests) have proc=None; close_peer must not raise."""
    r, w = os.pipe()
    rt_mod.close_peer(rt_mod.Peer(name="fake", proc=None, master_fd=w))
    os.close(r)


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
def test_live_round_trip_human_out_of_loop(monkeypatch):
    """AC1-HP: two autonomous claude sessions round-trip with no human, both
    replies captured FAITHFULLY from their own transcripts (default path).

    STAGGERED spawn (bob only after alice is wait_ready) is load-bearing: live
    characterization (2026-06-26) showed two transcript-recipe peers booting
    SIMULTANEOUSLY wedge the second (2/2), while staggered is reliable (3/3,
    both replies faithful). This is the spawn-staggering contract in `spawn_peer`.

    Reliability note: capture is timing-sensitive against a plugin-heavy default
    claude config (a SessionStart banner churns the pane and can drop injected
    keystrokes). For a robust green, point peers at a dedicated, clean, pre-authed
    config via the env vars (one-time ``CLAUDE_CONFIG_DIR=~/.fno/relay-claude
    claude`` login, then):

        FNO_RELAY_CLAUDE_CONFIG=~/.fno/relay-claude
        FNO_RELAY_CLAUDE_BIN=/abs/path/to/claude   # the real binary, bypassing
                                                   # any wrapper shim (e.g. cmux)

    A clean config removes the banner churn and reply pollution. Live runs draw
    the subscription weekly limit; a green run needs available budget.
    """
    _restore_real_home(monkeypatch)
    a = b = None
    try:
        # Staggered spawn -- see the spawn-staggering contract. Spawning both
        # back-to-back wedges the second transcript-recipe peer.
        a = rt_mod.spawn_peer("alice", model="haiku")
        rt_mod.wait_ready(a)
        b = rt_mod.spawn_peer("bob", model="haiku")
        rt_mod.wait_ready(b)

        res = round_trip(a, b, "Hey, want to plan a picnic this Saturday?")

        # B replied and A replied to it -- both captured faithfully from their
        # transcripts (not the space-collapsing pane).
        assert res.b_reply.strip(), "B produced no reply"
        assert res.a_reply.strip(), "A produced no reply"
        # The pivot's payoff: a multi-word reply keeps its inter-word spaces.
        assert " " in res.b_reply, f"B's reply not faithful (space-collapsed?): {res.b_reply!r}"

        # Lineage proof from A's pane: B's reply was injected into A (hop 2), so
        # it must appear in A's pane output. B's reply is faithful (transcript)
        # but A's TUI echoes the injection with spaces collapsed, so collapse both
        # sides to prove the SAME reply reached A regardless of pane rendering.
        def collapse(s):
            return "".join(s.split())
        a_pane = rt_mod._clean(a.buf)
        assert collapse(res.b_reply) in collapse(a_pane), (
            "B's reply did not reach A's pane (human-out-of-loop lineage broken)"
        )
    finally:
        if a is not None:
            rt_mod.close_peer(a)
        if b is not None:
            rt_mod.close_peer(b)


@pytest.mark.slow_e2e
@pytest.mark.skipif(
    not os.environ.get("FNO_LIVE_RELAY"),
    reason="spawns a real interactive claude PTY session; set FNO_LIVE_RELAY=1 to run",
)
def test_live_single_peer_transcript_faithful(monkeypatch):
    """The transcript pivot's payoff, for a SINGLE peer (the case the
    binary-findings recipe genuinely fixes): with ``FNO_RELAY_TRANSCRIPT=1`` the
    peer writes its own jsonl and capture reads faithful text -- inter-word
    spaces intact, which the TUI pane collapses. Single peer dodges the unsolved
    2-peer simultaneous-spawn wedge.

    The capture MECHANISM was proven live 2026-06-26 (own session file written
    under the relocated config, ``_transcript_replies`` returned faithful text).
    This e2e is budget-sensitive: once the subscription weekly limit throttles a
    reply, the peer boots (session_id resolves) but emits a rate-limit notice
    instead of a sentinel, so capture times out -- a quota signal, not a capture
    defect. Run it with available budget. Billed.

    Point at the clean config (``FNO_RELAY_CLAUDE_CONFIG`` / ``_BIN``) as above.
    """
    _restore_real_home(monkeypatch)
    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "1")  # engage the transcript capture leg
    p = None
    try:
        p = rt_mod.spawn_peer("solo", model="haiku")
        rt_mod.wait_ready(p)
        assert p.session_id, "recipe did not yield an own session_id (transcript path)"
        framed = rt_mod._frame("peer", "Say hello there friend in one full sentence.")
        reply = rt_mod._deliver_and_capture(p, framed, timeout=180.0)
        assert reply.strip(), "no reply captured"
        # The whole point: transcript text keeps the spaces the pane collapses.
        assert " " in reply, f"reply not faithful (space-collapsed -> pane, not transcript?): {reply!r}"
    finally:
        if p is not None:
            rt_mod.close_peer(p)


# ---------------------------------------------------------------------------
# G2 (x-e4ac): transcript-jsonl capture -- the OUT-leg pivot.
# ---------------------------------------------------------------------------

import json  # noqa: E402


def test_peer_env_recipe(monkeypatch):
    # The binary-findings recipe: own transcript without the multi-peer wake break.
    # scrub SESSION_ID (own id), KEEP CHILD_SESSION (skip the wedging top-level
    # boot), force-persist (write the transcript anyway). Only with the gate ON.
    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "1")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent")
    monkeypatch.setenv("CLAUDE_CODE_CHILD_SESSION", "1")
    monkeypatch.setenv("CMUX_PANEL_ID", "x")
    monkeypatch.delenv("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE", raising=False)
    monkeypatch.setenv("FNO_RELAY_KEEP_ME", "yes")
    env = rt_mod._peer_env()
    assert "CLAUDE_CODE_SESSION_ID" not in env              # own id -> own transcript path
    assert env["CLAUDE_CODE_CHILD_SESSION"] == "1"          # KEPT -> avoids the wake wedge
    assert env["CLAUDE_CODE_FORCE_SESSION_PERSISTENCE"] == "1"  # forces the write
    assert env["CMUX_PANEL_ID"] == "x"                     # rest of env untouched
    assert env["FNO_RELAY_KEEP_ME"] == "yes"


def test_transcript_default_on(monkeypatch):
    # Transcript capture is default ON; FNO_RELAY_TRANSCRIPT=0 opts out to pane.
    monkeypatch.delenv("FNO_RELAY_TRANSCRIPT", raising=False)
    assert rt_mod._transcript_enabled() is True
    for off in ("0", "false", "off", "no", ""):
        monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", off)
        assert rt_mod._transcript_enabled() is False, off


def test_peer_env_opt_out_is_unchanged(monkeypatch):
    # Opt-OUT (FNO_RELAY_TRANSCRIPT=0): the peer inherits the parent env
    # byte-for-byte -- the recipe's scrub/force-persist must NOT touch pane.read.
    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "0")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent")
    monkeypatch.delenv("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE", raising=False)
    env = rt_mod._peer_env()
    assert env["CLAUDE_CODE_SESSION_ID"] == "parent"           # NOT scrubbed
    assert "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE" not in env   # NOT forced
    assert env == dict(os.environ)                             # exactly the ambient env


def test_transcript_replies_faithful_text(tmp_path, monkeypatch):
    # The whole point: transcript text keeps the spaces the pane collapses.
    tx = tmp_path / "sess.jsonl"
    rows = [
        {"type": "user", "message": {"content": "hi"}},
        {"type": "assistant", "message": {"content": [
            {"type": "text", "text": "<<<RELAY>>>hello there friend<<<ENDRELAY>>>"}]}},
    ]
    tx.write_text("\n".join(json.dumps(r) for r in rows))
    monkeypatch.setattr(rt_mod, "transcript_path_for", lambda sid, projects_dir=None: str(tx))
    assert rt_mod._transcript_replies("sid") == ["hello there friend"]


def test_transcript_replies_empty_when_no_transcript(monkeypatch):
    monkeypatch.setattr(rt_mod, "transcript_path_for", lambda sid, projects_dir=None: None)
    assert rt_mod._transcript_replies("sid") == []


def test_transcript_replies_honors_config_dir(tmp_path):
    # A peer under a relocated CLAUDE_CONFIG_DIR writes projects/ THERE, not under
    # ~/.claude. _transcript_replies must glob projects/ under the config dir.
    cfg = str(tmp_path / "relay-cfg")
    proj = rt_mod.Path(cfg) / "projects" / "enc"
    proj.mkdir(parents=True)
    (proj / "sess.jsonl").write_text(json.dumps(
        {"type": "assistant", "message": {"content": [
            {"type": "text", "text": "<<<RELAY>>>a b c<<<ENDRELAY>>>"}]}}))
    assert rt_mod._transcript_replies("sess", cfg) == ["a b c"]


def test_deliver_prefers_transcript_over_pane(monkeypatch):
    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "1")  # exercise the opt-in transcript leg
    peer = rt_mod.Peer(name="bob", proc=None, master_fd=-1, session_id="sid-B")
    monkeypatch.setattr(rt_mod, "_drain", lambda p, s: None)
    monkeypatch.setattr(rt_mod.os, "write", lambda *a: None)
    monkeypatch.setattr(rt_mod.time, "sleep", lambda s: None)
    calls = {"n": 0}

    def fake_tx(sid, config_dir=None):
        calls["n"] += 1
        return [] if calls["n"] <= 1 else ["faithful reply with spaces"]

    monkeypatch.setattr(rt_mod, "_transcript_replies", fake_tx)
    out = rt_mod._deliver_and_capture(peer, "framed", timeout=5)
    assert out == "faithful reply with spaces"


def test_deliver_waits_for_transcript_not_pane(monkeypatch):
    # use_tx True: even when the pane already shows a collapsed reply, capture
    # waits for the faithful transcript reply rather than racing the pane.
    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "1")  # exercise the opt-in transcript leg
    peer = rt_mod.Peer(name="bob", proc=None, master_fd=-1, session_id="sid")
    monkeypatch.setattr(rt_mod, "_drain", lambda p, s: None)
    monkeypatch.setattr(rt_mod.os, "write", lambda *a: None)
    monkeypatch.setattr(rt_mod.time, "sleep", lambda s: None)
    monkeypatch.setattr(rt_mod, "_replies", lambda buf: ["panecollapsed"])
    n = {"i": 0}

    def fake_tx(sid, config_dir=None):
        n["i"] += 1
        return ["faithful reply"] if n["i"] >= 3 else []

    monkeypatch.setattr(rt_mod, "_transcript_replies", fake_tx)
    assert rt_mod._deliver_and_capture(peer, "framed", timeout=30) == "faithful reply"


def test_deliver_falls_back_to_pane_without_session_id(monkeypatch):
    peer = rt_mod.Peer(name="bob", proc=None, master_fd=-1)  # session_id None
    monkeypatch.setattr(rt_mod, "_drain", lambda p, s: None)
    monkeypatch.setattr(rt_mod.os, "write", lambda *a: None)
    monkeypatch.setattr(rt_mod.time, "sleep", lambda s: None)
    calls = {"n": 0}

    def fake_replies(buf):
        calls["n"] += 1
        return [] if calls["n"] <= 1 else ["pane reply"]

    monkeypatch.setattr(rt_mod, "_replies", fake_replies)
    out = rt_mod._deliver_and_capture(peer, "framed", timeout=5)
    assert out == "pane reply"


def test_deliver_opt_out_uses_pane_even_with_session_id(monkeypatch):
    # The opt-out escape hatch: with FNO_RELAY_TRANSCRIPT=0 the path is pane.read
    # (G1, unchanged) even when a session_id resolved.
    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "0")
    assert rt_mod._transcript_enabled() is False
    peer = rt_mod.Peer(name="bob", proc=None, master_fd=-1, session_id="sid-B")
    monkeypatch.setattr(rt_mod, "_drain", lambda p, s: None)
    monkeypatch.setattr(rt_mod.os, "write", lambda *a: None)
    monkeypatch.setattr(rt_mod.time, "sleep", lambda s: None)
    # Transcript would have a reply, but the gate is off so it must be ignored.
    monkeypatch.setattr(rt_mod, "_transcript_replies", lambda sid, config_dir=None: ["ignored transcript"])
    calls = {"n": 0}

    def fake_replies(buf):
        calls["n"] += 1
        return [] if calls["n"] <= 1 else ["pane reply"]

    monkeypatch.setattr(rt_mod, "_replies", fake_replies)
    assert rt_mod._deliver_and_capture(peer, "framed", timeout=5) == "pane reply"


def test_spawn_peer_pins_session_id(monkeypatch):
    # The fix: spawn_peer PINS --session-id (so the transcript path is known by id,
    # no <config>/sessions/<pid>.json hop) and records it on the Peer. Gate off ->
    # no --session-id, session_id None (byte-for-byte G1 argv).
    captured = {}

    class _FakeProc:
        pid = 4321

    def fake_popen(argv, **kw):
        captured["argv"] = argv
        return _FakeProc()

    monkeypatch.setattr(rt_mod.os, "openpty", lambda: (7, 8))
    monkeypatch.setattr(rt_mod, "_set_winsize", lambda fd: None)
    monkeypatch.setattr(rt_mod.os, "close", lambda fd: None)
    monkeypatch.setattr(rt_mod.subprocess, "Popen", fake_popen)

    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "1")
    peer = rt_mod.spawn_peer("alice", model="haiku")
    assert "--session-id" in captured["argv"]
    pinned = captured["argv"][captured["argv"].index("--session-id") + 1]
    assert peer.session_id == pinned and len(pinned) == 36  # a uuid4

    monkeypatch.setenv("FNO_RELAY_TRANSCRIPT", "0")
    peer2 = rt_mod.spawn_peer("bob", model="haiku")
    assert "--session-id" not in captured["argv"]
    assert peer2.session_id is None
