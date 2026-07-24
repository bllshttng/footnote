"""x-88df: transcript-exists probe (US4) + direct-finalize middle rung (US5).

The finalize invocation is injected via the ``finalize_origin`` seam so no real
``fno-agents finalize`` fires; ``resolve_warm_session`` is stubbed so liveness is
deterministic. The cold ritual verb is injected via ``run_verb`` so no real
``fno pr ritual`` subprocess fires. A separate slow-marked smoke test exercises
the REAL binary.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.post_merge_route import (
    ColdRitualResult,
    dispatch_post_merge_ritual,
    origin_transcript_exists,
)


@pytest.fixture(autouse=True)
def _hermetic_receipts(monkeypatch):
    """Keep the receipt log hermetic: dispatch appends to the global events log
    by default; redirect to a no-op so tests do not pollute ~/.fno/events.jsonl."""
    import fno.post_merge_route as pmr

    monkeypatch.setattr(pmr, "emit_receipt", lambda *a, **k: True)


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

class _Verb:
    """A cold-path verb-runner seam: records (pr, cwd), returns ok."""

    def __init__(self):
        self.calls: list = []

    def __call__(self, pr_number, cwd):
        self.calls.append((pr_number, cwd))
        return ColdRitualResult(ok=True, tail="ok")


def _dead_origin(monkeypatch):
    """Force an explicit family-1 death verdict for the origin."""
    import fno.post_merge_route as route
    monkeypatch.setattr(route, "resolve_warm_session", lambda *a, **k: None)
    monkeypatch.setattr(route, "session_death_confirmed", lambda *a, **k: True)


def test_unknown_origin_never_direct_finalizes(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-unknown")
    _point_projects(monkeypatch, projects)
    import fno.post_merge_route as route

    monkeypatch.setattr(route, "resolve_warm_session", lambda *a, **k: None)
    monkeypatch.setattr(route, "session_death_confirmed", lambda *a, **k: False)
    finalized: list = []
    verb = _Verb()

    res = dispatch_post_merge_ritual(
        6, dedup_key="shaUnknown", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sid-unknown", source_harness="claude",
        source_cwd=cwd, finalize_origin=lambda *a: finalized.append(1) or True,
    )

    assert res.outcome == "dispatched"
    assert finalized == []
    assert verb.calls == [(6, str(tmp_path))]


def test_transcript_only_dead_origin_direct_finalizes_without_truth_stub(
    tmp_path, monkeypatch
):
    cwd, projects = _mk_origin(tmp_path, "sid-transcript-dead")
    _point_projects(monkeypatch, projects)
    transcript = next(projects.glob("*/sid-transcript-dead.jsonl"))
    transcript.write_text(
        '{"type":"assistant","message":{"content":[{"type":"text",'
        '"text":"<promise>done</promise>"}]}}\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(
        "fno.post_merge_route.resolve_warm_session", lambda *a, **k: None
    )
    finalized: list = []

    res = dispatch_post_merge_ritual(
        61,
        dedup_key="shaTranscriptDead",
        auto_run=True,
        canonical_root=tmp_path,
        run_verb=_Verb(),
        source_session_id="sid-transcript-dead",
        source_harness="claude",
        source_cwd=cwd,
        finalize_origin=lambda *a: finalized.append(a) or True,
    )

    assert res.outcome == "finalized-origin"
    assert len(finalized) == 1


def test_dead_origin_direct_finalizes_then_runs_ritual_cold(tmp_path, monkeypatch):
    # Finalize writes the ledger row, THEN falls through to the cold verb so the
    # post-merge ritual (retro/parking-lot/canonical-sync) still runs.
    cwd, projects = _mk_origin(tmp_path, "sid-live")
    _point_projects(monkeypatch, projects)
    _dead_origin(monkeypatch)

    seam_calls: list = []

    def _fin(source_cwd, transcript, harness):
        seam_calls.append((source_cwd, transcript, harness))
        return True

    verb = _Verb()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaF", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sid-live", source_harness="claude",
        source_cwd=cwd, finalize_origin=_fin,
    )
    assert res.outcome == "finalized-origin"  # ledger came from direct finalize
    assert verb.calls == [(7, str(tmp_path))]  # ritual STILL ran cold
    assert len(seam_calls) == 1
    assert seam_calls[0][0] == cwd and seam_calls[0][2] == "claude"
    assert seam_calls[0][1].endswith("sid-live.jsonl")
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaF").exists()


def test_finalize_nonzero_degrades_to_cold(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-fail")
    _point_projects(monkeypatch, projects)
    _dead_origin(monkeypatch)

    verb = _Verb()
    res = dispatch_post_merge_ritual(
        8, dedup_key="shaG", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sid-fail", source_harness="claude",
        source_cwd=cwd, finalize_origin=lambda *a: False,  # non-zero
    )
    assert res.outcome == "dispatched"  # AC2-ERR: degrade, never raise
    assert verb.calls == [(8, str(tmp_path))]
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaG").exists()


def test_missing_manifest_falls_to_cold(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-nomani", manifest=False)
    _point_projects(monkeypatch, projects)
    _dead_origin(monkeypatch)

    called: list = []
    verb = _Verb()
    res = dispatch_post_merge_ritual(
        9, dedup_key="shaH", auto_run=True, canonical_root=tmp_path,
        run_verb=verb, source_session_id="sid-nomani", source_harness="claude",
        source_cwd=cwd, finalize_origin=lambda *a: called.append(1) or True,
    )
    assert res.outcome == "dispatched"  # probe False -> rung skipped
    assert called == []  # finalize never invoked
    assert verb.calls == [(9, str(tmp_path))]


def test_live_origin_never_direct_finalized(tmp_path, monkeypatch):
    cwd, projects = _mk_origin(tmp_path, "sid-alive")
    _point_projects(monkeypatch, projects)
    # Origin is LIVE: resolve_warm_session returns a sid; warm inject delivers.
    import fno.post_merge_route as route
    monkeypatch.setattr(route, "resolve_warm_session", lambda *a, **k: "sid-alive")

    fin_called: list = []
    res = dispatch_post_merge_ritual(
        10, dedup_key="shaI", auto_run=True, canonical_root=tmp_path,
        run_verb=_Verb(), source_session_id="sid-alive", source_harness="claude",
        source_cwd=cwd,
        warm_inject=lambda *a: (True, "delivered"),
        finalize_origin=lambda *a: fin_called.append(1) or True,
    )
    assert res.outcome == "routed-warm"
    assert fin_called == []  # AC1-EDGE: live origin never direct-finalized
