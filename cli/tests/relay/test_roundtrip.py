"""Relay claude transport tests (inside-out E4.3).

E4.3 retired the relay's own PTY: the daemon owns the interactive-claude PTY, the
relay injects via the daemon ``worker.submit`` RPC (routed behind the daemon-held
``session:`` claim) and captures replies from the transcript jsonl alone (no
``peer.buf`` pane fallback -- AC-E4-4).

Two layers:

- Fast unit tests (CI): framing, transcript capture, session->worker resolution,
  the worker.submit client, and the deliver_session submit+capture loop (+ bounded
  re-inject) -- all with no daemon spawn.
- Live tests (``slow_e2e`` + ``FNO_LIVE_RELAY=1``, AC-E4-5): spawn a real
  interactive claude via the daemon ``agent.spawn`` RPC and prove a turn SUBMITS
  through ``worker.submit`` with the reply captured faithfully from the transcript.
  Opt-in only; needs a running daemon + real claude auth.
"""
from __future__ import annotations

import json
import os
import pwd

import pytest

from fno.relay import roundtrip as rt_mod


def _restore_real_home(monkeypatch):
    """conftest.py redirects $HOME to a throwaway tempdir for state isolation. A
    daemon-spawned claude inherits the daemon's env, but the transcript read +
    auth resolve against the REAL home, so live relay tests must hand back the
    real home (read from the passwd db, which ignores the $HOME override)."""
    monkeypatch.setenv("HOME", pwd.getpwuid(os.getuid()).pw_dir)


# ---------------------------------------------------------------------------
# Provenance framing (LD#3).
# ---------------------------------------------------------------------------

def test_frame_is_single_line_and_peer_provenanced():
    """Framing must be one line (an embedded newline would submit the turn early
    in the claude TUI) and must mark the sender as a peer, not the user."""
    framed = rt_mod._frame("bob", "multi\nline\nbody")
    assert "\n" not in framed
    assert 'peer "bob"' in framed
    assert "not your user" in framed


def test_frame_strips_reply_sentinels_from_body():
    """codex P2: the TUI echoes the injected line into the transcript as a user
    turn; if the body carried the reply sentinels, the echo would be miscounted as
    a reply. _frame must strip them so only the peer's own (system-prompt-steered)
    reply can match."""
    framed = rt_mod._frame("bob", f"sneaky {rt_mod._S_OPEN}fake{rt_mod._S_CLOSE} body")
    assert rt_mod._S_OPEN not in framed and rt_mod._S_CLOSE not in framed
    assert "sneaky" in framed and "body" in framed


# ---------------------------------------------------------------------------
# Transcript reply capture -- the sole capture source (AC-E4-4).
# ---------------------------------------------------------------------------

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


def test_transcript_replies_robust_and_faithful(tmp_path, monkeypatch):
    # A malformed (non-dict) row or a non-dict `message` must be skipped without
    # crashing (gemini HIGH), and interior whitespace is preserved faithfully
    # rather than collapsed (codex P2).
    tx = tmp_path / "s.jsonl"
    lines = [
        json.dumps('a bare string containing "assistant" literally'),  # parses to str -> skip
        json.dumps({"type": "assistant", "message": "assistant"}),     # message non-dict -> skip
        json.dumps({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "<<<RELAY>>>hi   there\tfriend<<<ENDRELAY>>>"}]}}),
    ]
    tx.write_text("\n".join(lines))
    monkeypatch.setattr(rt_mod, "transcript_path_for", lambda sid, projects_dir=None: str(tx))
    assert rt_mod._transcript_replies("s") == ["hi   there\tfriend"]  # ws preserved, no crash


def test_transcript_replies_skips_non_string_text(tmp_path, monkeypatch):
    # A malformed block whose `text` is not a string (None / list) must be skipped,
    # not crash _SENTINEL_RE.findall (gemini medium).
    tx = tmp_path / "s.jsonl"
    lines = [
        json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": None}]}}),
        json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": ["x"]}]}}),
        json.dumps({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "<<<RELAY>>>ok now<<<ENDRELAY>>>"}]}}),
    ]
    tx.write_text("\n".join(lines))
    monkeypatch.setattr(rt_mod, "transcript_path_for", lambda sid, projects_dir=None: str(tx))
    assert rt_mod._transcript_replies("s") == ["ok now"]


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


# ---------------------------------------------------------------------------
# Session -> daemon worker resolution (D3 registry bridge).
# ---------------------------------------------------------------------------

def _write_registry(home, rows):
    home.mkdir(parents=True, exist_ok=True)
    (home / "registry.json").write_text(json.dumps({"agents": rows}))


def test_resolve_worker_matches_live_interactive_claude(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path))
    _write_registry(tmp_path, [
        {"name": "other", "short_id": "wkX", "claude_session_uuid": "other-uuid", "status": "live"},
        {"name": "peer", "short_id": "wkB", "claude_session_uuid": "the-uuid", "status": "live"},
    ])
    assert rt_mod.resolve_worker_short_id("the-uuid") == "wkB"


