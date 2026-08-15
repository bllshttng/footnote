"""fno doctor: id collisions between the working graph and the archive (x-f69b).

A freed working-graph id can be reminted while the archive still holds a
different node under the same id -- a real collision, not a duplicate. The
check must count it and doctor must fail its exit code above zero.
"""
from __future__ import annotations

import json

from fno import doctor
from fno.paths_testing import use_tmpdir


def _seed(monkeypatch, tmp_path, *, working: list[dict], archive: list[dict] | None):
    use_tmpdir(monkeypatch, tmp_path)
    from fno.paths import graph_archive_json, graph_json

    graph_json().parent.mkdir(parents=True, exist_ok=True)
    graph_json().write_text(json.dumps({"entries": working}), encoding="utf-8")
    if archive is not None:
        graph_archive_json().parent.mkdir(parents=True, exist_ok=True)
        graph_archive_json().write_text(json.dumps({"entries": archive}), encoding="utf-8")


def test_no_archive_file_reports_zero(tmp_path, monkeypatch):
    _seed(monkeypatch, tmp_path, working=[{"id": "x-1"}], archive=None)
    result = doctor._archive_id_collisions()
    assert result == {"count": 0, "ids": []}


def test_disjoint_pools_report_zero(tmp_path, monkeypatch):
    _seed(
        monkeypatch, tmp_path,
        working=[{"id": "x-1"}],
        archive=[{"id": "x-2", "completed_at": "2026-01-01T00:00:00Z"}],
    )
    result = doctor._archive_id_collisions()
    assert result == {"count": 0, "ids": []}


def test_intersecting_id_is_counted_and_named(tmp_path, monkeypatch):
    _seed(
        monkeypatch, tmp_path,
        working=[{"id": "x-dup", "title": "live"}],
        archive=[{"id": "x-dup", "title": "archived", "completed_at": "2026-01-01T00:00:00Z"}],
    )
    result = doctor._archive_id_collisions()
    assert result == {"count": 1, "ids": ["x-dup"]}


def test_corrupt_archive_is_an_alarm_not_a_clean_zero(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno.paths import graph_archive_json, graph_json

    graph_json().parent.mkdir(parents=True, exist_ok=True)
    graph_json().write_text(json.dumps({"entries": [{"id": "x-1"}]}), encoding="utf-8")
    graph_archive_json().parent.mkdir(parents=True, exist_ok=True)
    graph_archive_json().write_text("{not json at all", encoding="utf-8")

    # Reading the corrupt archive must not report a measured zero: the ids
    # cannot be checked, and a green exit here resumes the collision bug with
    # every check passing.
    result = doctor._archive_id_collisions()
    assert result.get("unreadable") is True
