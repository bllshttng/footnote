"""Integration: the flow projection refresh end to end (x-b07a).

Builds an isolated graph+ledger fixture (two projects, a retried node, a PR
pending review, an unlinked PR row, unknown origin, missing spend), drives
the real post-mutation projection refresh, and asserts the emitted boards
carry the matching project/window flow payload while the public output stays
free of private provenance and local paths. AC3-HP. Timestamps are relative
to the run's wall clock, so the test never ages out.
"""

from __future__ import annotations

import json
import re
from datetime import datetime, timedelta
from pathlib import Path

import pytest

import fno.graph.roadmap_public as rp
from fno.config import RenderTargetConfig
from fno.graph.render_html import LEAK_PATTERNS
from fno.graph.roadmap_public import render_configured_targets

NOW = datetime.now().replace(microsecond=0)


def _iso(days_ago: float) -> str:
    return (NOW - timedelta(days=days_ago)).isoformat()


def _node(eid: str, project: str, title: str, **kw) -> dict:
    base = {
        "id": eid,
        "title": title,
        "type": "feature",
        "priority": "p2",
        "status": "ready",
        "project": project,
        "created_at": _iso(40),
        "touched_at": _iso(0),
        "blocked_by": [],
        "plan_path": None,
        "pr_number": None,
        "pr_url": None,
    }
    base.update(kw)
    return base


def _entries() -> list[dict]:
    return [
        _node("ab-m1a2b3c4", "alpha", "Steady import sweep", status="done",
              merge_status="merged", merged_at=_iso(10), pr_number=401, created_at=_iso(40)),
        _node("ab-m2d4e5f6", "alpha", "Queue drain backoff", status="done",
              merge_status="merged", merged_at=_iso(3), pr_number=402, created_at=_iso(9)),
        # Retried: a wedge row, then the ship row, same node.
        _node("ab-m3g5h6i7", "alpha", "Export dedupe fix", status="done",
              merge_status="merged", merged_at=_iso(8), pr_number=403, created_at=_iso(30)),
        # Unknown request origin, no PR: the canonical WIP population.
        _node("ab-w4j7k8l9", "alpha", "Chart tooltips pass", status="in_progress",
              request_origin="unknown"),
        _node("ab-r5m8n9o0", "alpha", "Reader pagination", status="in_review",
              pr_number=404, created_at=_iso(20)),
        _node("ab-b6p9q0r1", "alpha", "Sitemap stale rows", status="blocked"),
        _node("ab-d7q0r1s2", "alpha", "Metrics definitions brief", status="done",
              completed_at=_iso(2)),
        _node("ab-b8r1s2t3", "beta", "Beta side rollout", status="done",
              merge_status="merged", merged_at=_iso(10), pr_number=409, created_at=_iso(12)),
        # Real history, not this window's throughput: merged 45 days ago.
        _node("ab-m9oldaa4", "alpha", "Ancient import path", status="done",
              merge_status="merged", merged_at=_iso(45), pr_number=400, created_at=_iso(70)),
    ]


def _rows() -> list[dict]:
    return [
        {"type": "execution", "graph_node_id": "ab-m1a2b3c4", "termination_reason": "DonePRGreen",
         "completed": _iso(10), "cost_usd": 1.5, "project": "alpha"},
        # Missing spend: a real recorded outcome with no cost number.
        {"type": "execution", "graph_node_id": "ab-m2d4e5f6", "termination_reason": "DonePRGreen",
         "completed": _iso(3), "project": "alpha"},
        {"type": "execution", "graph_node_id": "ab-m3g5h6i7", "termination_reason": "Budget",
         "completed": _iso(15), "cost_usd": 0.8, "project": "alpha"},
        {"type": "execution", "graph_node_id": "ab-m3g5h6i7", "termination_reason": "DonePRGreen",
         "completed": _iso(8), "cost_usd": 2.0, "project": "alpha"},
        {"type": "execution", "graph_node_id": "ab-d7q0r1s2", "termination_reason": "DoneAdvisory",
         "completed": _iso(2), "project": "alpha"},
        # Unlinked PR: delivered, carries a PR, and no graph node anywhere.
        {"type": "execution", "termination_reason": "DonePRGreen", "pr_number": 405,
         "completed": _iso(5)},
        # The 45-day-old merge's row: outside the window with it.
        {"type": "execution", "graph_node_id": "ab-m9oldaa4", "termination_reason": "DonePRGreen",
         "completed": _iso(45), "cost_usd": 3.0, "project": "alpha"},
    ]


