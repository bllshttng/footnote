from __future__ import annotations


def test_failed_coord_graph_read_returns_unknown_not_unscoped(monkeypatch):
    import fno.decide as decide

    row = {
        "decision_id": "d-unread",
        "subject": "x-1956",
        "decision": "bounded repair grant",
        "authority_source": "agent",
    }
    monkeypatch.setattr(decide, "_read_index", lambda: ([row], 0))

    def unreadable(*, required=False):
        assert required
        raise OSError("database is locked")

    monkeypatch.setattr(decide, "_graph_entries", unreadable)

    try:
        _, rows, _ = decide.list_decisions(state="all")
    except OSError:
        rows = []

    assert len(rows) == 1
    assert rows[0]["lifecycle"] == "unknown"
    assert rows[0]["lifecycle_reason"] == "the graph could not be read (database is locked)"


def test_unknown_survives_live_filter_and_subjectless_coord_stays_unscoped(monkeypatch):
    import fno.decide as decide

    rows = [
        {
            "decision_id": "d-coord",
            "subject": "x-1956",
            "decision": "coord ruling",
            "authority_source": "agent",
        },
        {
            "decision_id": "d-law",
            "subject": "x-1956",
            "decision": "law ruling",
            "authority_source": "operator",
            "ts": "2026-08-25T00:00:00Z",
        },
        {
            "decision_id": "d-subjectless",
            "decision": "subjectless coord ruling",
            "authority_source": "agent",
        },
    ]
    monkeypatch.setattr(decide, "_read_index", lambda: (rows, 0))

    def unreadable(*, required=False):
        assert required
        raise OSError("database is locked")

    monkeypatch.setattr(decide, "_graph_entries", unreadable)

    try:
        _, live, _ = decide.list_decisions(state="live")
    except OSError:
        live = []
    assert {row["decision_id"]: row["lifecycle"] for row in live} == {
        "d-coord": "unknown",
        "d-law": "live",
    }

    try:
        _, history, _ = decide.list_decisions(state="all")
    except OSError:
        history = []
    subjectless = next(
        (row for row in history if row["decision_id"] == "d-subjectless"), None
    )
    assert subjectless is not None
    assert subjectless["lifecycle"] == "unscoped"


def test_review_list_uses_soft_graph_read_that_reports_failure(monkeypatch, capsys):
    import sys

    import fno.decide as decide

    monkeypatch.setattr(decide, "_read_index", lambda: ([], 0))

    def graph_entries(*, required=False):
        if required:
            raise OSError("database is locked")
        print("decide: the graph could not be read (database is locked)", file=sys.stderr)
        return []

    monkeypatch.setattr(decide, "_graph_entries", graph_entries)

    decide.review_list()

    assert "decide: the graph could not be read" in capsys.readouterr().err
