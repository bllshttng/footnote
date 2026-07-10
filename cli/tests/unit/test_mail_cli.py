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


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    """Co-isolate the md render (FNO_INBOX_ROOT) and the bus log under tmp."""
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
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
    # A live codex session with this thread id -> own handle codex-019f48e1.
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
    assert "codex-019f48e1" in sent.output
    assert "queued (durable)" in sent.output

    # The codex session drains its own handle and sees the message.
    monkeypatch.setenv("CODEX_THREAD_ID", sid)
    drained = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert drained.exit_code == 0, drained.output
    payload = json.loads(drained.stdout.strip().splitlines()[-1])
    assert payload and payload[0]["to"] == "codex-019f48e1"
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
