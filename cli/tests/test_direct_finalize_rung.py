"""x-88df: transcript-exists probe (US4) + direct-finalize middle rung (US5).

The finalize invocation is injected via the ``finalize_origin`` seam so no real
``fno-agents finalize`` fires; ``resolve_warm_session`` is stubbed so liveness is
deterministic. A separate slow-marked smoke test exercises the REAL binary.
"""
from __future__ import annotations

from pathlib import Path

import pytest

import fno.graph._reconcile as _reconcile
from fno.graph._reconcile import (
    dispatch_post_merge_ritual,
    origin_transcript_exists,
)


# --- US4: origin_transcript_exists probe ----------------------------------

def _mk_origin(tmp_path: Path, sid: str, *, manifest: bool = True) -> tuple[str, Path]:
    """Build a fake origin: cwd with .fno/target-state.md + a transcript jsonl.

    Returns (source_cwd, projects_dir). The transcript lands under the cwd's
    encoded projects subdir so the real _candidate_dir_names encoding resolves it.
    """
    from fno.agents.discover import _candidate_dir_names

    cwd = tmp_path / "origin-wt"
    cwd.mkdir(parents=True, exist_ok=True)
    if manifest:
        (cwd / ".fno").mkdir(parents=True, exist_ok=True)
        (cwd / ".fno" / "target-state.md").write_text("---\nstatus: IN_PROGRESS\n---\n")
    projects = tmp_path / "projects"
    enc = _candidate_dir_names(str(cwd))[0]
    (projects / enc).mkdir(parents=True, exist_ok=True)
    (projects / enc / f"{sid}.jsonl").write_text('{"type":"user"}\n')
    return str(cwd), projects


def _point_projects(monkeypatch, projects: Path):
    monkeypatch.setenv("FNO_CLAUDE_PROJECTS_DIR", str(projects))


def test_probe_true_when_transcript_and_manifest_present(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-1")
    _point_projects(monkeypatch, projects)
    assert origin_transcript_exists("sid-1", cwd, "claude") is True


def test_probe_false_when_manifest_missing(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-2", manifest=False)
    _point_projects(monkeypatch, projects)
    assert origin_transcript_exists("sid-2", cwd, "claude") is False


def test_probe_false_when_transcript_missing(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-3")
    _point_projects(monkeypatch, projects)
    assert origin_transcript_exists("nope", cwd, "claude") is False


def test_probe_false_for_non_claude_harness(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-4")
    _point_projects(monkeypatch, projects)
    assert origin_transcript_exists("sid-4", cwd, "codex") is False


# --- US5: the direct-finalize middle rung ---------------------------------

class _Spawn:
    def __init__(self, short_id="cold1"):
        self.short_id = short_id
        self.calls: list = []

    def __call__(self, pr_number, cwd):
        self.calls.append((pr_number, cwd))
        return self.short_id


def _dead_origin(monkeypatch):
    """Force resolve_warm_session -> None so the origin reads as dead."""
    import fno.post_merge_route as route
    monkeypatch.setattr(route, "resolve_warm_session", lambda *a, **k: None)


def test_dead_origin_direct_finalizes_then_runs_ritual_cold(tmp_path, monkeypatch):
    # Finalize writes the ledger row, THEN falls through to the cold spawn so
    # the post-merge ritual (retro/parking-lot/canonical-sync) still runs.
    cwd, projects = _mk_origin(tmp_path, "sid-live")
    _point_projects(monkeypatch, projects)
    _dead_origin(monkeypatch)

    seam_calls: list = []

    def _fin(source_cwd, transcript, harness):
        seam_calls.append((source_cwd, transcript, harness))
        return True

    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaF", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sid-live", source_harness="claude",
        source_cwd=cwd, finalize_origin=_fin,
    )
    assert res.outcome == "finalized-origin"  # ledger came from direct finalize
    assert spawn.calls == [(7, str(tmp_path))]  # ritual STILL dispatched cold
    assert len(seam_calls) == 1
    assert seam_calls[0][0] == cwd and seam_calls[0][2] == "claude"
    assert seam_calls[0][1].endswith("sid-live.jsonl")
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaF").exists()


def test_finalize_nonzero_degrades_to_cold(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-fail")
    _point_projects(monkeypatch, projects)
    _dead_origin(monkeypatch)

    spawn = _Spawn(short_id="coldX")
    res = dispatch_post_merge_ritual(
        8, dedup_key="shaG", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sid-fail", source_harness="claude",
        source_cwd=cwd, finalize_origin=lambda *a: False,  # non-zero
    )
    assert res.outcome == "dispatched"  # AC2-ERR: degrade, never raise
    assert spawn.calls == [(8, str(tmp_path))]
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaG").exists()


def test_missing_manifest_falls_to_cold(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-nomani", manifest=False)
    _point_projects(monkeypatch, projects)
    _dead_origin(monkeypatch)

    called: list = []
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        9, dedup_key="shaH", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sid-nomani", source_harness="claude",
        source_cwd=cwd, finalize_origin=lambda *a: called.append(1) or True,
    )
    assert res.outcome == "dispatched"  # probe False -> rung skipped
    assert called == []  # finalize never invoked
    assert spawn.calls == [(9, str(tmp_path))]


def test_live_origin_never_direct_finalized(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-alive")
    _point_projects(monkeypatch, projects)
    # Origin is LIVE: resolve_warm_session returns a sid; warm inject delivers.
    import fno.post_merge_route as route
    monkeypatch.setattr(route, "resolve_warm_session", lambda *a, **k: "sid-alive")

    fin_called: list = []
    res = dispatch_post_merge_ritual(
        10, dedup_key="shaI", auto_run=True, canonical_root=tmp_path,
        spawn=_Spawn(), source_session_id="sid-alive", source_harness="claude",
        source_cwd=cwd,
        warm_inject=lambda *a: (True, "delivered"),
        finalize_origin=lambda *a: fin_called.append(1) or True,
    )
    assert res.outcome == "routed-warm"
    assert fin_called == []  # AC1-EDGE: live origin never direct-finalized
