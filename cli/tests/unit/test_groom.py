"""Tests for the daily grooming pass (`fno backlog groom`).

Covers the once-a-day dedup marker, the spawn shape, the failure hand-back, and
the skill brief's lever contract.
"""
from __future__ import annotations

from datetime import date
from pathlib import Path

import pytest

from fno.backlog import groom as G

SKILL = Path(__file__).resolve().parents[3] / "skills" / "groom" / "SKILL.md"


@pytest.fixture
def claims_root(tmp_path, monkeypatch) -> Path:
    """Route the groom: claim into a tmp dir so the marker is hermetic."""
    root = tmp_path / "claims_home"
    root.mkdir()
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(root))
    return root


@pytest.fixture
def spawns(monkeypatch) -> list:
    """Capture spawn calls instead of launching a real worker."""
    calls: list = []

    def _fake(brief: str, cwd: str, model: str, day: str) -> str:
        calls.append({"brief": brief, "cwd": cwd, "model": model, "day": day})
        return "gr01"

    monkeypatch.setattr(G, "_spawn_groom_worker", _fake)
    return calls


DAY = date(2026, 7, 19)


def test_day_key_is_utc_date_scoped():
    assert G.groom_day_key(DAY) == "groom:2026-07-19"


def test_groom_key_routes_to_the_global_claims_root():
    # Grooming operates on the GLOBAL graph, so its daily marker must dedup
    # across repos - a repo-local root would let two checkouts both groom today.
    from fno.claims.io import claims_root_for

    assert claims_root_for(G.groom_day_key(DAY)) is not None


def test_first_run_dispatches(claims_root, spawns):
    r = G.run_groom(cwd="/tmp", today=DAY)
    assert r["status"] == "dispatched"
    assert r["day"] == "2026-07-19"
    assert r["short_id"] == "gr01"
    assert len(spawns) == 1


def test_second_run_same_day_is_a_no_op(claims_root, spawns):
    first = G.run_groom(cwd="/tmp", today=DAY)
    second = G.run_groom(cwd="/tmp", today=DAY)

    assert first["status"] == "dispatched"
    assert second["status"] == "already-ran"
    assert len(spawns) == 1, "the second run must not spawn a worker"


def test_next_day_dispatches_again(claims_root, spawns):
    G.run_groom(cwd="/tmp", today=DAY)
    r = G.run_groom(cwd="/tmp", today=date(2026, 7, 20))

    assert r["status"] == "dispatched"
    assert len(spawns) == 2


def test_dry_run_neither_claims_nor_spawns(claims_root, spawns):
    r = G.run_groom(cwd="/tmp", today=DAY, dry_run=True)

    assert r["status"] == "dry-run"
    assert not spawns
    # No marker was written, so a real run today is still available.
    assert G.run_groom(cwd="/tmp", today=DAY)["status"] == "dispatched"


def test_failed_spawn_hands_the_day_back(claims_root, monkeypatch, spawns):
    # A transient spawn fault must not burn the day behind a marker nothing
    # clears until tomorrow.
    def _boom(*a, **k):
        raise RuntimeError("spawn exited 2")

    monkeypatch.setattr(G, "_spawn_groom_worker", _boom)
    failed = G.run_groom(cwd="/tmp", today=DAY)
    assert failed["status"] == "failed"
    assert "spawn exited 2" in failed["detail"]

    monkeypatch.setattr(G, "_spawn_groom_worker", lambda *a, **k: "gr02")
    assert G.run_groom(cwd="/tmp", today=DAY)["status"] == "dispatched"


def test_spawn_is_headless_sonnet(monkeypatch, claims_root):
    """The substrate and model are load-bearing: explicit headless, never `-p`."""
    captured: dict = {}

    class _Proc:
        returncode = 0
        stdout = '{"short_id": "gr03"}'
        stderr = ""

    def _fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(G.subprocess, "run", _fake_run)
    r = G.run_groom(cwd="/repo", today=DAY)

    assert r["short_id"] == "gr03"
    cmd = captured["cmd"]
    assert "--substrate" in cmd and cmd[cmd.index("--substrate") + 1] == "headless"
    assert cmd[cmd.index("--model") + 1] == G.GROOM_MODEL_DEFAULT
    assert cmd[cmd.index("--cwd") + 1] == "/repo"
    assert "-p" not in cmd, "the subscription lane never shells bare -p"


# ── the skill brief contract ────────────────────────────────────────────────


def test_brief_points_at_the_skill():
    brief = G.groom_brief("2026-07-19")
    assert "skills/groom/SKILL.md" in brief
    assert "2026-07-19" in brief


@pytest.mark.parametrize(
    "lever",
    ["supersede", "defer", "undefer", "--priority", "rank", "idea"],
)
def test_skill_names_every_allowed_lever(lever):
    assert lever in SKILL.read_text()


def test_skill_carries_the_auto_convene_and_report_contract():
    text = SKILL.read_text()
    assert "Auto-convene" in text
    assert "fno mail send" in text
    assert "Net mint rate" in text


def test_skill_forbids_direct_state_edits():
    text = SKILL.read_text()
    assert "graph.json" in text and "Never" in text
    for forbidden in ("jq -i", "sed -i"):
        assert forbidden in text, "the brief must name the direct-edit paths it forbids"
