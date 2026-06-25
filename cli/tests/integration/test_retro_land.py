"""Integration tests for retro land hybrid-by-mode (Wave 4.1, US4)."""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.retro import land
from fno.retro.land import (
    MODE_AUTONOMOUS,
    MODE_INTERACTIVE,
    land_candidates,
    resolve_mode,
)
from fno.retro.types import TIER_INBOX, TIER_NODE, Candidate


def _node(title="t", tier=TIER_NODE, pr=343, sid="c1", chash="abc") -> Candidate:
    return Candidate(
        title=title, body="reasoning", tier=tier, priority="p1",
        source_pr=pr, source_id=sid, content_hash=chash,
    )


class _Recorder:
    def __init__(self):
        self.created = []
        self.inbox = []
        self._n = 0

    def create(self, *, title, details, priority, project, cwd, domain="code", queued=False):
        self._n += 1
        nid = f"ab-{self._n:08d}"
        self.created.append(
            {"id": nid, "title": title, "details": details, "cwd": cwd, "queued": queued}
        )
        return nid

    def inbox_append(self, candidate):
        self.inbox.append(candidate)


def test_ac4_hp_autonomous_creates_active(tmp_path: Path):
    """AC4-HP: autonomous mode -> node created active (queued=False)."""
    rec = _Recorder()
    results = land_candidates(
        [_node()], mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert len(rec.created) == 1
    assert rec.created[0]["queued"] is False  # active, not queued
    assert results[0].outcome == "active"


def test_ac4_ui_interactive_queues(tmp_path: Path):
    """AC4-UI: interactive mode -> node created ALREADY queued (one atomic mutation)."""
    rec = _Recorder()
    results = land_candidates(
        [_node()], mode=MODE_INTERACTIVE, repo_root=tmp_path,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert len(rec.created) == 1
    assert rec.created[0]["queued"] is True  # created queued in the same step
    assert results[0].outcome == "queued"


def test_ac4_edge_nit_goes_to_inbox(tmp_path: Path):
    """AC4-EDGE: a low/nit (tier=inbox) candidate -> inbox line, no node."""
    rec = _Recorder()
    results = land_candidates(
        [_node(tier=TIER_INBOX)], mode=MODE_INTERACTIVE, repo_root=tmp_path,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert rec.created == []
    assert len(rec.inbox) == 1
    assert results[0].outcome == "inbox"


def test_ac4_mode_absent_defaults_interactive():
    """AC4-MODE: sentinel without a readable mode -> interactive (safe)."""
    assert resolve_mode(None) == MODE_INTERACTIVE
    assert resolve_mode({}) == MODE_INTERACTIVE
    assert resolve_mode({"mode": "garbage"}) == MODE_INTERACTIVE
    assert resolve_mode({"mode": "autonomous"}) == MODE_AUTONOMOUS


def test_ac4_err_creation_failure_recorded(tmp_path: Path):
    """AC4-ERR: a creation failure is recorded (not raised) so the caller can retry."""
    def boom(**kwargs):
        raise TimeoutError("graph lock timeout")

    results = land_candidates(
        [_node()], mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=boom, inbox_fn=lambda s: None,
    )
    assert results[0].outcome == "failed"
    assert land.has_failures(results)


def test_ac4_fr_partial_progress_persists(tmp_path: Path):
    """AC4-FR: when one of several fails, the others still land (re-run dedups the rest)."""
    rec = _Recorder()
    n_calls = {"i": 0}

    def flaky(**kwargs):
        n_calls["i"] += 1
        if n_calls["i"] == 2:
            raise TimeoutError("lock timeout on #2")
        return rec.create(**kwargs)

    cands = [_node(sid="a"), _node(sid="b"), _node(sid="c")]
    results = land_candidates(
        cands, mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=flaky, inbox_fn=rec.inbox_append,
    )
    outcomes = [r.outcome for r in results]
    assert outcomes.count("active") == 2
    assert outcomes.count("failed") == 1


def test_node_details_carry_dedup_trailer(tmp_path: Path):
    """Landed node details include the machine trailer so a re-run dedups it."""
    rec = _Recorder()
    land_candidates(
        [_node(chash="deadbeef")], mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert "retro-triage source_pr=343 finding_hash=deadbeef" in rec.created[0]["details"]


def test_inbox_default_appends_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Without a vault, the default capture writer appends to the canonical
    .fno/backlog/parking-lot.md (the default was renamed from inbox.md).

    Settings are isolated to obsidian-off so the resolved default does not
    depend on the dev's ambient ~/.fno/settings.yaml (which is why this
    previously passed locally on an obsidian-enabled machine but failed in CI).
    """
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    land_candidates(
        [_node(tier=TIER_INBOX, title="a nit")], mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=lambda **k: "x",
    )
    inbox = tmp_path / ".fno" / "backlog" / "parking-lot.md"
    assert inbox.exists()
    assert "a nit" in inbox.read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# v2 A1 - the retro-harvest birth path routes through on_node_born
# (the exact gap this epic fixes: x-7c38 / x-6e23 filed with no /think).
# ---------------------------------------------------------------------------


def test_a1_birth_hook_fires_per_created_node_sharing_one_run_state(tmp_path, monkeypatch):
    """Every retro-filed node routes through on_node_born once; the whole batch
    threads ONE RunState so the blast cap bounds the harvest, not each node."""
    import fno.provenance.spawn_think as st

    calls: list[str] = []
    seen_rs: list = []

    def fake(node, *, run_state=None, **k):
        calls.append(node["id"])
        seen_rs.append(run_state)

    monkeypatch.setattr(st, "on_node_born", fake)
    rec = _Recorder()
    land_candidates(
        [_node(title="a", chash="h1"), _node(title="b", chash="h2")],
        mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert calls == ["ab-00000001", "ab-00000002"]
    assert seen_rs[0] is seen_rs[1] is not None  # one shared cap across the batch


def test_a1_birth_hook_skips_inbox_and_failed_creates(tmp_path, monkeypatch):
    """A nit landed to inbox (no node) and a failed create both skip the hook."""
    import fno.provenance.spawn_think as st

    calls: list[str] = []
    monkeypatch.setattr(st, "on_node_born", lambda node, **k: calls.append(node["id"]))

    rec = _Recorder()
    # inbox-tier -> appended, never a node
    land_candidates(
        [_node(tier=TIER_INBOX)], mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    # create raising -> recorded failure, never a birth
    def boom(**k):
        raise RuntimeError("lock timeout")

    land_candidates(
        [_node()], mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=boom, inbox_fn=rec.inbox_append,
    )
    assert calls == []


def test_a1_birth_hook_failure_is_non_fatal_to_harvest(tmp_path, monkeypatch):
    """A raising hook never breaks the harvest: the node still lands active."""
    import fno.provenance.spawn_think as st

    monkeypatch.setattr(
        st, "on_node_born",
        lambda node, **k: (_ for _ in ()).throw(RuntimeError("spawn leg exploded")),
    )
    rec = _Recorder()
    results = land_candidates(
        [_node()], mode=MODE_AUTONOMOUS, repo_root=tmp_path,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert results[0].outcome == "active"
