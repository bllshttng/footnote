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