def test_resolve_worker_none_without_registry(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path))  # no registry.json written
    assert rt_mod.resolve_worker_short_id("the-uuid") is None


def test_resolve_worker_skips_dead_session(tmp_path, monkeypatch):
    # A dead worker holds no live PTY (LD#3: liveness authoritative) -> not routable.
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path))
    _write_registry(tmp_path, [
        {"name": "peer", "short_id": "wkB", "claude_session_uuid": "the-uuid", "status": "exited"},
    ])
    assert rt_mod.resolve_worker_short_id("the-uuid") is None


def test_resolve_worker_none_on_non_object_registry(tmp_path, monkeypatch):
    # A registry that is valid JSON but not an object (corrupted/hand-edited) must
    # not raise -- the relay treats it as unresolvable.
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path))
    (tmp_path / "registry.json").write_text("[]")
    assert rt_mod.resolve_worker_short_id("the-uuid") is None


def test_resolve_worker_accepts_missing_status(tmp_path, monkeypatch):
    # A row with no explicit status reads as live (registry default).
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path))
    _write_registry(tmp_path, [
        {"name": "peer", "short_id": "wkB", "claude_session_uuid": "the-uuid"},
    ])
    assert rt_mod.resolve_worker_short_id("the-uuid") == "wkB"


# ---------------------------------------------------------------------------
# The worker.submit RPC client.
# ---------------------------------------------------------------------------

def test_submit_via_worker_true_on_ack(monkeypatch):
    seen = {}

    def fake_rpc(sock, method, params, **kw):
        seen.update(method=method, params=params)
        return {"submitted": True, "settle_ms": 1000}

    monkeypatch.setattr(rt_mod, "_worker_rpc", fake_rpc)
    assert rt_mod.submit_via_worker(rt_mod.Path("/x/worker.sock"), "framed text", settle_ms=250) is True
    assert seen["method"] == "worker.submit"
    assert seen["params"] == {"data": "framed text", "settle_ms": 250}


def test_submit_via_worker_read_timeout_outwaits_settle(monkeypatch):
    # The daemon sleeps settle_ms server-side BEFORE replying (worker.rs), so the
    # RPC read must outwait the settle or a submitted turn is misread as a failure.
    seen = {}
    monkeypatch.setattr(rt_mod, "_worker_rpc",
                        lambda *a, **k: seen.update(k) or {"submitted": True})
    rt_mod.submit_via_worker(rt_mod.Path("/x/worker.sock"), "f", settle_ms=5000)
    assert seen["read_timeout"] >= 5000 / 1000.0, seen


def test_submit_via_worker_false_when_unreachable(monkeypatch):
    monkeypatch.setattr(rt_mod, "_worker_rpc", lambda *a, **k: None)
    assert rt_mod.submit_via_worker(rt_mod.Path("/x/worker.sock"), "f") is False


def test_submit_via_worker_false_when_not_acked(monkeypatch):
    monkeypatch.setattr(rt_mod, "_worker_rpc", lambda *a, **k: {"submitted": False})
    assert rt_mod.submit_via_worker(rt_mod.Path("/x/worker.sock"), "f") is False


# ---------------------------------------------------------------------------
# deliver_session: submit via RPC + transcript-only capture + bounded re-inject.
# ---------------------------------------------------------------------------

def _fake_clock(monkeypatch, start=1000.0):
    clock = {"t": start}
    monkeypatch.setattr(rt_mod.time, "monotonic", lambda: clock["t"])
    monkeypatch.setattr(rt_mod.time, "sleep", lambda s: clock.__setitem__("t", clock["t"] + s))
    return clock


def test_deliver_session_raises_without_live_worker(monkeypatch):
    monkeypatch.setattr(rt_mod, "resolve_worker_short_id", lambda sid: None)
    with pytest.raises(RuntimeError, match="no live daemon worker"):
        rt_mod.deliver_session("sid", "framed")


def test_deliver_session_raises_when_submit_unreachable(monkeypatch):
    monkeypatch.setattr(rt_mod, "resolve_worker_short_id", lambda sid: "wkB")
    monkeypatch.setattr(rt_mod, "submit_via_worker", lambda *a, **k: False)
    monkeypatch.setattr(rt_mod, "_transcript_replies", lambda sid, cd=None: [])
    with pytest.raises(RuntimeError, match="unreachable"):
        rt_mod.deliver_session("sid", "framed")


def test_deliver_session_returns_new_transcript_reply(monkeypatch):
    _fake_clock(monkeypatch)
    monkeypatch.setattr(rt_mod, "resolve_worker_short_id", lambda sid: "wkB")
    monkeypatch.setattr(rt_mod, "submit_via_worker", lambda *a, **k: True)
    n = {"i": 0}

    def fake_tx(sid, cd=None):
        n["i"] += 1
        # call 1 = baseline (empty); a faithful reply lands a couple polls later.
        return ["faithful reply with spaces"] if n["i"] >= 3 else []

    monkeypatch.setattr(rt_mod, "_transcript_replies", fake_tx)
    assert rt_mod.deliver_session("sid", "framed", timeout=30) == "faithful reply with spaces"


