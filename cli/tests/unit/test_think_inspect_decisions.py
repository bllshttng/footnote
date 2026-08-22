from __future__ import annotations

from pathlib import Path


def _receipt(monkeypatch, tmp_path: Path, seed: str, fake_list_decisions):
    from fno.think_inspect import build_receipt

    monkeypatch.setattr("fno.decide.list_decisions", fake_list_decisions)
    repo = tmp_path / "repo"
    repo.mkdir()
    graph = [
        {
            "id": "x-38d3",
            "slug": "spike-keep-fno-mail",
            "title": "SPIKE",
            "status": "done",
            "domain": "code",
        },
        {
            "id": "x-953b",
            "slug": "no-decisions",
            "title": "No decisions",
            "status": "ready",
            "domain": "code",
        },
    ]

    def run(argv, cwd=None, timeout=10):
        import subprocess

        return subprocess.CompletedProcess(argv, 1, "", "unavailable")

    return build_receipt(
        seed,
        repo=repo,
        graph_entries=graph,
        archive_entries=[],
        plans_path=tmp_path / "plans",
        home=tmp_path,
        run=run,
    )


def test_receipt_carries_one_live_decision(monkeypatch, tmp_path: Path) -> None:
    def fake(subject, limit=None, lane=None):
        assert subject == "x-38d3"
        row = {
            "decision_id": "d-a4b6e1c8",
            "ts": "2026-08-21T18:07:21Z",
            "lane": "coord",
            "subject": "x-38d3",
            "decision": "VERDICT: KEEP.",
            "supersedes": None,
            "superseded_by": None,
        }
        return subject, [row], 0

    receipt = _receipt(monkeypatch, tmp_path, "x-38d3", fake)

    decisions = receipt["graph"]["decisions"]
    assert [row["decision_id"] for row in decisions] == ["d-a4b6e1c8"]
    assert receipt["graph"]["decisions_status"] == "ok"
    assert receipt["graph"]["decisions_truncated"] is False


def test_superseded_ruling_is_dropped_by_the_derived_field(monkeypatch, tmp_path: Path) -> None:
    def fake(subject, limit=None, lane=None):
        withdrawn = {
            "decision_id": "d-85352ee7",
            "ts": "2026-08-22T12:12:00Z",
            "lane": "coord",
            "subject": "x-38d3",
            # Prose claims supersession, but the derived field is what the reader
            # trusts. list_decisions computes superseded_by from `supersedes`
            # ACROSS the index, so a withdrawn row carries it here even though
            # its own text never says so.
            "decision": "HYBRID",
            "supersedes": None,
            "superseded_by": "d-62cd9d80",
        }
        winner = {
            "decision_id": "d-62cd9d80",
            "ts": "2026-08-22T12:27:04Z",
            "lane": "coord",
            "subject": "x-38d3",
            "decision": "CORRECTION, SUPERSEDES d-85352ee7. WITHDRAWN.",
            "supersedes": "d-85352ee7",
            "superseded_by": None,
        }
        return subject, [winner, withdrawn], 0

    receipt = _receipt(monkeypatch, tmp_path, "x-38d3", fake)

    ids = [row["decision_id"] for row in receipt["graph"]["decisions"]]
    assert "d-62cd9d80" in ids
    assert "d-85352ee7" not in ids


def test_unreadable_decision_index_reports_error_not_empty(monkeypatch, tmp_path: Path) -> None:
    def fake(subject, limit=None, lane=None):
        raise OSError("index corrupt")

    receipt = _receipt(monkeypatch, tmp_path, "x-38d3", fake)

    assert receipt["graph"]["decisions"] == []
    assert receipt["graph"]["decisions_status"] == "error"
    assert receipt["graph"]["decisions_detail"] and "index corrupt" in receipt["graph"]["decisions_detail"]


def test_node_with_no_rulings_reads_clean(monkeypatch, tmp_path: Path) -> None:
    def fake(subject, limit=None, lane=None):
        return subject, [], 0

    receipt = _receipt(monkeypatch, tmp_path, "x-953b", fake)

    assert receipt["graph"]["decisions"] == []
    assert receipt["graph"]["decisions_status"] == "ok"
    assert receipt["graph"]["decisions_detail"] is None


def test_decisions_list_is_capped_and_says_so(monkeypatch, tmp_path: Path) -> None:
    from fno import think_inspect

    def fake(subject, limit=None, lane=None):
        rows = [
            {
                "decision_id": f"d-{i:08x}",
                "ts": f"2026-08-{i + 1:02d}T00:00:00Z",
                "lane": "coord",
                "subject": subject,
                "decision": "noise",
                "supersedes": None,
                "superseded_by": None,
            }
            for i in range(think_inspect._DECISIONS_CAP + 3)
        ]
        return subject, rows, 0

    receipt = _receipt(monkeypatch, tmp_path, "x-38d3", fake)

    assert len(receipt["graph"]["decisions"]) == think_inspect._DECISIONS_CAP
    assert receipt["graph"]["decisions_truncated"] is True


def test_no_decisions_result_is_not_a_shared_mutable_list(monkeypatch, tmp_path: Path) -> None:
    """x-953b review finding: a module-level constant's "decisions": [] was
    the SAME list object handed to every "no node" / "no decisions" receipt.
    Mutating one must never poison another (or the module constant itself)."""
    from fno.think_inspect import build_receipt

    repo = tmp_path / "repo"
    repo.mkdir()

    def run(argv, cwd=None, timeout=10):
        import subprocess

        return subprocess.CompletedProcess(argv, 1, "", "unavailable")

    def fake(subject, limit=None, lane=None):
        return subject, [], 0

    monkeypatch.setattr("fno.decide.list_decisions", fake)

    first = build_receipt(
        "no-such-node", repo=repo, graph_entries=[], archive_entries=[],
        plans_path=tmp_path / "plans", home=tmp_path, run=run,
    )
    first["graph"]["decisions"].append({"poison": True})

    second = build_receipt(
        "no-such-node", repo=repo, graph_entries=[], archive_entries=[],
        plans_path=tmp_path / "plans", home=tmp_path, run=run,
    )

    assert second["graph"]["decisions"] == []
