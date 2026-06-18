"""Tests for the unread_scan helper used by wake hooks and stop hook."""
from __future__ import annotations

import io
import json
from contextlib import redirect_stdout

import pytest


@pytest.fixture
def project_with_inbox(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    proj_dir = tmp_path / "alice"
    settings_dir = proj_dir / ".fno"
    settings_dir.mkdir(parents=True)
    (settings_dir / "settings.yaml").write_text("project: alice\n", encoding="utf-8")
    monkeypatch.chdir(proj_dir)
    return proj_dir


def test_count_zero_when_no_unread(project_with_inbox):
    from fno.inbox.unread_scan import main

    buf = io.StringIO()
    with redirect_stdout(buf):
        rc = main(["count"])
    assert rc == 0
    assert buf.getvalue().strip() == "0"


def test_count_matches_unread_threads(project_with_inbox):
    from fno.inbox.store import write_new_thread, mark_thread_read, Kind
    from fno.inbox.unread_scan import main

    write_new_thread("alice", "bob", Kind.FYI.value, "first")
    write_new_thread("alice", "bob", Kind.HEADS_UP.value, "second")
    h = write_new_thread("alice", "bob", Kind.QUESTION.value, "third")
    mark_thread_read(h.path)

    buf = io.StringIO()
    with redirect_stdout(buf):
        main(["count"])
    assert buf.getvalue().strip() == "2"


def test_list_json_includes_kind_and_summary(project_with_inbox):
    from fno.inbox.store import write_new_thread, Kind
    from fno.inbox.unread_scan import main

    write_new_thread(
        "alice", "bob", Kind.QUESTION.value,
        "should we roll the rotation queue back?",
    )

    buf = io.StringIO()
    with redirect_stdout(buf):
        main(["list-json"])
    payload = json.loads(buf.getvalue().strip())
    assert len(payload) == 1
    assert payload[0]["kind"] == "question"
    assert payload[0]["from"] == "bob"
    assert "rotation" in payload[0]["summary"]


def test_should_block_default_false(project_with_inbox):
    from fno.inbox.unread_scan import main

    buf = io.StringIO()
    with redirect_stdout(buf):
        main(["should-block"])
    assert buf.getvalue().strip() == "false"


def test_should_block_true_when_settings_say_so(project_with_inbox):
    proj_dir = project_with_inbox
    (proj_dir / ".fno" / "settings.yaml").write_text(
        "project: alice\nconfig:\n  inbox:\n    block_complete_on_unread: true\n",
        encoding="utf-8",
    )
    from fno.inbox.unread_scan import main

    buf = io.StringIO()
    with redirect_stdout(buf):
        main(["should-block"])
    assert buf.getvalue().strip() == "true"
