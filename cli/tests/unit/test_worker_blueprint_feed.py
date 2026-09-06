"""x-e221 tasks 3.1/3.2: the territory blueprinter feed (fno worker blueprint-feed)."""
import json
from datetime import datetime, timedelta, timezone
from urllib.parse import quote

import pytest

import fno.worker.blueprint as bp
from fno.graph.ladder import Rung
from fno.worker.blueprint import (
    REDO_AFTER_SECS,
    RETRY_AFTER_SECS,
    blueprint_feed,
    record_path,
    worker_name_for_scope,
)

TERRITORY = {"scope": "fno", "rung": 1, "kingless": True, "members": ["fno"]}
NOW = datetime(2026, 9, 6, 12, 0, 0, tzinfo=timezone.utc)


def fake_plan_rung(row):
    return {"idea": Rung.IDEA, "design": Rung.DESIGN, "ready": Rung.READY}.get(
        row.get("_rung"), Rung.NONE
    )


def iso(seconds_offset):
    dt = NOW + timedelta(seconds=seconds_offset)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


@pytest.fixture()
def world(monkeypatch, tmp_path):
    """Isolated state dir + patched territory, membership, graph, liveness.

    Territory membership itself is covered by test_king_scope.py (task 1.1);
    these tests isolate the feed's own logic: selection, fed ledger,
    delivery, repair.
    """
    (tmp_path / "state").mkdir()
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path / "state")
    monkeypatch.setattr(bp, "_territory", lambda scope: dict(TERRITORY))
    monkeypatch.setattr(
        "fno.king.scope.territory_membership",
        lambda scope, entries, **k: type(
            "TM",
            (),
            {"state": "ok", "key": scope, "ids": frozenset({"x-a", "x-b", "x-c", "x-d", "x-e"})},
        )(),
    )
    monkeypatch.setattr("fno.graph.ladder.plan_rung", fake_plan_rung)
    monkeypatch.setattr(bp, "_live_registry_names", lambda: set())
    return tmp_path


def _write_record(tmp_path, worker=None, fed=None, repairs=None):
    rp = record_path("fno")
    rp.parent.mkdir(parents=True, exist_ok=True)
    rp.write_text(
        json.dumps({"worker": worker, "fed": fed or {}, "repairs": repairs or []})
    )
    return rp


def _rows_simple():
    return [
        {"id": "x-a", "status": "idea", "_rung": "idea"},
        {"id": "x-b", "status": "idea", "_rung": "design"},
    ]

