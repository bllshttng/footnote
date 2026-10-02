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
    # The retirement verdict reads the graph; blanked so a unit test here
    # never touches the operator's real ~/.fno/graph.json.
    from fno.agents import retirement

    monkeypatch.setattr(retirement, "verdicts", lambda rows, entries=None: {})
    # The claim join reads the operator's real claims dir and shells the CLI
    # for the PR; blanked so every test here stays hermetic.
    import fno.agents.top as top

    monkeypatch.setattr(top, "_claim_sessions", lambda: {})
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


def test_foreign_row_progress_joins_registry_through_session_id(
    patched, monkeypatch
):
    """AC4-HP (x-1379): a foreign claude row is labelled by the FIRST 8 hex of
    the session uuid while the registry entry is keyed by the handle, so the
    PROGRESS axis joins through the session uuid - the same bridge the crown
    join already uses - instead of rendering `-` on every such row."""
    from fno.agents import registry
    from fno.agents.registry import AgentEntry

    entry = AgentEntry(
        name="last8reg",
        cwd="/w",
        log_path="",
        harness="claude",
        harness_session_id="full-session-uuid",
    )
    monkeypatch.setattr(registry, "load_registry", lambda: [entry])
    patched["answers"]["first8row"] = _answer("first8row", "working", 45)
    (row,) = _rows(
        patched,
        [
            _worker(
                source="claude",
                name="first8row",
                session_id="full-session-uuid",
            )
        ],
    )
    assert row["progress"] == "advancing"


def test_claim_join_renders_node_and_pr(patched, monkeypatch, tmp_path):
    """x-54ba: the session's own claim answers before any name-keyed join.

    A revived claude-store-only session (reaped row, adopt minted a fresh one)
    holds a live ``node:<id>`` claim whose holder names its full session id.
    The row renders that node and the node's PR, with the basis naming the
    claim - never the null an unresolvable name produced."""
    import fno.agents.top as top

    monkeypatch.setattr(
        top,
        "_claim_sessions",
        lambda: {
            "979e1acc-e240-4af5-9998-0a74ec6c0683": ("x-4dc0", 2965),
            "full-session-uuid": ("x-06f7", None),
        },
    )
    (row,) = _rows(
        patched,
        [
            _worker(
                source="claude",
                name="979e1acc",
                session_id="979e1acc-e240-4af5-9998-0a74ec6c0683",
            )
        ],
    )
    assert row["node"] == "x-4dc0"
    assert row["node_basis"] == "claim"
    assert row["pr"] == 2965
    assert row["pr_basis"] == "node"

    # A claim with no PR yet still names the node (pr_basis no-pr).
    (alone,) = _rows(
        patched,
        [_worker(source="claude", name="0a4aad70", session_id="full-session-uuid")],
    )
    assert alone["node"] == "x-06f7"
    assert alone["pr"] is None
    assert alone["pr_basis"] == "no-pr"


def test_claim_sessions_reads_the_real_yaml_lockfile(tmp_path, monkeypatch):
    """The join's reader, not a stub: a real ``node:`` claim lockfile (YAML,
    global-rooted) parses through fno.claims' own reader, so the parse and
    the root cannot silently rot."""
    from fno.agents.top import _claim_sessions
    from fno.claims.io import claim_path, global_claims_root, serialize_claim
    from fno.claims.types import Claim

    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    lock = claim_path("node:x-join", root=global_claims_root())
    lock.parent.mkdir(parents=True, exist_ok=True)
    lock.write_text(
        serialize_claim(
            Claim(
                key="node:x-join",
                holder="target-session:SID-X",
                acquired_at=1_700_000_000_000,
                pid=41468,
                host="test",
            )
        ),
        encoding="utf-8",
    )
    assert _claim_sessions() == {"sid-x": ("x-join", None)}


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
    monkeypatch.setattr(top, "_tree_rss", lambda pids: {41468: 297})
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
    monkeypatch.setattr(top, "_tree_rss", lambda pids: {41468: 297})
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


def test_retirable_line_renders_under_lanes_and_none_when_empty(
    patched, monkeypatch
):
    """AC3-HP/EDGE (x-1379): a lane holder whose node is done and merged gets
    its `retirable:` line under LANES; with no retirable row, nothing extra
    renders. The live fleet rarely carries a retirable holder at scan time,
    so the positive case is pinned here rather than against the world."""
    import fno.agents.top as top
    from fno.agents.retirement import Retirement

    def _render():
        class _census:
            warnings: list[str] = []
            slot_claims = 0
            workers = [
                _worker(name="1a2b3c4d", session_id="full-session-uuid")
            ]

        monkeypatch.setattr(top, "census", lambda: _census)
        monkeypatch.setattr(
            top,
            "lane_rows",
            lambda: [
                {"provider": "zai", "cap": 10, "count": 1, "holders": ["t-06f7-row"]}
            ],
        )
        monkeypatch.setattr(top, "_tree_rss", lambda pids: {41468: 297})
        return top.render_top()

    patched["answers"]["1a2b3c4d"] = _answer("1a2b3c4d", "working", 45)
    monkeypatch.setattr(
        top,
        "_registry_maps",
        lambda: (
            {"full-session-uuid": "t-06f7-row"},
            {"t-06f7-row": "x-06f7"},
        ),
    )
    import fno.agents.retirement as retirement

    monkeypatch.setattr(
        retirement,
        "verdicts",
        lambda rows, entries=None: {
            "t-06f7-row": Retirement("x-06f7", "name", True, "done+merged PR 1553")
        },
    )
    text = _render()
    assert (
        "retirable: 1a2b3c4d holds a zai lane; "
        "x-06f7 is done, merged at PR 1553"
    ) in text
    assert "NODE" in text

    # A merged node with no recorded PR number still renders, without a
    # bogus "at PR" clause.
    monkeypatch.setattr(
        retirement,
        "verdicts",
        lambda rows, entries=None: {
            "t-06f7-row": Retirement("x-06f7", "name", True, "done+merged")
        },
    )
    assert "retirable: 1a2b3c4d holds a zai lane; x-06f7 is done, merged" in (
        _render()
    )

    monkeypatch.setattr(retirement, "verdicts", lambda rows, entries=None: {})
    assert "retirable:" not in _render()
