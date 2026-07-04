"""W4 human_touch merge emitters (x-aff6).

The mux inject/answer emitters are Rust-side (crates/fno server unit tests);
these cover the two Python merge choke points: the tty-gated `fno pr merge`
followup and the reconcile out-of-band close.
"""
from __future__ import annotations

import json
import types
from pathlib import Path

from fno.graph._reconcile import MergeDriftRecord, emit_human_touch_for_record
from fno.pr import _merge


def _events(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(ln) for ln in path.read_text().splitlines() if ln.strip()]


def _fake_graph(tmp_path: Path, entries: list[dict]) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}))
    return p


def _tty(monkeypatch, value: bool) -> None:
    monkeypatch.setattr(
        _merge.sys, "stdin", types.SimpleNamespace(isatty=lambda: value)
    )


def test_manual_merge_emits_with_resolved_node(tmp_path, monkeypatch):
    """A tty merge emits human_touch{merge} carrying the pr's graph node."""
    graph = _fake_graph(
        tmp_path, [{"id": "x-1234", "title": "t", "pr_number": 42}]
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    _tty(monkeypatch, True)
    state_dir = tmp_path / ".fno"

    _merge._emit_human_touch_merge(42, str(state_dir))

    rows = _events(state_dir / "events.jsonl")
    assert len(rows) == 1
    assert rows[0]["type"] == "human_touch"
    assert rows[0]["data"] == {
        "graph_node_id": "x-1234",
        "source": "merge",
        "resolution": "ok",
    }


def test_loop_merge_never_emits(tmp_path, monkeypatch):
    """No tty = the autonomous loop's ship gate; it must not count as touch."""
    _tty(monkeypatch, False)
    state_dir = tmp_path / ".fno"

    _merge._emit_human_touch_merge(42, str(state_dir))

    assert _events(state_dir / "events.jsonl") == []


def test_manual_merge_unresolved_node_counts_as_failed(tmp_path, monkeypatch):
    """No matching node -> resolution=failed, never dropped (AC4-FR shape)."""
    graph = _fake_graph(tmp_path, [{"id": "x-9999", "title": "t", "pr_number": 7}])
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    _tty(monkeypatch, True)
    state_dir = tmp_path / ".fno"

    _merge._emit_human_touch_merge(42, str(state_dir))

    rows = _events(state_dir / "events.jsonl")
    assert len(rows) == 1
    assert rows[0]["data"]["graph_node_id"] is None
    assert rows[0]["data"]["resolution"] == "failed"


def test_manual_merge_matches_additional_prs(tmp_path, monkeypatch):
    """A follow-up PR recorded in additional_prs still attributes the node."""
    graph = _fake_graph(
        tmp_path,
        [{"id": "x-5678", "title": "t", "additional_prs": [{"number": 42}]}],
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    _tty(monkeypatch, True)
    state_dir = tmp_path / ".fno"

    _merge._emit_human_touch_merge(42, str(state_dir))

    rows = _events(state_dir / "events.jsonl")
    assert rows[0]["data"]["graph_node_id"] == "x-5678"


def _record(tmp_path: Path) -> MergeDriftRecord:
    return MergeDriftRecord(
        node_id="x-abcd",
        plan_path=None,
        pr_number=42,
        pr_url="https://github.com/o/r/pull/42",
        pr_state="MERGED",
        merged_at="2026-07-04T00:00:00Z",
        cwd=str(tmp_path),
    )


def test_reconcile_close_emits_human_touch(tmp_path):
    """AC4-EDGE: reconcile closing an out-of-band merge emits once, ok."""
    out = emit_human_touch_for_record(_record(tmp_path))

    assert out == tmp_path / ".fno" / "events.jsonl"
    rows = _events(out)
    assert len(rows) == 1
    assert rows[0]["type"] == "human_touch"
    assert rows[0]["source"] == "backlog"
    assert rows[0]["data"] == {
        "graph_node_id": "x-abcd",
        "source": "merge",
        "resolution": "ok",
    }


def test_reconcile_emit_failure_is_nonfatal(tmp_path, capsys):
    """A broken cwd never raises out of the close loop (best-effort)."""
    rec = _record(tmp_path)
    # A file where the .fno DIRECTORY should be forces mkdir to fail.
    (tmp_path / ".fno").write_text("not a dir")

    out = emit_human_touch_for_record(rec)

    assert out is None
    assert "human_touch emit failed" in capsys.readouterr().err