@pytest.mark.usefixtures("world")
class TestBlueprintFeed:
    def test_status_lists_unfed_triaged_ideas(self, monkeypatch):
        rows = [
            {"id": "x-a", "status": "idea", "_rung": "idea"},
            {"id": "x-b", "status": "idea", "_rung": "design"},
            {"id": "x-c", "status": "idea", "_rung": "design", "completed_at": "t"},
            {"id": "x-d", "status": "ready", "_rung": "ready"},
            {"id": "x-e", "status": "idea", "_rung": "none"},
            {"id": "x-f", "status": "queued", "_rung": "design"},
        ]
        monkeypatch.setattr(bp, "_read_entries", lambda: rows)

        out = blueprint_feed("fno", now=NOW)

        assert out["action"] == "status"
        assert [i["id"] for i in out["ideas"]] == ["x-a", "x-b"]
        assert out["kingless"] is True and out["rung"] == 1

    def test_status_excludes_out_of_territory_nodes(self, monkeypatch):
        monkeypatch.setattr(
            bp,
            "_read_entries",
            lambda: [{"id": "x-z", "status": "idea", "_rung": "idea"}],
        )
        out = blueprint_feed("fno", now=NOW)
        assert out["ideas"] == []

    def test_deliver_mails_each_idea_and_marks_fed(self, monkeypatch, tmp_path):
        monkeypatch.setattr(bp, "_read_entries", lambda: _rows_simple())
        sent = []
        monkeypatch.setattr(
            bp, "_mail_deliver", lambda name, nid: sent.append((name, nid)) or (True, "")
        )
        monkeypatch.setattr(bp, "_live_registry_names", lambda: {"w"})
        rp = _write_record(tmp_path, worker={"name": "w", "spawned_at": "t"})

        out = blueprint_feed("fno", deliver=True, now=NOW)

        assert out["action"] == "deliver"
        assert out["delivered"] == ["x-a", "x-b"]
        assert sent == [("w", "x-a"), ("w", "x-b")]
        stored = json.loads(rp.read_text())
        assert stored["fed"]["x-a"]["ok"] is True

    def test_deliver_failure_preserves_the_idea(self, monkeypatch, tmp_path):
        monkeypatch.setattr(
            bp,
            "_read_entries",
            lambda: [{"id": "x-a", "status": "idea", "_rung": "idea"}],
        )
        monkeypatch.setattr(bp, "_mail_deliver", lambda name, nid: (False, "mailbox full"))
        monkeypatch.setattr(bp, "_live_registry_names", lambda: {"w"})
        rp = _write_record(tmp_path, worker={"name": "w", "spawned_at": "t"})

        out = blueprint_feed("fno", deliver=True, now=NOW)

        assert out["action"] == "deliver"
        assert out["delivered"] == []
        assert out["failed"][0]["id"] == "x-a"
        stored = json.loads(rp.read_text())
        assert stored["fed"]["x-a"]["ok"] is False
        later = NOW + timedelta(seconds=RETRY_AFTER_SECS + 1)
        out2 = blueprint_feed("fno", now=later)
        assert [i["id"] for i in out2["ideas"]] == ["x-a"]

    def test_worker_not_live_blocks_and_records_repair(self, monkeypatch, tmp_path):
        monkeypatch.setattr(bp, "_read_entries", lambda: _rows_simple())
        rp = _write_record(tmp_path, worker={"name": "w", "spawned_at": "t"})

        out = blueprint_feed("fno", deliver=True, now=NOW)

        assert out["action"] == "blocked"
        assert out["reason"] == "worker_not_live"
        stored = json.loads(rp.read_text())
        assert stored["repairs"][-1]["reason"].startswith("worker_not_live")

    def test_unknown_territory_refuses(self, monkeypatch):
        monkeypatch.setattr(bp, "_territory", lambda scope: None)
        out = blueprint_feed("nowhere", now=NOW)
        assert out["action"] == "unknown"
        assert "no such territory" in out["reason"]

    def test_membership_unknown_refuses(self, monkeypatch):
        monkeypatch.setattr(
            "fno.king.scope.territory_membership",
            lambda scope, entries, **k: type(
                "TM", (), {"state": "unknown", "reason": "bad scope"}
            )(),
        )
        out = blueprint_feed("fno", now=NOW)
        assert out["action"] == "unknown"
        assert out["reason"] == "bad scope"

    def test_repair_preserves_ideas(self, monkeypatch, tmp_path):
        monkeypatch.setattr(
            bp,
            "_read_entries",
            lambda: [{"id": "x-a", "status": "idea", "_rung": "idea"}],
        )
        rp = _write_record(tmp_path, worker=None)

        out = blueprint_feed("fno", repair="spawn refused: cap", now=NOW)

        assert out["action"] == "repair"
        assert out["ideas"] == 1

        out2 = blueprint_feed("fno", now=NOW + timedelta(hours=25))
        assert [i["id"] for i in out2["ideas"]] == ["x-a"]
        stored = json.loads(rp.read_text())
        assert stored["repairs"][0]["reason"] == "spawn refused: cap"

    def test_prune_drops_closed_and_vanished_nodes(self, monkeypatch, tmp_path):
        monkeypatch.setattr(
            bp,
            "_read_entries",
            lambda: [
                {"id": "x-a", "status": "idea", "_rung": "idea"},
                {"id": "x-done", "status": "done"},
            ],
        )
        rp = _write_record(
            tmp_path,
            fed={
                "x-done": {"at": iso(-48 * 3600), "ok": True},
                "x-gone": {"at": iso(-48 * 3600), "ok": True},
                "x-a": {"at": iso(-3600), "ok": True},
            },
        )

        out = blueprint_feed("fno", now=NOW)

        stored = json.loads(rp.read_text())
        assert "x-done" not in stored["fed"] and "x-gone" not in stored["fed"]
        assert "x-a" in stored["fed"]
        assert out["ideas"] == []

    def test_redo_window_redelivers_stale_delivered_ideas(self, monkeypatch, tmp_path):
        monkeypatch.setattr(
            bp,
            "_read_entries",
            lambda: [{"id": "x-a", "status": "idea", "_rung": "idea"}],
        )
        _write_record(
            tmp_path,
            worker={"name": "w", "spawned_at": "t"},
            fed={"x-a": {"at": iso(-(REDO_AFTER_SECS + 60)), "ok": True}},
        )

        out = blueprint_feed("fno", now=NOW)

        assert [i["id"] for i in out["ideas"]] == ["x-a"]


class TestMailTransport:
    def test_mail_uses_the_real_transport_argv(self, monkeypatch, tmp_path):
        recorded = tmp_path / "mail-argv.log"
        stub = tmp_path / "fno-py"
        stub.write_text(f'#!/bin/sh\necho "$@" >> {recorded}\nexit 0\n')
        stub.chmod(0o755)
        monkeypatch.setattr("fno._subprocess_util.fno_py_cmd", lambda: [str(stub)])

        ok, detail = bp._mail_deliver("w", "x-9")

        assert ok is True and detail == ""
        assert "mail send w /fno:blueprint x-9" in recorded.read_text()

    def test_mail_failure_surfaces_the_reason(self, monkeypatch, tmp_path):
        stub = tmp_path / "fno-py"
        stub.write_text('#!/bin/sh\necho "refused" >&2\nexit 2\n')
        stub.chmod(0o755)
        monkeypatch.setattr("fno._subprocess_util.fno_py_cmd", lambda: [str(stub)])

        ok, detail = bp._mail_deliver("w", "x-9")

        assert ok is False
        assert "refused" in detail


class TestNaming:
    def test_worker_name_is_stable_and_registry_safe(self):
        assert worker_name_for_scope("fno") == worker_name_for_scope("fno")
        name = worker_name_for_scope("loose:fno")
        assert name.startswith("blueprinter-")
        assert "/" not in name and ":" not in name
        assert name != worker_name_for_scope("fno")

    def test_record_path_encodes_the_scope(self):
        assert record_path("loose:fno").name == quote("loose:fno", safe="") + ".json"
