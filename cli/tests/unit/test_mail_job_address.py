"""Job-addressed mail: node:<id> / pr:<n> resolves to the current claim holder
(x-8f8c part 2).

A job address names the work, not the process holding it, so it survives the
holder's death: the durable copy is addressed to ``node:<id>`` and a successor's
drain picks it up. These tests pin the two load-bearing guarantees:

- a job with no live holder is REFUSED (exit 16, nothing queued) -- queueing it
  would strand the message at the new address, the defect again one address over;
- a live holder delivers, and a live miss durable-floors to ``node:<id>`` (never
  the session handle);
- drain-self surfaces job mail only to the session that CURRENTLY holds the node.
"""
from __future__ import annotations

import json
import os

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner():
    return CliRunner()


def _isolate(tmp_path, monkeypatch):
    """Co-isolate claims root, bus log, inbox, and roster under tmp."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))
    use_tmpdir(monkeypatch, tmp_path)
    # drain-self reads the session manifest from cwd/.fno/target-state.md
    # (production: the session's project cwd), so run from the fixture dir.
    monkeypatch.chdir(tmp_path)
    return tmp_path


@pytest.fixture
def isolated(tmp_path, monkeypatch):
    return _isolate(tmp_path, monkeypatch)


def _acquire_node(node_id: str, session_id: str, *, harness: str = "claude") -> None:
    """Create a LIVE node:<id> claim held by target-session:<session_id>."""
    from fno.claims.core import acquire_claim

    acquire_claim(
        key=f"node:{node_id}",
        holder=f"target-session:{session_id}",
        ttl_ms=3_600_000,
        reason="test",
        harness=harness,
    )


def _bus_to(recipient: str):
    from fno.bus.log import iter_messages

    return [m for m in iter_messages() if m.to == recipient]


def _set_identity(monkeypatch, session_id: str) -> None:
    """Scrub every ambient harness marker, then pin THIS session.

    drain-self resolves identity from env markers; the live test host carries
    real ones (CLAUDE_CODE_SESSION_ID etc.) that outrank a naive setenv, so they
    must be cleared first or the drain reads the host session, not the fixture.
    """
    from fno.harness_identity import AMBIENT_IDENTITY_ENV

    for var in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", session_id)


def _write_target_state(
    repo_root, *, session_id: str, claim_key: str, pr_number=None
) -> None:
    """Write a minimal .fno/target-state.md so drain-self can find 'my job'."""
    lines = [
        f"session_id: {session_id}",
        f"target_claim_key: {claim_key}",
        f'target_claim_holder: "target-session:{session_id}"',
    ]
    if pr_number is not None:
        lines.append(f"pr_number: {pr_number}")
    (repo_root / ".fno").mkdir(parents=True, exist_ok=True)
    (repo_root / ".fno" / "target-state.md").write_text("\n".join(lines) + "\n")


# ---------------------------------------------------------------------------
# Send: no holder -> refuse, never queue (constraint 2)
# ---------------------------------------------------------------------------


def test_send_refuses_when_no_holder(runner, isolated, monkeypatch):
    # No claim acquired for node:free-abcd -> state free -> refuse.
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))
    res = runner.invoke(
        app,
        ["mail", "send", "node:free-abcd", "please review", "--from-name", "king"],
    )
    assert res.exit_code == 16, res.output
    assert "no live holder" in (res.stdout + (res.stderr or "")).lower()
    # Nothing was queued at any address.
    assert _bus_to("node:free-abcd") == []


def test_send_refuses_when_claim_stale(runner, isolated, monkeypatch):
    # A STALE claim (expired TTL) reads as non-live -> refuse, same path as free.
    from fno.claims.core import claim_status

    # Acquire with the minimum TTL, then rewrite the lock file to back-date its
    # expiry past the horizon so classify() reports STALE (not suspect/live).
    from fno.claims.io import claim_path

    acquire = __import__("fno.claims.core", fromlist=["acquire_claim"]).acquire_claim
    acquire(
        key="node:stale-abcd",
        holder="target-session:dead0000-0000-0000-0000-000000000000",
        ttl_ms=60_000,
        reason="test",
        harness="claude",
    )
    p = claim_path("node:stale-abcd")
    text = p.read_text()
    import re

    # Push expires_at into the distant past.
    text = re.sub(r"expires_at: \d+", "expires_at: 1000", text)
    text = re.sub(r"acquired_at: \d+", "acquired_at: 1000", text)
    p.write_text(text)
    assert claim_status("node:stale-abcd")["state"] == "stale"

    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))
    res = runner.invoke(
        app,
        ["mail", "send", "node:stale-abcd", "hello", "--from-name", "king"],
    )
    assert res.exit_code == 16, res.output
    assert _bus_to("node:stale-abcd") == []


# ---------------------------------------------------------------------------
# Send: live holder -> delivered (hosted), no durable copy
# ---------------------------------------------------------------------------


def test_send_live_holder_delivers_no_durable(runner, isolated, monkeypatch):
    _acquire_node("live-abcd", "11111111-1111-1111-1111-111111111111")
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))

    inject_calls: list[str] = []

    def _fake_inject(session_id, wrapped):
        inject_calls.append(session_id)
        return True  # confirmed delivery

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", _fake_inject)

    res = runner.invoke(
        app,
        ["mail", "send", "node:live-abcd", "ship it", "--from-name", "king"],
    )
    assert res.exit_code == 0, res.output
    assert "delivered (hosted)" in res.stdout
    # Inject targeted the holder's session, not a handle.
    assert inject_calls == ["11111111-1111-1111-1111-111111111111"]
    # A hosted send writes NO durable thread (live delivery is its own record).
    assert _bus_to("node:live-abcd") == []


# ---------------------------------------------------------------------------
# Send: live holder but inject misses -> durable to node:<id> (the JOB)
# ---------------------------------------------------------------------------


def test_send_live_miss_durables_to_job_address(runner, isolated, monkeypatch):
    _acquire_node("miss-abcd", "22222222-2222-2222-2222-222222222222")
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))

    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda sid, wrapped: False,  # live miss
    )

    res = runner.invoke(
        app,
        ["mail", "send", "node:miss-abcd", "try again", "--from-name", "king"],
    )
    assert res.exit_code == 0, res.output
    assert "queued (durable)" in res.stdout
    msgs = _bus_to("node:miss-abcd")
    assert len(msgs) == 1
    assert msgs[0].to == "node:miss-abcd"
    # Addressed to the JOB, never the holder's session handle.
    assert _bus_to("22222222") == []
    # Owner is wake-daemon: the holder exists but waits for a drain, not a turn.
    assert msgs[0].meta.get("owner") == "wake-daemon"
    assert msgs[0].to_kind == "node"


# ---------------------------------------------------------------------------
# Send: pr:<n> resolves to a node carrying that PR
# ---------------------------------------------------------------------------


def test_pr_resolves_to_node_and_delivers(runner, isolated, monkeypatch):
    # Seed the graph with one node carrying PR 4242, held live.
    monkeypatch.setattr(
        "fno.graph.store.read_graph",
        lambda *a, **k: [
            {"id": "x-pr42", "title": "pr node", "pr_number": 4242, "status": "ready"}
        ],
    )
    _acquire_node("x-pr42", "33333333-3333-3333-3333-333333333333")
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))

    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude", lambda sid, wrapped: True
    )

    res = runner.invoke(
        app, ["mail", "send", "pr:4242", "review plz", "--from-name", "king"]
    )
    assert res.exit_code == 0, res.output
    assert "node:x-pr42" in res.stdout


def test_pr_with_no_node_refuses(runner, isolated, monkeypatch):
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))
    res = runner.invoke(
        app, ["mail", "send", "pr:9991", "hello", "--from-name", "king"]
    )
    assert res.exit_code == 16, res.output
    assert _bus_to("node:") == []


def test_pr_refuses_when_ambiguous_across_nodes(runner, isolated, monkeypatch):
    # Two nodes carry PR 5050 (per-repo numbers collide on the global graph).
    # pr:<n> must refuse rather than silently route to one of them.
    monkeypatch.setattr(
        "fno.graph.store.read_graph",
        lambda *a, **k: [
            {"id": "x-a", "pr_number": 5050, "status": "ready"},
            {"id": "x-b", "pr_number": 5050, "status": "ready"},
        ],
    )
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))
    res = runner.invoke(
        app, ["mail", "send", "pr:5050", "hi", "--from-name", "king"]
    )
    assert res.exit_code == 16, res.output
    out = (res.stdout + (res.stderr or "")).lower()
    assert "ambiguous" in out
    assert _bus_to("node:") == []


def test_pr_resolves_via_additional_prs(runner, isolated, monkeypatch):
    # A PR carried only as a secondary entry (additional_prs) still resolves.
    monkeypatch.setattr(
        "fno.graph.store.read_graph",
        lambda *a, **k: [
            {
                "id": "x-multi",
                "pr_number": 100,
                "additional_prs": [{"number": 101, "url": "u"}],
                "status": "ready",
            }
        ],
    )
    _acquire_node("x-multi", "88888888-8888-8888-8888-888888888888")
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude", lambda sid, wrapped: True
    )
    res = runner.invoke(
        app, ["mail", "send", "pr:101", "secondary pr", "--from-name", "king"]
    )
    assert res.exit_code == 0, res.output
    assert "node:x-multi" in res.stdout


def test_pr_unicode_digit_refuses_cleanly(runner, isolated, monkeypatch):
    # A unicode digit that isdigit() accepts but int() rejects must refuse, not
    # crash with a traceback.
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))
    res = runner.invoke(
        app, ["mail", "send", "pr:²", "hi", "--from-name", "king"]
    )
    assert res.exit_code == 16, res.output
    assert "Traceback" not in res.output


# ---------------------------------------------------------------------------
# Reply: a node-addressed message is answered at its sender
# ---------------------------------------------------------------------------


def test_reply_to_node_message_routes_to_sender(runner, isolated, monkeypatch):
    # Seed a durable-floored job message (to_kind=node) from a known sender.
    from fno.bus.log import append as bus_append
    from fno.bus.log import Envelope
    from datetime import datetime, timezone

    sender_handle = "5555abcd"
    bus_append(
        Envelope(
            id="msg-node-1",
            thread="msg-node-1",
            from_=sender_handle,
            to="node:reply-abcd",
            kind="send",
            body="job note",
            ts=datetime.now(tz=timezone.utc).isoformat(),
            to_kind="node",
        )
    )
    _acquire_node("reply-abcd", "99999999-9999-9999-9999-999999999999")

    captured: list[str] = []

    def _capture(body, *, from_project, target, to_msg, require_resolution=False):
        captured.append(target)
        print(f"replied to {target}")
        return None

    monkeypatch.setattr("fno.mail.cli._reply_to_name_handle", _capture)
    res = runner.invoke(
        app,
        ["mail", "reply", "--to", "msg-node-1", "--body", "got it"],
    )
    assert res.exit_code == 0, res.output
    # Routed back to the original sender, not the job address.
    assert captured == [sender_handle]



# ---------------------------------------------------------------------------
# Drain: a holding session surfaces job mail; a non-holder does not
# ---------------------------------------------------------------------------


def test_drain_self_surfaces_job_mail_for_holder(runner, isolated, monkeypatch):
    sid = "44444444-4444-4444-4444-444444444444"
    _acquire_node("drain-abcd", sid)
    _write_target_state(isolated, session_id=sid, claim_key="node:drain-abcd")

    # Seed job-addressed mail on the bus (as if a sender durable-floored it).
    from fno.bus.log import append as bus_append
    from fno.bus.log import Envelope
    from datetime import datetime, timezone

    bus_append(
        Envelope(
            id="msg-drain-1",
            thread="msg-drain-1",
            from_="king",
            to="node:drain-abcd",
            kind="send",
            body="reached the successor",
            ts=datetime.now(tz=timezone.utc).isoformat(),
            to_kind="node",
        )
    )

    # Ambient identity = the holding session, so drain-self resolves to it.
    _set_identity(monkeypatch, sid)
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))

    res = runner.invoke(app, ["mail", "drain-self"])
    assert res.exit_code == 0, res.output
    assert "reached the successor" in res.stdout


def test_drain_self_skips_job_mail_when_not_holder(runner, isolated, monkeypatch):
    # Job mail exists for node:other-abcd, but THIS session holds a different node.
    _acquire_node("other-abcd", "55555555-5555-5555-5555-555555555555")
    sid = "66666666-6666-6666-6666-666666666666"
    _acquire_node("mine-abcd", sid)
    _write_target_state(isolated, session_id=sid, claim_key="node:mine-abcd")

    from fno.bus.log import append as bus_append
    from fno.bus.log import Envelope
    from datetime import datetime, timezone

    bus_append(
        Envelope(
            id="msg-other-1",
            thread="msg-other-1",
            from_="king",
            to="node:other-abcd",
            kind="send",
            body="not for me",
            ts=datetime.now(tz=timezone.utc).isoformat(),
            to_kind="node",
        )
    )

    _set_identity(monkeypatch, sid)
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))

    res = runner.invoke(app, ["mail", "drain-self"])
    assert res.exit_code == 0, res.output
    assert "not for me" not in res.stdout


def test_drain_self_surfaces_both_handle_and_job_mail(runner, isolated, monkeypatch):
    # The holder sees its own handle mail AND its job mail in one drain.
    sid = "77777777-7777-7777-7777-777777777777"
    _acquire_node("both-abcd", sid)
    _write_target_state(isolated, session_id=sid, claim_key="node:both-abcd")

    from fno.bus.log import append as bus_append
    from fno.bus.log import Envelope
    from datetime import datetime, timezone
    from fno.harness_identity import canonical_handle

    handle = canonical_handle(sid)
    ts = datetime.now(tz=timezone.utc).isoformat()
    bus_append(
        Envelope(
            id="msg-h-1", thread="msg-h-1", from_="a", to=handle, kind="send",
            body="handle mail", ts=ts,
        )
    )
    bus_append(
        Envelope(
            id="msg-j-1", thread="msg-j-1", from_="b", to="node:both-abcd",
            kind="send", body="job mail", ts=ts, to_kind="node",
        )
    )

    _set_identity(monkeypatch, sid)
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(isolated / "projects"))

    res = runner.invoke(app, ["mail", "drain-self"])
    assert res.exit_code == 0, res.output
    assert "handle mail" in res.stdout
    assert "job mail" in res.stdout
