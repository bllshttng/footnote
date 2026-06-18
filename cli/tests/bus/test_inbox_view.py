"""Group 1 (ab-ba91b807) - `fno inbox view`: read-only projection over the bus.

AC2-UI: the jsonl bus is the source of truth; `view` renders it (md/text/json),
surfacing the enriched address fields when present and ignoring unknown fields
(LD11 additive read). Default scope is the reader's project so a cross-project
body is not leaked; --all is the explicit operator view.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def env(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "agents"))
    return tmp_path


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _bus(to, body, **kw):
    from fno.bus.log import Envelope, append
    append(Envelope.new(from_=kw.pop("from_", "x"), to=to, kind="send", body=body, **kw))


def test_view_json_all_renders_enriched_fields(env, runner):
    from fno.mail.cli import mail_app

    _bus("worker-b", "hi b", from_="alice", to_kind="name",
         provider_from="claude", from_model="opus-4-8")

    res = runner.invoke(mail_app, ["view", "--json", "--all"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.stdout)
    assert len(data) == 1
    m = data[0]
    assert m["to"] == "worker-b"
    assert m["to_kind"] == "name"
    assert m["from_model"] == "opus-4-8"
    assert m["from"] == "alice"
    assert m["body"] == "hi b"


def test_view_default_scopes_to_project(env, runner):
    from fno.mail.cli import mail_app

    _bus("projA", "for A", from_="someone", to_kind="project")
    _bus("projB", "for B (other project)", from_="elsewhere", to_kind="project")

    res = runner.invoke(mail_app, ["view", "--json", "--from", "projA"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.stdout)
    tos = {m["to"] for m in data}
    assert "projA" in tos
    assert "projB" not in tos  # cross-project body not leaked by default


def test_view_all_shows_cross_project(env, runner):
    from fno.mail.cli import mail_app

    _bus("projA", "for A", from_="someone", to_kind="project")
    _bus("projB", "for B", from_="elsewhere", to_kind="project")

    res = runner.invoke(mail_app, ["view", "--json", "--all"])
    assert res.exit_code == 0, res.output
    tos = {m["to"] for m in json.loads(res.stdout)}
    assert {"projA", "projB"} <= tos


def test_view_additive_ignores_unknown_fields(env, runner):
    from fno.bus.log import bus_log_path
    from fno.mail.cli import mail_app

    p = bus_log_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(
        json.dumps({
            "v": 1, "id": "msg-x", "ts": "2026-01-01T00:00:00Z", "thread": "msg-x",
            "from": "a", "to": "projA", "kind": "send", "to_kind": "project",
            "future_field": "ignored-by-old-readers", "body": "hi",
        }) + "\n",
        encoding="utf-8",
    )
    res = runner.invoke(mail_app, ["view", "--json", "--from", "projA"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.stdout)
    assert data[0]["body"] == "hi"
    assert "future_field" not in data[0]  # unknown fields are not surfaced


def test_view_text_output_is_human_readable(env, runner):
    from fno.mail.cli import mail_app

    _bus("projA", "the body text", from_="alice", to_kind="project")
    res = runner.invoke(mail_app, ["view", "--from", "projA"])
    assert res.exit_code == 0, res.output
    assert "alice" in res.stdout
    assert "the body text" in res.stdout
