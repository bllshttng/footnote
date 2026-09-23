from __future__ import annotations


def test_backlog_decisions_json_names_unread_graph_for_unknown_row(
    monkeypatch, tmp_path
):
    import json

    import fno.decide as decide
    from fno.graph.cli import cli as backlog_app
    from typer.testing import CliRunner

    row = {
        "decision_id": "d-unread",
        "subject": "x-1956",
        "decision": "bounded repair grant",
        "authority_source": "agent",
    }
    monkeypatch.setattr(decide, "_read_index", lambda *args, **kwargs: ([row], 0))

    def unreadable(*, required=False):
        if required:
            raise OSError("database is locked")
        return []

    monkeypatch.setattr(decide, "_graph_entries", unreadable)
    plans = tmp_path / "plans"
    plans.mkdir()
    monkeypatch.setattr("fno.paths.plans_content_dir", lambda project_root=None: plans)

    result = CliRunner().invoke(
        backlog_app, ["decisions", "x-1956", "--json"]
    )

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["decisions"][0]["lifecycle"] == "unknown"
    assert (
        "backlog decisions: the graph could not be read (database is locked), "
        "so 1 coord ruling(s) read UNKNOWN, not unscoped."
    ) in result.stderr


def test_backlog_decisions_does_not_name_an_unread_graph_for_live_rows(monkeypatch):
    import fno.decide as decide
    from fno.graph.cli import cli as backlog_app
    from typer.testing import CliRunner

    row = {
        "decision_id": "d-live",
        "subject": "x-1956",
        "decision": "readable ruling",
        "authority_source": "agent",
        "lane": "coord",
        "lifecycle": "live",
    }
    monkeypatch.setattr(
        decide,
        "list_decisions",
        lambda subject, limit=None, lane=None, state=None, entries=None: (
            subject or "(all)", [row], 0
        ),
    )
    monkeypatch.setattr(decide, "_graph_entries", lambda *, required=False: [])

    result = CliRunner().invoke(
        backlog_app, ["decisions", "x-1956", "--json"]
    )

    assert result.exit_code == 0, result.output
    assert "read UNKNOWN" not in result.stderr