@pytest.fixture
def hermetic(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": _rows()}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.ledger_json", lambda: ledger)
    targets = [
        RenderTargetConfig(path=str(tmp_path / "local.html"), scope=rp.ALL_PROJECTS, projection="local"),
        RenderTargetConfig(path=str(tmp_path / "alpha-backlog.html"), scope="alpha", projection="backlog"),
        RenderTargetConfig(path=str(tmp_path / "alpha-roadmap.html"), scope="alpha", projection="roadmap"),
    ]
    monkeypatch.setattr(rp, "_configured_targets", lambda: targets)
    return tmp_path


def _payload_flow(text: str) -> dict:
    match = re.search(
        r'<script id="data" type="application/json">(.*?)</script>', text, re.S
    )
    assert match, "dashboard payload script tag missing"
    return json.loads(match.group(1))["flow"]


def _payload_json(text: str) -> str:
    match = re.search(
        r'<script id="data" type="application/json">(.*?)</script>', text, re.S
    )
    assert match, "dashboard payload script tag missing"
    return match.group(1)


def test_ac3_hp_refresh_emits_matching_flow_and_public_stays_private(hermetic: Path):
    render_configured_targets(_entries(), skip_canonical=True)

    local = (hermetic / "local.html").read_text()
    flow = _payload_flow(local)
    assert flow["available"] is True
    # Window and grouping are explicit on the payload itself.
    assert flow["window"]["since_days"] == 28
    assert flow["window"]["week_start"] == "monday"
    assert re.match(r"^[+-]\d\d:\d\d$", flow["window"]["tz_offset"])
    # A 28-day window touches 4 or 5 local Monday-weeks; weekly sums match
    # the window totals, and the retried node delivers once, not twice.
    weeks = flow["deliveries"]["weeks"]
    assert 4 <= len(weeks) <= 5
    assert weeks[0]["partial"] is True and weeks[-1]["partial"] is True
    assert sum(w["code"] for w in weeks) == flow["deliveries"]["code"] == 4
    assert sum(w["doc"] for w in weeks) == flow["deliveries"]["doc"] == 1
    # PR open-to-merge on the all-board: four merges with both timestamps
    # (alpha 30/6/22 days plus beta's 2), median 14, nearest-rank p85 30.
    assert flow["cycle"] == {"median_days": 14.0, "p85_days": 30.0, "n": 4}
    assert flow["open_prs"]["count"] == 1
    assert flow["waiting"]["in_progress"]["count"] == 1
    assert flow["waiting"]["in_review"]["count"] == 1
    assert flow["waiting"]["blocked"]["count"] == 1
    # The unlinked PR rides repository coverage, never the weekly series.
    assert flow["coverage"]["unlinked"] == 1
    assert flow["coverage"]["rows"] == 7
    assert flow["coverage"]["age_basis"] == "node created_at"

    # Project scoping: beta's merge is out of the alpha denominator, and the
    # unlinked PR (no project) stays a repository-measure row. The 45-day-old
    # alpha merge is real history, not this window's throughput.
    alpha = (hermetic / "alpha-backlog.html").read_text()
    aflow = _payload_flow(alpha)
    assert aflow["deliveries"]["code"] == 3
    assert sum(w["code"] for w in aflow["deliveries"]["weeks"]) == 3
    # Alpha's own cycle: 30, 6, 22 days elapsed.
    assert aflow["cycle"] == {"median_days": 22.0, "p85_days": 30.0, "n": 3}
    assert aflow["coverage"]["unlinked"] == 0
    roadmap = (hermetic / "alpha-roadmap.html").read_text()
    assert _payload_flow(roadmap)["deliveries"]["code"] == 3

    # The refresh really rendered the boards, not just the payloads. The
    # public backlog has no review column: its in-review node appears on the
    # roadmap's Now column, never on the backlog board.
    assert "Chart tooltips pass" in alpha
    assert "Reader pagination" not in alpha
    assert "Reader pagination" in roadmap

    # Privacy: public boards carry no fixture ids and no tmp path, and the
    # data payload clears every leak class. The scan targets the payload
    # JSON, the same narrowing the shipped whole-payload test uses: the raw
    # document carries CSS color literals the pr-reference pattern would
    # false-positive on.
    for name in ("alpha-backlog.html", "alpha-roadmap.html"):
        html = (hermetic / name).read_text()
        for nid in ("ab-m1a2b3c4", "ab-w4j7k8l9", "ab-b8r1s2t3"):
            assert nid not in html, f"{name} carries {nid}"
        assert str(hermetic) not in html
        assert "merge_status" not in html
        payload = _payload_json(html)
        for leak_name, pattern in LEAK_PATTERNS:
            assert not pattern.search(payload), f"{name} payload leaks {leak_name}"