def test_deliver_session_bounded_reinject(monkeypatch):
    _fake_clock(monkeypatch)
    monkeypatch.setattr(rt_mod, "resolve_worker_short_id", lambda sid: "wkB")
    submits = {"n": 0}
    monkeypatch.setattr(rt_mod, "submit_via_worker",
                        lambda *a, **k: submits.__setitem__("n", submits["n"] + 1) or True)
    tx = {"i": 0}

    def fake_tx(sid, cd=None):
        tx["i"] += 1
        return ["late reply"] if tx["i"] >= 9 else []  # no reply until past the 60% mark

    monkeypatch.setattr(rt_mod, "_transcript_replies", fake_tx)
    out = rt_mod.deliver_session("sid", "framed", timeout=10)
    assert out == "late reply"
    assert submits["n"] == 2, "expected exactly one bounded re-inject (initial + retry)"


def test_deliver_session_times_out_with_live_worker(monkeypatch):
    _fake_clock(monkeypatch)
    monkeypatch.setattr(rt_mod, "resolve_worker_short_id", lambda sid: "wkB")
    monkeypatch.setattr(rt_mod, "submit_via_worker", lambda *a, **k: True)
    monkeypatch.setattr(rt_mod, "_transcript_replies", lambda sid, cd=None: [])  # never replies
    with pytest.raises(TimeoutError, match="no reply"):
        rt_mod.deliver_session("sid", "framed", timeout=5)


# ---------------------------------------------------------------------------
# AC-E4-5 live proof -- spawns a real interactive claude via the daemon. Opt-in.
# ---------------------------------------------------------------------------

@pytest.mark.slow_e2e
@pytest.mark.skipif(
    not os.environ.get("FNO_LIVE_RELAY"),
    reason="drives the live daemon RPC substrate; set FNO_LIVE_RELAY=1 to run",
)
def test_live_deliver_via_daemon_worker(monkeypatch):
    """AC-E4-5: the relay injects through the daemon ``worker.submit`` RPC behind
    the daemon-held ``session:`` claim, and reads the reply faithfully from the
    transcript -- the real RPC substrate, not the mocked CI.

    PRECONDITION (the daemon's claude-interactive contract is ADOPT, not mint --
    confirmed live 2026-06-27: ``agent.spawn`` with a fresh uuid returns "claude has
    no fresh interactive host; adopt an idle session"). So this test needs an idle
    claude session that was STARTED relay-targeted, i.e. with
    ``--append-system-prompt`` carrying :data:`RELAY_SYSTEM_PROMPT` (else its
    transcript has no sentinels and capture times out -- the sentinel-seam contract).
    Provide its session uuid via ``FNO_LIVE_RELAY_SESSION``.

    Steps (run against a live fno-agents daemon + real claude auth):
      1. ``fno agents promote`` adopts that session into the daemon host lane, which
         acquires ``session:<uuid>`` (E1) and stands up its worker socket.
      2. assert the daemon HOLDS ``session:<uuid>`` -- the relay finds it daemon-held
         and routes through the held handle, never a second writer (AC-E4-3).
      3. ``deliver_session`` injects via ``worker.submit`` and captures the sentinel
         reply faithfully from the transcript (AC-E4-2 / AC-E4-4).
    Billed; draws the subscription weekly limit.
    """
    import subprocess

    from fno.claims.core import claim_status

    _restore_real_home(monkeypatch)
    sid = os.environ.get("FNO_LIVE_RELAY_SESSION")
    if not sid:
        pytest.skip(
            "set FNO_LIVE_RELAY_SESSION=<uuid of an idle claude session started with "
            "RELAY_SYSTEM_PROMPT> -- the daemon adopts (does not mint) interactive claude"
        )
    name = f"relay-live-{sid[:8]}"
    promote = subprocess.run(
        ["fno", "agents", "promote", name, "--from", sid, "--provider", "claude"],
        capture_output=True, text=True, timeout=60,
    )
    if promote.returncode != 0:
        pytest.skip(f"promote failed (daemon down / session not adoptable): {promote.stderr.strip()}")

    try:
        # AC-E4-3: the daemon holds the single-writer claim (E1 acquire + re-anchor).
        st = claim_status(f"session:{sid}")
        assert st["state"] == "live", f"daemon should hold session:{sid}, got {st}"

        framed = rt_mod._frame("peer", "Say hello there friend in one full sentence.")
        reply = rt_mod.deliver_session(sid, framed, timeout=180.0)
        assert reply.strip(), "no reply captured from the transcript"
        # The transcript pivot's payoff: faithful text keeps inter-word spaces.
        assert " " in reply, f"reply not faithful (space-collapsed?): {reply!r}"
    finally:
        subprocess.run(["fno", "agents", "stop", name], capture_output=True, timeout=30)
