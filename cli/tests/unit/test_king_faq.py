"""``fno agents king faq add`` - write, refuse-with-no-exit, and scope defaults.

Invoked through ``agents_king_app`` rather than the bare ``faq_app``: a Typer
app with exactly one command collapses to that command directly (no
subcommand word needed), so testing ``faq_app`` alone would silently swallow
the literal "add" the real ``fno agents king faq add`` invocation sends.
``agents_king_app`` carries sibling commands (init, done, cancel, ...) and
never collapses, matching the real CLI surface.
"""
from __future__ import annotations

import os
import time
from pathlib import Path

from typer.testing import CliRunner

from fno.king.cli import agents_king_app
from fno.king.king_faq import add_cmd, entries_for_scope, faq_app, write_faq_entry


def test_write_faq_entry_creates_file_with_expected_frontmatter(tmp_path):
    path = write_faq_entry(
        question="A node is blocked and I cannot unblock it",
        answer="Route it through the blocked_child board queue.",
        specimen="x-eb79, 2026-09-08",
        exit_="the blocked_child board queue, x-3ecf",
        scope="x-6cac",
        king="king-fixture",
        session="session-abc123",
        faqs_dir=tmp_path,
    )
    assert path.parent == tmp_path
    text = path.read_text(encoding="utf-8")
    assert "created: " in text
    assert "king: king-fixture" in text
    assert "session: session-abc123" in text
    assert "scope: x-6cac" in text
    assert "# A node is blocked and I cannot unblock it" in text
    assert "## Answer" in text
    assert "## Specimen" in text
    assert "## Exit" in text
    assert "the blocked_child board queue, x-3ecf" in text


def test_add_cmd_refuses_with_no_exit(monkeypatch, tmp_path):
    monkeypatch.setattr("fno.paths.king_faqs_dir", lambda: tmp_path)
    runner = CliRunner()
    result = runner.invoke(
        agents_king_app,
        ["faq", "add", "--question", "Q?", "--answer", "A", "--specimen", "S", "--scope", "x-6cac"],
    )
    assert result.exit_code == 2
    assert "no --exit" in result.output
    assert "workaround with no plan to stop needing it" in result.output
    assert list(tmp_path.iterdir()) == []


def test_add_cmd_refuses_with_no_scope_and_no_crown(monkeypatch, tmp_path):
    import fno.agents.crown as crown

    monkeypatch.setattr(crown, "current_crown", lambda: None)
    monkeypatch.setattr("fno.paths.king_faqs_dir", lambda: tmp_path)
    runner = CliRunner()
    result = runner.invoke(
        agents_king_app,
        ["faq", "add", "--question", "Q?", "--answer", "A", "--specimen", "S", "--exit", "E"],
    )
    assert result.exit_code == 2
    assert "no crown held and no --scope given" in result.output
    assert list(tmp_path.iterdir()) == []


def test_add_cmd_defaults_scope_from_current_crown(monkeypatch, tmp_path):
    import fno.agents.crown as crown
    import fno.agents.self_stamp as self_stamp

    monkeypatch.setattr(
        crown, "current_crown", lambda: {"level": 2, "scope": "x-6cac", "grantor": "human"}
    )
    monkeypatch.setattr(self_stamp, "resolve_self_handle", lambda: "king-fixture")
    monkeypatch.setattr(self_stamp, "resolve_self_session_id", lambda: "session-abc123")
    monkeypatch.setattr("fno.paths.king_faqs_dir", lambda: tmp_path)

    runner = CliRunner()
    result = runner.invoke(
        agents_king_app,
        ["faq", "add", "--question", "Q?", "--answer", "A", "--specimen", "S", "--exit", "E"],
    )
    assert result.exit_code == 0, result.output
    written = Path(result.output.strip())
    assert written.exists()
    text = written.read_text(encoding="utf-8")
    assert "scope: x-6cac" in text
    assert "king: king-fixture" in text
    assert "session: session-abc123" in text


def test_entries_for_scope_orders_by_mtime_not_filename(tmp_path):
    # The filename's variable part is a random hex token, not a timestamp, so
    # an alphabetical sort would not reflect write order. Name the older entry
    # so it alphabetically SORTS LAST, proving the ordering comes from mtime.
    older = write_faq_entry(
        question="older", answer="a", specimen="s", exit_="e",
        scope="x-6cac", king="k", session="sess", faqs_dir=tmp_path,
    )
    older = older.rename(tmp_path / "king-x-6cac-zzzzzzzz.md")
    time.sleep(0.01)
    newer = write_faq_entry(
        question="newer", answer="a", specimen="s", exit_="e",
        scope="x-6cac", king="k", session="sess", faqs_dir=tmp_path,
    )
    newer = newer.rename(tmp_path / "king-x-6cac-00000000.md")
    now = time.time()
    os.utime(older, (now - 100, now - 100))
    os.utime(newer, (now, now))

    entries = entries_for_scope("x-6cac", faqs_dir=tmp_path)
    assert len(entries) == 2
    assert "# older" in entries[0]
    assert "# newer" in entries[1]


def test_faq_app_is_reachable_from_agents_king_app():
    # Not registered on bare king_app: the plan's usage is `fno agents king
    # faq add` only, and king_app's advertised surface is menu-capped.
    assert any(g.typer_instance is faq_app for g in agents_king_app.registered_groups)
    assert add_cmd is not None
