"""Tests for `fno agents send --kind` (bus epic G4 / US8, AC8-HP).

The faithful replacement for `fno inbox send --to <project> --kind <kind>`:
inbox kinds (heads-up / question / fyi) route through the durable inbox
write path so the recipient's drain still dispatches by kind. Covers the
data-layer core (`post_inbox_message`) and the CLI flag wiring.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def isolated(tmp_path: Path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    monkeypatch.setenv("FNO_INBOX_TEST_MODE", "1")  # suppress desktop notify
    return tmp_path


# ---------------------------------------------------------------------------
# Data layer: post_inbox_message
# ---------------------------------------------------------------------------


def test_post_inbox_message_new_thread(isolated, monkeypatch) -> None:
    from fno.inbox.store import post_inbox_message, read_unread_threads

    res = post_inbox_message(
        recipient="acme-docs", sender="acme-web", kind="heads-up", body="ship X"
    )

    assert res.appended is False and res.orphan is False
    threads = read_unread_threads("acme-docs")
    assert len(threads) == 1
    assert threads[0].kind == "heads-up"
    assert threads[0].from_project == "acme-web"


def test_post_inbox_message_persist_requires_fyi(isolated) -> None:
    from fno.inbox.store import post_inbox_message

    with pytest.raises(ValueError, match="fyi"):
        post_inbox_message(
            recipient="x", sender="y", kind="heads-up",
            body="b", persist_to_memory=True,
        )


def test_post_inbox_message_invalid_kind(isolated) -> None:
    from fno.inbox.store import post_inbox_message

    with pytest.raises(ValueError, match="invalid kind"):
        post_inbox_message(recipient="x", sender="y", kind="bogus", body="b")


def test_post_inbox_message_reply_appends(isolated) -> None:
    from fno.inbox.store import (
        post_inbox_message,
        read_unread_threads,
        write_new_thread,
    )

    root = write_new_thread("acme-docs", "acme-web", "fyi", "root msg")
    res = post_inbox_message(
        recipient="acme-docs", sender="acme-web", kind="fyi",
        body="follow up", reply_to=root.thread_id,
    )

    assert res.appended is True
    threads = read_unread_threads("acme-docs")
    assert len(threads) == 1  # appended onto the same thread
    assert len(threads[0].messages) == 2


# ---------------------------------------------------------------------------
# CLI wiring: fno agents send --kind
# ---------------------------------------------------------------------------


def _invoke(runner: CliRunner, *args: str):
    from fno.mail.cli import mail_app

    return runner.invoke(mail_app, ["send", *args])


def test_cmd_send_kind_heads_up_writes_durable_thread(isolated, runner) -> None:
    from fno.inbox.store import read_unread_threads

    result = _invoke(
        runner,
        "--to-project", "acme-docs",
        "--kind", "heads-up",
        "--from-name", "acme-web",
        "locked: schema change; impact: migration",
    )

    assert result.exit_code == 0, result.output
    assert "queued (durable)" in result.output
    threads = read_unread_threads("acme-docs")
    assert len(threads) == 1
    assert threads[0].kind == "heads-up"
    assert threads[0].from_project == "acme-web"


def test_cmd_send_kind_question(isolated, runner) -> None:
    from fno.inbox.store import read_unread_threads

    result = _invoke(
        runner,
        "--to-project", "fno-peer",
        "--kind", "question",
        "--from-name", "acme-web",
        "design Q: which auth?",
    )
    assert result.exit_code == 0, result.output
    threads = read_unread_threads("fno-peer")
    assert threads[0].kind == "question"


def test_cmd_send_fyi_persist_memory(isolated, runner) -> None:
    from fno.inbox.store import read_unread_threads

    result = _invoke(
        runner,
        "--to-project", "acme-backend",
        "--kind", "fyi",
        "--persist", "memory",
        "--from-name", "acme-web",
        "lesson: always quote yaml scalars",
    )
    assert result.exit_code == 0, result.output
    threads = read_unread_threads("acme-backend")
    assert threads[0].persist_to_memory is True


def test_cmd_send_invalid_kind_rejected(isolated, runner) -> None:
    result = _invoke(
        runner, "--to-project", "x", "--kind", "bogus", "--from-name", "a", "body"
    )
    assert result.exit_code == 2
    assert "heads-up" in result.output  # names the valid kinds


def test_cmd_send_kind_send_rejected(isolated, runner) -> None:
    """`--kind send` is not an inbox kind - it must route the default path."""
    result = _invoke(
        runner, "--to-project", "x", "--kind", "send", "--from-name", "a", "body"
    )
    assert result.exit_code == 2


def test_cmd_send_persist_nonfyi_rejected(isolated, runner) -> None:
    result = _invoke(
        runner,
        "--to-project", "x",
        "--kind", "heads-up",
        "--persist", "memory",
        "--from-name", "a",
        "body",
    )
    assert result.exit_code == 2


def test_cmd_send_reply_to_appends(isolated, runner) -> None:
    from fno.inbox.store import read_unread_threads, write_new_thread

    root = write_new_thread("acme-docs", "acme-web", "fyi", "root")
    result = _invoke(
        runner,
        "--to-project", "acme-docs",
        "--kind", "fyi",
        "--reply-to", root.thread_id,
        "--from-name", "acme-web",
        "appended reply",
    )
    assert result.exit_code == 0, result.output
    assert "appended (durable)" in result.output
    threads = read_unread_threads("acme-docs")
    assert len(threads[0].messages) == 2


def test_cmd_send_body_flag(isolated, runner) -> None:
    """--body is the inbox-send-compatible alternative to the positional arg."""
    from fno.inbox.store import read_unread_threads

    result = _invoke(
        runner,
        "--to-project", "acme-docs",
        "--kind", "heads-up",
        "--from-name", "acme-web",
        "--body", "spec'd: feature X; touches surface api/",
    )
    assert result.exit_code == 0, result.output
    threads = read_unread_threads("acme-docs")
    assert len(threads) == 1
    assert threads[0].kind == "heads-up"


def test_cmd_send_ref_flags_ride_in_thread(isolated, runner) -> None:
    """--ref-pr/--ref-node enrich the thread for recipient triage (AC8-UI)."""
    from fno.inbox.store import read_unread_threads

    result = _invoke(
        runner,
        "--to-project", "acme-web",
        "--kind", "heads-up",
        "--from-name", "acme-backend",
        "--ref-pr", "112",
        "--ref-node", "ab-1f3c9a2b",
        "--body", "region column needed",
    )
    assert result.exit_code == 0, result.output
    refs = read_unread_threads("acme-web")[0].refs
    assert refs.get("ref_pr") == "112"
    assert refs.get("ref_node") == "ab-1f3c9a2b"


def test_cmd_send_explicit_from_name_abilities_is_honored(isolated, runner) -> None:
    """An explicit --from-name (even the literal 'fno') wins verbatim;
    only an UNSET --from-name resolves the sender from settings."""
    from fno.inbox.store import read_unread_threads

    result = _invoke(
        runner,
        "--to-project", "acme-docs",
        "--kind", "fyi",
        "--from-name", "fno",
        "explicit sender",
    )
    assert result.exit_code == 0, result.output
    assert read_unread_threads("acme-docs")[0].from_project == "fno"


def test_cmd_send_deprecated_kind_gives_migration_hint(isolated, runner) -> None:
    result = _invoke(
        runner, "--to-project", "x", "--kind", "lesson", "--from-name", "a", "body"
    )
    assert result.exit_code == 2
    out = result.stdout + (result.stderr or "")
    assert "fyi --persist memory" in out  # the retired-kind replacement hint


def test_cmd_send_no_kind_still_default_send(isolated, runner) -> None:
    """Without --kind, a bare project send keeps the durable agent-send path."""
    result = _invoke(
        runner, "--to-project", "nobody-home", "--from-name", "acme-web", "ping"
    )
    # No live peer -> durable queue for the project (existing US6 behavior).
    assert result.exit_code == 0, result.output
    assert "queued (durable)" in result.output
