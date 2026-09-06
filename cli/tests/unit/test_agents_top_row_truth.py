"""x-6d89: top renders the reachability verdict it already computes.

`_progress_map` (now ``_row_truth``) called ``resolve_session_truth`` and
``classify_reachability`` and returned only the progress verdict, so the row
that made a king mail an unreachable session rendered no reachability word and
no transcript age. These tests pin the row contract: reachability verdict,
transcript age, the foreign-row split (the age needs no harness context, the
progress verdict does), the JSON mirror, and the one-read-per-row budget.
"""

from __future__ import annotations

import json

import pytest

from fno.agents.spawn_gate import LiveWorker


def _worker(source="fno", name="t-06f7-row", **kw) -> LiveWorker:
    return LiveWorker(
        source=source,
        name=name,
        harness="claude",
        substrate="bg",
        pid=41468,
        status="live",
        **kw,
    )


@pytest.fixture
def patched(monkeypatch):
    """Blank the registry and install a scriptable ``resolve_session_truth``.

    The truth read is late-bound inside ``_row_truth`` from
    ``fno.agents.session_truth``, so patching the module attribute is the seam.
    ``answers`` maps handle -> the dict the read returns; unseen handles get
    the unreadable-transcript answer (state None, age None).
    """
    from fno.agents import registry, session_truth

    state = {"answers": {}, "reads": []}

    def fake_resolve(handle, **kw):
        state["reads"].append(handle)
        return state["answers"].get(
            handle,
            {
                "handle": handle,
                "state": None,
                "reason": "not-found",
                "last_activity_age_s": None,
                "last_event_at": None,
                "last_message": None,
            },
        )

    monkeypatch.setattr(session_truth, "resolve_session_truth", fake_resolve)
    monkeypatch.setattr(registry, "load_registry", lambda: [])
    return state


def _answer(handle, state, age_s):
    return {
        "handle": handle,
        "state": state,
        "reason": None,
        "last_activity_age_s": age_s,
        "last_event_at": None,
        "last_message": None,
    }


def _rows(patched, workers):
    from fno.agents.top import _rows

    return _rows(workers, {})


def test_stale_transcript_row_renders_the_verdict_not_bare_live(patched):
    """AC1-HP: stood down two hours ago, process alive -> the row says so."""
    patched["answers"]["t-06f7-row"] = _answer("t-06f7-row", "stalled", 7426)
    (row,) = _rows(patched, [_worker()])
    assert row["reach"] == "unknown"
    assert row["reach_basis"] == "silent"
    assert row["status_age_s"] == 7426
    assert row["status"] == "quiet"


def test_fresh_transcript_row_reads_reachable(patched):
    """AC2-HP: transcript written seconds ago -> reachable, age in seconds."""
    patched["answers"]["t-06f7-row"] = _answer("t-06f7-row", "working", 45)
    (row,) = _rows(patched, [_worker()])
    assert row["reach"] == "reachable"
    assert row["status_age_s"] == 45
    assert row["status"] == "writing"


def test_unreadable_transcript_never_renders_fresh_or_zero(patched):
    """AC4-EDGE: no readable transcript -> unknown, and the age is None."""
    (row,) = _rows(patched, [_worker()])
    assert row["status"] == "unknown"
    assert row["status_age_s"] is None
    assert row["reach"] == "unknown"


def test_foreign_row_carries_age_and_reach_without_a_registry_entry(patched):
    """AC5-EDGE: the age and the verdict need only the handle, not the registry.

    The PROGRESS verdict stays None for the reason the old skip documented (no
    harness/route context to judge a refusal against); the rest renders.
    """
    patched["answers"]["0a4aad70"] = _answer("0a4aad70", "stalled", 7426)
    (row,) = _rows(patched, [_worker(source="claude", name="0a4aad70")])
    assert row["status_age_s"] == 7426
    assert row["reach"] == "unknown"
    assert row["progress"] is None


def test_disagreement_is_visible_in_one_rendered_row(patched, monkeypatch):
    """AC3-HP: process alive + transcript stood down, one row says both."""
    import fno.agents.top as top

    patched["answers"]["t-06f7-row"] = _answer("t-06f7-row", "stalled", 7426)

    class _census:
        warnings: list[str] = []
        slot_claims = 0

        workers = [_worker()]

    monkeypatch.setattr(top, "census", lambda: _census)
    monkeypatch.setattr(top, "lane_rows", lambda: [])
    monkeypatch.setattr(top, "tree_rss_mb", lambda pid: 297.0)
    text = top.render_top()
    assert "REACH" in text
    assert "unknown" in text
    assert "quiet 2h" in text
    assert "41468" in text


def test_json_mirror_carries_new_fields_without_touching_old_ones(
    patched, monkeypatch
):
    """AC6-FR: reach and reach_basis are their own keys; status keeps meaning."""

    import fno.agents.top as top

    patched["answers"]["t-06f7-row"] = _answer("t-06f7-row", "working", 45)

    class _census:
        warnings: list[str] = []
        slot_claims = 0

        workers = [_worker()]

    monkeypatch.setattr(top, "census", lambda: _census)
    monkeypatch.setattr(top, "lane_rows", lambda: [])
    monkeypatch.setattr(top, "tree_rss_mb", lambda pid: 297.0)
    payload = json.loads(top.render_top(as_json=True))
    (row,) = payload["workers"]
    assert row["reach"] == "reachable"
    assert row["reach_basis"] == "transcript"
    assert row["status"] == "writing"
    assert row["stored_status"] == "live"
    assert row["status_age_s"] == 45


def test_one_transcript_read_per_row(patched):
    """AC7-FR: the reachability and progress verdicts share the one read."""
    patched["answers"]["t-06f7-row"] = _answer("t-06f7-row", "working", 45)
    patched["answers"]["second"] = _answer("second", "stalled", 7426)
    _rows(patched, [_worker(), _worker(name="second")])
    assert sorted(patched["reads"]) == ["second", "t-06f7-row"]
