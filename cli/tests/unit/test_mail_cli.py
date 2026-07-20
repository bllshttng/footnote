"""Integration tests for the `fno mail` CLI surface (ab-cee91152).

Messaging is one namespace over the jsonl-canon bus log: `mail send` publishes
a durable envelope; `mail unread`/`ack` are the per-recipient cursor consume;
`mail rebuild-render` regenerates the derived markdown from the log. The old
`fno inbox` namespace is retired clean (no pointer, no shim).
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner():
    return CliRunner()


def isolate_mailbox(tmp_path, monkeypatch):
    """Co-isolate the md render (FNO_INBOX_ROOT), the bus log, and the roster under tmp.

    A plain helper (not a fixture) so tests can invoke it imperatively after
    seeding an ambient env, without depending on fixture setup ordering.
    """
    # FNO_BUS_DIR outranks FNO_INBOX_ROOT in bus_dir(); clear it so FNO_INBOX_ROOT wins.
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    # FNO_CLAUDE_DAEMON_DIR is process-global (set by spawn_gate) and defaults to the
    # real ~/.claude/daemon; pin an empty dir so no real live session leaks in. Tests
    # that need a roster (_isolate_claude_roster) setenv after this and win.
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    """Co-isolate the md render (FNO_INBOX_ROOT), the bus log, and the roster under tmp."""
    isolate_mailbox(tmp_path, monkeypatch)
    return tmp_path


# ---------------------------------------------------------------------------
# AC5-HP: `fno inbox` is gone clean (no redirect)
# ---------------------------------------------------------------------------

def test_inbox_namespace_is_retired(runner, mailbox):
    res = runner.invoke(app, ["inbox", "unread"])
    assert res.exit_code != 0
    assert "No such command 'inbox'" in (res.stdout + (res.stderr or ""))


# ---------------------------------------------------------------------------
# AC1-HP / AC2-HP: publish durable-first, cursor-consume, ack advances cursor
# ---------------------------------------------------------------------------

def test_send_then_unread_then_ack(runner, mailbox):
    sent = runner.invoke(
        app,
        ["mail", "send", "--to-project", "web", "--kind", "fyi",
         "--from-name", "etl", "--body", "build is green"],
    )
    assert sent.exit_code == 0, sent.output

    listing = runner.invoke(app, ["mail", "unread", "--name", "web", "--json"])
    assert listing.exit_code == 0, listing.output
    msgs = json.loads(listing.stdout.strip().splitlines()[-1])
    assert [m["body"] for m in msgs] == ["build is green"]
    msg_id = msgs[0]["id"]

    acked = runner.invoke(app, ["mail", "ack", msg_id, "--name", "web"])
    assert acked.exit_code == 0, acked.output

    after = runner.invoke(app, ["mail", "unread", "--name", "web", "--json"])
    assert json.loads(after.stdout.strip().splitlines()[-1]) == []


# ---------------------------------------------------------------------------
# AC1-EDGE: a deleted render is regenerated from the log, no message lost
# ---------------------------------------------------------------------------

def test_rebuild_render_regenerates_from_log(runner, mailbox):
    from fno.inbox.store import inbox_dir_for

    runner.invoke(
        app, ["mail", "send", "--to-project", "web", "--kind", "fyi",
              "--from-name", "etl", "--body", "durable note"],
    )
    inbox = inbox_dir_for("web")
    for p in inbox.glob("*.md"):
        p.unlink()
    assert list(inbox.glob("*.md")) == []

    res = runner.invoke(app, ["mail", "rebuild-render", "web", "--json"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout.strip().splitlines()[-1])
    assert payload["threads"] == 1
    assert list(inbox.glob("*.md"))  # render regenerated from the canonical log


# ---------------------------------------------------------------------------
# US5 / AC2-HP: a session drains its OWN cross-harness inbox and acks it
# ---------------------------------------------------------------------------

def _seed_bus_message(*, to: str, from_: str, body: str):
    """Append one durable bus envelope addressed to <to> (bypasses resolution)."""
    from fno.inbox.store import write_new_thread

    return write_new_thread(
        recipient=to, sender=from_, kind="send", body=body, to_kind="name"
    )


def test_drain_self_reads_own_handle_and_acks(runner, mailbox, monkeypatch):
    # AC1-EDGE: mail queued to the LEGACY <harness>-<short8> address before the
    # handle flip still drains exactly once after it, and acks under the new key.
    monkeypatch.setenv("CODEX_THREAD_ID", "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4")
    _seed_bus_message(to="codex-019f48e1", from_="claude-web", body="ack from K")

    res = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout.strip().splitlines()[-1])
    assert [m["body"] for m in payload] == ["ack from K"]
    assert payload[0]["to"] == "codex-019f48e1"

    # Ack advanced the cursor: a second drain sees nothing (not re-surfaced).
    again = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert json.loads(again.stdout.strip().splitlines()[-1]) == []


def test_drain_self_human_render_surfaces_id_and_reply_hint(runner, mailbox, monkeypatch):
    # The receive-path render must show each message's id (what `reply --to`
    # correlates against) and the how-to, so a draining agent can answer.
    monkeypatch.setenv("CODEX_THREAD_ID", "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4")
    h = _seed_bus_message(to="codex-019f48e1", from_="claude-web", body="need a decision")

    res = runner.invoke(app, ["mail", "drain-self"])  # human path (no --json)
    assert res.exit_code == 0, res.output
    assert f"id:{h.thread_id}" in res.output
    assert "fno mail reply --to <id>" in res.output


# ---------------------------------------------------------------------------
# Handle-flip migration: the cursor filename IS the address, so renaming the
# address orphans the cursor. Adoption carries the read position over ONCE.
# ---------------------------------------------------------------------------

CODEX_SID = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"


def test_ac2_edge_legacy_read_position_honored_no_history_replay(runner, mailbox, monkeypatch):
    """A recipient holding only a legacy cursor does not re-read what it already
    consumed under that address on its first post-flip drain."""
    from fno.bus.cursor import read_cursor, write_cursor

    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_SID)
    old = _seed_bus_message(to="codex-019f48e1", from_="web", body="ancient history")
    write_cursor("codex-019f48e1", old.thread_id)  # already consumed, pre-flip
    _seed_bus_message(to="019f48e1", from_="web", body="fresh")

    res = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert res.exit_code == 0, res.output
    assert [m["body"] for m in json.loads(res.stdout.strip().splitlines()[-1])] == ["fresh"]
    assert read_cursor("019f48e1") is not None  # acked under the live key


def test_mixed_version_bare_mail_is_not_stranded(runner, mailbox, monkeypatch):
    """Mixed-version window: a sender that flipped early addressed the bare name
    while this consumer was still draining the legacy one, so the bare message
    sits BEFORE the legacy cursor. It must still drain."""
    from fno.bus.cursor import write_cursor

    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_SID)
    _seed_bus_message(to="019f48e1", from_="early-flipper", body="bare, sent early")
    legacy = _seed_bus_message(to="codex-019f48e1", from_="web", body="legacy, consumed")
    write_cursor("codex-019f48e1", legacy.thread_id)

    res = runner.invoke(app, ["mail", "drain-self", "--json"])
    bodies = [m["body"] for m in json.loads(res.stdout.strip().splitlines()[-1])]
    assert bodies == ["bare, sent early"]  # delivered, and the consumed one is not replayed


def test_each_address_is_bounded_by_its_own_cursor(runner, mailbox, monkeypatch):
    """Per-address watermarks: consumed legacy mail stays consumed while mail to
    the never-consumed bare address is still delivered, even though the bare
    message sits EARLIER in the log than the legacy cursor. One shared watermark
    cannot do both - it either replays the first or strands the second."""
    from fno.bus.cursor import scan_unread, write_cursor

    _seed_bus_message(to="019f48e1", from_="early-flipper", body="bare, never consumed")
    consumed = _seed_bus_message(to="codex-019f48e1", from_="web", body="legacy, consumed")
    _seed_bus_message(to="codex-019f48e1", from_="web", body="legacy, still unread")
    write_cursor("codex-019f48e1", consumed.thread_id)

    bodies = [m.body for m in scan_unread("019f48e1", aliases=("codex-019f48e1",))]
    assert bodies == ["bare, never consumed", "legacy, still unread"]


def test_shared_cursor_id_does_not_strand_either_address(runner, mailbox, monkeypatch):
    """A drain advances every alias to the SAME id, so both addresses routinely
    share a cursor id. Both must resume from it - keeping one owner per id leaves
    the other permanently unpassed and silently eats its mail."""
    from fno.bus.cursor import scan_unread, write_cursor

    shared = _seed_bus_message(to="019f48e1", from_="web", body="drained")
    write_cursor("019f48e1", shared.thread_id)
    write_cursor("codex-019f48e1", shared.thread_id)  # what drain-self leaves behind
    _seed_bus_message(to="019f48e1", from_="web", body="after, bare")
    _seed_bus_message(to="codex-019f48e1", from_="web", body="after, legacy")

    bodies = [m.body for m in scan_unread("019f48e1", aliases=("codex-019f48e1",))]
    assert bodies == ["after, bare", "after, legacy"]  # neither lane stranded


def test_read_only_scan_honors_legacy_position_without_writing(runner, mailbox, monkeypatch):
    """SessionStart runs whoami before drain-self, so a read-only counter must
    already honor the legacy read position - otherwise it reports the entire
    retained history as unread - and it must not write a cursor to do so."""
    from fno.bus.cursor import cursor_path, scan_unread, write_cursor

    consumed = _seed_bus_message(to="codex-019f48e1", from_="web", body="already read")
    write_cursor("codex-019f48e1", consumed.thread_id)

    assert scan_unread("019f48e1", aliases=("codex-019f48e1",)) == []
    assert not cursor_path("019f48e1").exists()  # the read stayed read-only


def test_drain_advances_every_alias_cursor(runner, mailbox, monkeypatch):
    """The sender's unclaimed-mail check reads the cursor named by the envelope's
    `to`, so a retired alias cursor left frozen reports delivered mail as
    unclaimed forever."""
    from fno.bus.cursor import read_cursor

    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_SID)
    m = _seed_bus_message(to="codex-019f48e1", from_="web", body="legacy-addressed")

    runner.invoke(app, ["mail", "drain-self", "--json"])
    assert read_cursor("019f48e1") == m.thread_id
    assert read_cursor("codex-019f48e1") == m.thread_id  # alias tracked, not frozen


def test_ac1_fr_corrupt_legacy_cursor_repeats_never_loses(runner, mailbox, monkeypatch):
    """A corrupt legacy cursor degrades to the rescan posture: everything
    addressed to either form re-surfaces (a repeat), and the next drain is quiet."""
    from fno.bus.cursor import cursor_path

    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_SID)
    _seed_bus_message(to="codex-019f48e1", from_="web", body="legacy-addressed")
    _seed_bus_message(to="019f48e1", from_="web", body="bare-addressed")
    p = cursor_path("codex-019f48e1")
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("{not json", encoding="utf-8")

    res = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert res.exit_code == 0, res.output
    bodies = [m["body"] for m in json.loads(res.stdout.strip().splitlines()[-1])]
    assert bodies == ["legacy-addressed", "bare-addressed"]  # nothing lost
    again = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert json.loads(again.stdout.strip().splitlines()[-1]) == []  # quiet after


def test_ac2_fr_interrupted_drain_resurfaces_under_new_key(runner, mailbox, monkeypatch):
    """Inject-before-ack survives the rename: a drain that never acks re-surfaces
    its messages rather than dropping them."""
    from fno.bus import cursor as cursor_mod

    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_SID)
    _seed_bus_message(to="codex-019f48e1", from_="web", body="mid-flight")
    # Restore by hand, not monkeypatch.undo() - undo would also revert the
    # mailbox isolation and point the second drain at a different bus.
    real_advance = cursor_mod.advance_cursor

    def _crash(*_a, **_kw):
        raise RuntimeError("crash between print and ack")

    cursor_mod.advance_cursor = _crash
    try:
        runner.invoke(app, ["mail", "drain-self", "--json"])  # prints, never acks
    finally:
        cursor_mod.advance_cursor = real_advance

    res = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert [m["body"] for m in json.loads(res.stdout.strip().splitlines()[-1])] == ["mid-flight"]


def test_drain_self_no_harness_env_is_noop(runner, mailbox, monkeypatch):
    for var in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID",
                "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    res = runner.invoke(app, ["mail", "drain-self"])
    assert res.exit_code == 0
    assert res.stdout.strip() == ""


# ---------------------------------------------------------------------------
# US7(a) / AC1-HP + AC2-HP: send to a DISK-DISCOVERED codex session (never
# registered) is addressed to its handle and drained by drain-self. Full round.
# ---------------------------------------------------------------------------

def _write_codex_rollout(codex_dir, *, session_id, cwd):
    import json as _json
    import os as _os
    import time as _time

    day = codex_dir / "2026" / "07" / "09"
    day.mkdir(parents=True, exist_ok=True)
    f = day / f"rollout-2026-07-09T00-00-00-{session_id}.jsonl"
    f.write_text(
        _json.dumps({"type": "session_meta", "payload": {"id": session_id, "cwd": cwd}}) + "\n",
        encoding="utf-8",
    )
    mt = _time.time() - 5.0
    _os.utime(f, (mt, mt))


def _isolate_codex_discovery(monkeypatch, tmp_path, *, session_id):
    from fno.agents import discover

    codex_dir = tmp_path / "codex"
    _write_codex_rollout(codex_dir, session_id=session_id, cwd="/Users/x/proj")
    empty = tmp_path / "empty"
    empty.mkdir()
    monkeypatch.setenv(discover.SESSIONS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(codex_dir))


def test_us7a_send_to_disk_discovered_codex_round_trips(runner, mailbox, monkeypatch, tmp_path):
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _isolate_codex_discovery(monkeypatch, tmp_path, session_id=sid)
    # US8: no live daemon in the test env -- force the codex live-inject miss so
    # the send deterministically writes the durable floor (round-trip target).
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a: False)

    # A claude session sends to the codex handle (codex row is unregistered, so
    # the send falls through registry-unknown into disk resolution).
    sent = runner.invoke(
        app, ["mail", "send", "codex-019f48e1", "ack from K", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "019f48e1" in sent.output
    assert "queued (durable)" in sent.output

    # The codex session drains its own handle and sees the message.
    monkeypatch.setenv("CODEX_THREAD_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert drained.exit_code == 0, drained.output
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload and payload[0]["to"] == "019f48e1"
    assert "ack from K" in payload[0]["body"]  # inside the <fno_mail> wrap


def test_us8_codex_live_inject_hosted_short_circuits_durable(
    runner, mailbox, monkeypatch, tmp_path
):
    # US8 (node x-d899): a running codex daemon takes the turn LIVE, so the send
    # reports "delivered (hosted)" and writes NO durable thread.
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _isolate_codex_discovery(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a: True)

    sent = runner.invoke(
        app, ["mail", "send", "codex-019f48e1", "ack from K", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "delivered (hosted)" in sent.output
    assert "queued (durable)" not in sent.output

    # No durable thread was written: drain-self sees nothing for the handle.
    monkeypatch.setenv("CODEX_THREAD_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert drained.exit_code == 0, drained.output
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload == []


# ---------------------------------------------------------------------------
# a2a US7b / AC3-HP: the mux PaneSend live rung. A resolved session that is
# mux-hosted (fno owns its PTY) delivers live through its pane instead of
# demoting to the durable bus. Provider-agnostic by construction: the rung keys
# off the roster entry's pane info, never a provider name.
# ---------------------------------------------------------------------------


def _stub_pane_rung(
    monkeypatch, *, in_roster: bool, pane_sends: bool, expect_token: str,
    status: str = "live",
) -> list:
    """Wire the pane rung to a fake roster entry and a recorded ``_mux_pane_send``.
    Returns the call log, so a test can assert the rung was skipped entirely.

    ``expect_token`` is asserted inside the resolver: the rung must look up the
    full session id, not the display handle. The two pick different match rules
    in resolve_agent_in (full_session_id vs derived_short), so a swap changes
    real behavior while leaving every outcome assertion green."""
    from types import SimpleNamespace

    from fno.agents.registry import AgentResolutionError, ResolvedAgent

    entry = SimpleNamespace(
        name="hosted-worker", status=status, mux={"session": "main", "pane_id": 3}
    )

    def _resolve(token, **_kw):
        assert token == expect_token, f"rung resolved {token!r}, expected the session id"
        if not in_roster:
            raise AgentResolutionError(f"no agent matching {token!r}")
        return ResolvedAgent(entry=entry, matched_by="full_session_id")

    calls: list = []

    def _send(resolved_entry, text):
        calls.append((resolved_entry, text))
        return pane_sends

    monkeypatch.setattr("fno.agents.registry.resolve_agent", _resolve)
    monkeypatch.setattr("fno.agents.dispatch._mux_pane_send", _send)
    return calls


def test_us7b_mux_pane_rung_delivers_live_when_socket_inject_misses(
    runner, mailbox, monkeypatch, tmp_path
):
    """Codex daemon socket is dead but the session is mux-hosted: the pane rung
    takes the turn live, so no durable thread is written."""
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _isolate_codex_discovery(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a: False)
    calls = _stub_pane_rung(
        monkeypatch, in_roster=True, pane_sends=True, expect_token=sid
    )

    sent = runner.invoke(
        app, ["mail", "send", "codex-019f48e1", "ping", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "delivered (hosted)" in sent.output
    assert "queued (durable)" not in sent.output
    assert len(calls) == 1
    assert "ping" in calls[0][1]  # the wrapped <fno_mail> envelope, not raw text

    monkeypatch.setenv("CODEX_THREAD_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert json.loads(drained.stdout.strip().splitlines()[-1]) == []


def test_us7b_mux_pane_send_failure_falls_closed_to_durable(
    runner, mailbox, monkeypatch, tmp_path
):
    """Roster entry exists but the pane send fails (mux gone, claim lost): the
    message must land on the durable floor, never vanish."""
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _isolate_codex_discovery(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a: False)
    calls = _stub_pane_rung(
        monkeypatch, in_roster=True, pane_sends=False, expect_token=sid
    )

    sent = runner.invoke(
        app, ["mail", "send", "codex-019f48e1", "ping", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "queued (durable)" in sent.output
    assert len(calls) == 1

    monkeypatch.setenv("CODEX_THREAD_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload and "ping" in payload[0]["body"]


def test_us7b_unrostered_session_skips_pane_rung_silently(
    runner, mailbox, monkeypatch, tmp_path
):
    """Not mux-hosted is the common case, not an error: resolution failure falls
    through to the durable floor without a crash or a stderr complaint."""
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _isolate_codex_discovery(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a: False)
    calls = _stub_pane_rung(
        monkeypatch, in_roster=False, pane_sends=True, expect_token=sid
    )

    sent = runner.invoke(
        app, ["mail", "send", "codex-019f48e1", "ping", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "queued (durable)" in sent.output
    assert calls == []
    assert "no agent matching" not in sent.output


def test_us7b_working_socket_inject_preempts_pane_rung(
    runner, mailbox, monkeypatch, tmp_path
):
    """Rung ordering: a socket inject is less invasive than typing into a live
    TUI, so a working control.sock means the pane is never touched."""
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: True)
    calls = _stub_pane_rung(
        monkeypatch, in_roster=True, pane_sends=True, expect_token=sid
    )

    sent = runner.invoke(
        app, ["mail", "send", "claude-9a063cd3", "hi", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "delivered (hosted)" in sent.output
    assert calls == []


@pytest.mark.parametrize("status", ["exited", "orphaned", "idle", "permanent_dead"])
def test_us7b_non_live_entry_never_pane_sends(
    runner, mailbox, monkeypatch, tmp_path, status
):
    """Only a "live" row may be pane-sent. A non-live row keeps its mux ref, and
    pane ids restart at 1 with the mux server, so sending on that ref would type
    into an unrelated pane and report hosted, suppressing the durable copy the
    real recipient needs. "idle" matters as much as "exited" here: it is the
    status a hand-started session registers under precisely because it has no
    live transport. Parameterized so weakening the gate to `!= "exited"` fails."""
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _isolate_codex_discovery(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a: False)
    calls = _stub_pane_rung(
        monkeypatch, in_roster=True, pane_sends=True, expect_token=sid, status=status
    )

    sent = runner.invoke(
        app, ["mail", "send", "codex-019f48e1", "ping", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "queued (durable)" in sent.output
    assert calls == []  # the stale pane was never written to

    monkeypatch.setenv("CODEX_THREAD_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload and "ping" in payload[0]["body"]


def test_us7b_rostered_but_paneless_entry_falls_to_durable(
    runner, mailbox, monkeypatch, tmp_path
):
    """Runs the REAL _mux_pane_send: an entry that is rostered but has no pane
    (never mux-hosted, or the pane is gone) must return False on its own
    predicate and demote to durable -- no subprocess, no hang. This is the test
    that would catch the rung handing _mux_pane_send the wrong object shape."""
    import subprocess
    from types import SimpleNamespace

    from fno.agents.registry import ResolvedAgent

    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _isolate_codex_discovery(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a: False)
    paneless = SimpleNamespace(name="not-hosted", status="live", mux=None)

    def _resolve(token, **_kw):
        assert token == sid, f"rung resolved {token!r}, expected the session id"
        return ResolvedAgent(entry=paneless, matched_by="full_session_id")

    monkeypatch.setattr("fno.agents.registry.resolve_agent", _resolve)
    # The mux predicate must short-circuit BEFORE shelling out. Without this the
    # test would still pass for the wrong reason: a real `fno mux pane claim`
    # against pane None fails and returns False anyway. Only mux calls are
    # trapped -- the send path legitimately shells out for other things.
    real_run = subprocess.run

    def _guard_run(args, *a, **kw):
        if isinstance(args, (list, tuple)) and "mux" in [str(x) for x in args]:
            pytest.fail(f"paneless entry must not shell out to mux: {args}")
        return real_run(args, *a, **kw)

    monkeypatch.setattr("fno.agents.dispatch.subprocess.run", _guard_run)

    sent = runner.invoke(
        app, ["mail", "send", "codex-019f48e1", "ping", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "queued (durable)" in sent.output

    monkeypatch.setenv("CODEX_THREAD_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload and "ping" in payload[0]["body"]


# ---------------------------------------------------------------------------
# x-605c US3 / AC1-HP + AC1-FR: send to a ROSTERED claude bg worker is
# handle-addressed (live-inject first, durable floor to its canonical handle);
# the old claude->project re-route is gone. US4/AC3-HP: the envelope carries the
# invoking session's real from + model.
# ---------------------------------------------------------------------------


def _isolate_claude_roster(monkeypatch, tmp_path, *, session_id):
    """Only the daemon roster resolves: empty disk sources + a fixture roster
    holding one rostered claude bg worker (no pid-sidecar)."""
    from fno.agents import discover

    empty = tmp_path / "empty"
    empty.mkdir(exist_ok=True)
    monkeypatch.setenv(discover.SESSIONS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(empty))
    daemon = tmp_path / "daemon"
    daemon.mkdir(parents=True, exist_ok=True)
    (daemon / "roster.json").write_text(
        json.dumps({"proto": 1, "workers": {
            session_id[:8]: {"sessionId": session_id, "pid": 4242, "cwd": "/Users/x/proj"}
        }}),
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))


def test_us3_rostered_claude_inject_miss_falls_to_drainable_floor(
    runner, mailbox, monkeypatch, tmp_path
):
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    # Force the claude live-inject miss so the send writes the durable floor.
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    sent = runner.invoke(
        app, ["mail", "send", "claude-9a063cd3", "hi bg worker", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "9a063cd3" in sent.output
    assert "queued (durable)" in sent.output

    # The bg worker drains its own handle and sees the message (stamp == drain key).
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert drained.exit_code == 0, drained.output
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload and payload[0]["to"] == "9a063cd3"
    assert "hi bg worker" in payload[0]["body"]


def test_us3_rostered_claude_hosted_short_circuits_durable(
    runner, mailbox, monkeypatch, tmp_path
):
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: True)

    sent = runner.invoke(
        app, ["mail", "send", "claude-9a063cd3", "hi", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output
    assert "delivered (hosted)" in sent.output
    assert "queued (durable)" not in sent.output

    # No durable thread was written.
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload == []


def test_ac3_hp_envelope_carries_real_from_and_model(
    runner, mailbox, monkeypatch, tmp_path
):
    recipient_sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=recipient_sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    # The SENDER is a claude session with its own transcript carrying a model.
    from fno.agents import discover

    sender_sid = "abcd1234-1111-2222-3333-444444444444"
    projects = tmp_path / "sender-projects"
    (projects / "-Users-x-proj").mkdir(parents=True, exist_ok=True)
    (projects / "-Users-x-proj" / f"{sender_sid}.jsonl").write_text(
        json.dumps(
            {
                "type": "assistant",
                "isSidechain": False,
                "message": {"model": "claude-opus-4-8"},
            }
        )
        + "\n",
        encoding="utf-8",
    )
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sender_sid)

    # No --from-name: from + model are auto-stamped from the invoking session.
    sent = runner.invoke(app, ["mail", "send", "claude-9a063cd3", "hello"])
    assert sent.exit_code == 0, sent.output

    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", recipient_sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    body = json.loads(drained.stdout.strip().splitlines()[-1])[0]["body"]
    assert 'from="abcd1234"' in body
    assert 'model="claude-opus-4-8"' in body


# ---------------------------------------------------------------------------
# Regression (x-3392): the mailbox fixture must neutralize an ambient, poisoning
# FNO_BUS_DIR / FNO_CLAUDE_DAEMON_DIR so mail-send tests stay hermetic under a
# symlinked worktree where a live worker session has exported them.
# ---------------------------------------------------------------------------

def test_mailbox_fixture_neutralizes_ambient_bus_dir(runner, monkeypatch, tmp_path):
    from fno.paths import bus_dir

    # Seed an ambient, poisoning FNO_BUS_DIR, then run the isolation helper
    # imperatively (as the mailbox fixture does) so ordering is explicit, not
    # dependent on fixture argument position.
    poison_bus = tmp_path / "poison-bus"
    poison_bus.mkdir()
    monkeypatch.setenv("FNO_BUS_DIR", str(poison_bus))
    isolate_mailbox(tmp_path, monkeypatch)

    # Root-cause invariant: FNO_BUS_DIR cleared ⟹ bus resolves under FNO_INBOX_ROOT.
    resolved = bus_dir()
    assert resolved == tmp_path / ".bus"
    assert resolved != poison_bus

    # And the send→drain round-trip works on the tmp bus, not the ambient one.
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)
    sent = runner.invoke(
        app, ["mail", "send", "claude-9a063cd3", "hi bg worker", "--from-name", "web"]
    )
    assert sent.exit_code == 0, sent.output

    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload and payload[0]["to"] == "9a063cd3"
    assert "hi bg worker" in payload[0]["body"]


# ---------------------------------------------------------------------------
# Dead-letterbox (x-730d): a project send with no live peer queues durably and
# fails loud on stderr so the sender knows delivery deferred (exit stays 0).
# ---------------------------------------------------------------------------

def test_project_send_no_peer_warns_deferred(runner, mailbox):
    res = runner.invoke(
        app, ["mail", "send", "--to-project", "web", "--from-name", "etl", "quiet?"]
    )
    assert res.exit_code == 0, res.output
    assert "queued (durable) for project web" in res.stdout
    assert "project inbox web has no live drain" in (res.stderr or "")
