"""The origin/related reader on ``fno backlog provenance`` (x-d157, Part C).

source_node_id shipped a month before anything read it back, which is why the
field looked broken while working correctly: a write-only field is
indistinguishable from one that is not being written. These tests pin the read
side so that cannot recur silently.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _node(node_id: str, **over) -> dict:
    base = {
        "id": node_id,
        "title": f"Node {node_id}",
        "_status": "ready",
        "domain": "code",
        "project": "fno",
        "slug": f"node-{node_id}",
    }
    base.update(over)
    return base


@pytest.fixture
def graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"

    def seed(entries: list[dict]) -> Path:
        g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")
        return g

    import fno.graph._constants as gc
    import fno.graph.store as gs

    # Patch the resolver FUNCTIONS, never the lazy module attrs: GRAPH_JSON is
    # __getattr__-provided, so monkeypatching the name pins the resolved path
    # into the module dict at teardown and every later _graph_path() reader in
    # the process sees this tmp graph forever.
    monkeypatch.setattr(gc, "_graph_json", lambda: g)
    monkeypatch.setattr(gc, "_graph_md", lambda: tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    for var in ("FNO_NODE", "CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    seed([])
    return seed


# ---------------------------------------------------------------------------
# AC1-UI: the origin renders, including when it is null
# ---------------------------------------------------------------------------


def test_ac1_ui_origin_renders_with_its_title(graph):
    """The line reads on its own; no second lookup to learn what the origin was."""
    graph([
        _node("x-aaaa", title="The origin"),
        _node("x-bbbb", source_node_id="x-aaaa"),
    ])
    result = runner.invoke(app, ["backlog", "provenance", "x-bbbb"])
    assert result.exit_code == 0, result.output
    assert "origin: x-aaaa (The origin)" in result.output


def test_ac1_ui_null_origin_renders_explicitly(graph):
    """A missing origin says so rather than dropping the line.

    An omitted line reads as "this verb does not report origins", which is
    precisely how the field stayed invisible for a month.
    """
    graph([_node("x-aaaa")])
    result = runner.invoke(app, ["backlog", "provenance", "x-aaaa"])
    assert result.exit_code == 0, result.output
    assert "origin: (none)" in result.output


def test_ac1_ui_filing_receipt_names_the_origin_on_stderr(graph, monkeypatch):
    """A filing that resolved an origin says which one; stdout stays JSON."""
    graph([_node("x-aaaa")])
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-1")
    monkeypatch.setenv("FNO_NODE", "x-aaaa")

    result = runner.invoke(app, ["backlog", "idea", "follow-up"])
    assert result.exit_code == 0, result.output
    assert "origin: x-aaaa" in result.output
    # stdout remains a clean JSON payload for `| jq` consumers.
    assert json.loads(result.stdout)["title"] == "follow-up"


def test_related_renders_both_populated_and_empty(graph):
    graph([
        _node("x-aaaa", related=["x-bbbb"]),
        _node("x-bbbb", title="The peer", related=["x-aaaa"]),
        _node("x-cccc"),
    ])
    out = runner.invoke(app, ["backlog", "provenance", "x-aaaa"]).output
    assert "x-bbbb (The peer)" in out
    assert "related: (none)" in runner.invoke(
        app, ["backlog", "provenance", "x-cccc"]
    ).output


def test_json_output_carries_origin_and_related(graph):
    graph([
        _node("x-aaaa", title="The origin"),
        _node("x-bbbb", source_node_id="x-aaaa", related=["x-aaaa"]),
    ])
    result = runner.invoke(app, ["backlog", "provenance", "x-bbbb", "--json"])
    payload = json.loads(result.stdout)
    assert payload["source_node_id"] == "x-aaaa"
    assert payload["source_node_title"] == "The origin"
    assert payload["related"] == ["x-aaaa"]


# ---------------------------------------------------------------------------
# AC5-HP / AC1-EDGE / AC2-EDGE: the reverse walk
# ---------------------------------------------------------------------------


def test_ac5_hp_reverse_walk_depth_is_traversal_derived(graph):
    """AC5-HP: depth comes from the traversal, so a fourth node reports depth 3."""
    graph([
        _node("x-aaaa"),
        _node("x-bbbb", source_node_id="x-aaaa"),
        _node("x-cccc", source_node_id="x-bbbb"),
        _node("x-dddd", source_node_id="x-cccc"),
    ])
    payload = json.loads(
        runner.invoke(
            app, ["backlog", "provenance", "x-aaaa", "--spawned", "--json"]
        ).stdout
    )
    assert {n["id"]: n["depth"] for n in payload["spawned"]["nodes"]} == {
        "x-bbbb": 1,
        "x-cccc": 2,
        "x-dddd": 3,
    }


def test_ac1_edge_cycle_terminates_keeps_results_and_is_flagged(graph):
    """AC1-EDGE: a mutually-attributing pair truncates rather than recursing.

    The walk must still return the descendant it found. Returning an empty set
    with a cycle flag would satisfy "terminates" while discarding the answer -
    the silent no-op this AC exists to catch.
    """
    graph([
        _node("x-aaaa", source_node_id="x-bbbb"),
        _node("x-bbbb", source_node_id="x-aaaa"),
    ])
    result = runner.invoke(
        app, ["backlog", "provenance", "x-aaaa", "--spawned", "--json"]
    )
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    ids = [n["id"] for n in payload["spawned"]["nodes"]]
    assert ids == ["x-bbbb"], "the real descendant must survive the cycle guard"
    assert payload["spawned"]["cycle_detected"] is True


def test_ac2_edge_no_descendants_states_so_and_exits_zero(graph):
    """AC2-EDGE: an empty walk is an empty result, not an error."""
    graph([_node("x-aaaa")])
    result = runner.invoke(app, ["backlog", "provenance", "x-aaaa", "--spawned"])
    assert result.exit_code == 0, result.output
    assert "spawned: (none)" in result.output


def test_walk_is_bounded_by_the_depth_cap(graph):
    """A legitimately deep chain truncates and says so, rather than dumping it all."""
    from fno.graph.cli import _SPAWNED_MAX_DEPTH

    chain = [_node("x-0000")]
    for i in range(1, _SPAWNED_MAX_DEPTH + 3):
        chain.append(_node(f"x-{i:04d}", source_node_id=f"x-{i - 1:04d}"))
    graph(chain)

    payload = json.loads(
        runner.invoke(
            app, ["backlog", "provenance", "x-0000", "--spawned", "--json"]
        ).stdout
    )
    assert len(payload["spawned"]["nodes"]) == _SPAWNED_MAX_DEPTH
    assert payload["spawned"]["truncated_at_depth"] == _SPAWNED_MAX_DEPTH


def test_spawned_is_opt_in(graph):
    """Without --spawned the verb output is unchanged for existing consumers."""
    graph([_node("x-aaaa"), _node("x-bbbb", source_node_id="x-aaaa")])
    payload = json.loads(
        runner.invoke(app, ["backlog", "provenance", "x-aaaa", "--json"]).stdout
    )
    assert "spawned" not in payload


def test_a_chain_ending_exactly_at_the_cap_is_not_reported_truncated(graph):
    """Truncation means something was cut, not that the walk used its whole budget."""
    from fno.graph.cli import _SPAWNED_MAX_DEPTH

    chain = [_node("x-0000")]
    for i in range(1, _SPAWNED_MAX_DEPTH + 1):
        chain.append(_node(f"x-{i:04d}", source_node_id=f"x-{i - 1:04d}"))
    graph(chain)

    payload = json.loads(
        runner.invoke(
            app, ["backlog", "provenance", "x-0000", "--spawned", "--json"]
        ).stdout
    )
    assert len(payload["spawned"]["nodes"]) == _SPAWNED_MAX_DEPTH
    assert payload["spawned"]["truncated_at_depth"] is None


# ---------------------------------------------------------------------------
# x-015c: the lifecycle roster (start / duration / honest total)
# ---------------------------------------------------------------------------


def _row(phase, harness="claude", session_id="sess-A", **times):
    r = {"phase": phase, "harness": harness, "session_id": session_id}
    r.update(times)
    return r


def test_roster_all_four_phases_with_honest_windows_summing(graph):
    """Canonical started_at+ended_at on the four build phases (no review row):
    each shows its window, the total is the sum, 4 of 5 phases recorded and
    review alone renders not recorded."""
    graph([_node("x-aaaa", sessions=[
        _row("think", started_at="2026-08-07T00:00:00Z", ended_at="2026-08-07T01:00:00Z"),
        _row("blueprint", started_at="2026-08-07T01:30:00Z", ended_at="2026-08-07T02:00:00Z"),
        _row("do", started_at="2026-08-07T02:30:00Z", ended_at="2026-08-07T05:00:00Z"),
        _row("ship", started_at="2026-08-07T05:30:00Z", ended_at="2026-08-07T06:00:00Z"),
    ])])
    out = runner.invoke(app, ["backlog", "provenance", "x-aaaa"]).output
    assert "4 of 5 phases recorded" in out
    assert out.count("not recorded") == 1
    assert "review" in out
    assert "4h30m" in out  # 1h + 30m + 2h30m + 30m


def test_roster_do_only_honest_window_marks_three_not_recorded(graph):
    """One honest do window: four phases render 'not recorded', the total names
    '1 of 4' rather than summing silently over the gaps."""
    graph([_node("x-aaaa", sessions=[
        _row("do", started_at="2026-08-07T02:30:00Z", ended_at="2026-08-07T05:00:00Z"),
    ])])
    out = runner.invoke(app, ["backlog", "provenance", "x-aaaa"]).output
    assert out.count("not recorded") == 4
    assert "1 of 5 phases recorded" in out
    assert "2h30m" in out


def test_roster_end_only_row_renders_no_duration(graph):
    """A row with an end but no start renders 'end only', never '0m', and
    contributes no window to the total."""
    graph([_node("x-aaaa", sessions=[
        _row("do", ended_at="2026-08-07T05:00:00Z"),
    ])])
    out = runner.invoke(app, ["backlog", "provenance", "x-aaaa"]).output
    assert "end only" in out
    assert "0m" not in out
    assert "0 of 5 phases recorded" in out


def test_roster_in_progress_row_renders_no_duration(graph):
    """A row with a start but no end (work in flight) renders 'in progress',
    never a duration - an open row is not a closed window."""
    graph([_node("x-aaaa", sessions=[
        _row("do", started_at="2026-08-07T02:30:00Z"),
    ])])
    out = runner.invoke(app, ["backlog", "provenance", "x-aaaa"]).output
    assert "in progress" in out
    assert "0m" not in out
    assert "0 of 5 phases recorded" in out


def test_roster_legacy_rows_are_not_summed_as_durations(graph):
    """Legacy rows (claimed_at/at) hold stamp-fire time, not phase boundaries -
    their span is the whole session - so they render 'end only' and are never
    summed or shown as a duration. The reader still surfaces their start in JSON."""
    graph([_node("x-aaaa", sessions=[
        _row("do", claimed_at="2026-08-07T00:00:00Z", at="2026-08-07T05:00:00Z"),
    ])])
    out = runner.invoke(app, ["backlog", "provenance", "x-aaaa"]).output
    assert "end only" in out
    assert "5h" not in out  # the defective session-span duration must not appear
    assert "0 of 5 phases recorded" in out
    payload = json.loads(
        runner.invoke(app, ["backlog", "provenance", "x-aaaa", "--json"]).stdout
    )
    do = next(p for p in payload["lifecycle"]["phases"] if p["phase"] == "do")
    assert do["start"] == "2026-08-07T00:00:00Z"  # claimed_at read as the start
    assert do["duration_seconds"] is None


def test_roster_json_absent_values_are_null_not_zero(graph):
    """-J emits the same fields with absent values as null, never 0."""
    graph([_node("x-aaaa", sessions=[
        _row("do", ended_at="2026-08-07T05:00:00Z"),  # end only -> null duration
    ])])
    payload = json.loads(
        runner.invoke(app, ["backlog", "provenance", "x-aaaa", "--json"]).stdout
    )
    lc = payload["lifecycle"]
    do = next(p for p in lc["phases"] if p["phase"] == "do")
    assert do["start"] is None
    assert do["duration_seconds"] is None
    assert lc["phases_recorded"] == 0
    assert lc["total_duration_seconds"] is None
    assert lc["phases_total"] == 5
    # a missing phase is recorded=False, not a fabricated zero-duration row
    assert next(p for p in lc["phases"] if p["phase"] == "think")["recorded"] is False
