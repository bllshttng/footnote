"""The retirement verdict is the Rust GC's, and this module only maps it.

These tests pin the forwarding contract: bucket -> verdict mapping, the
NODE column's survival for retirable rows, and the fail-closed direction
when the Rust projection cannot be read.
"""

from __future__ import annotations

import json

import pytest

from fno.agents.retirement import Retirement, verdicts


def _summary() -> dict:
    """One sweep summary carrying a row in every mapped bucket."""
    return {
        "retired": [{"id": "w-done", "basis": "every named node done: x-70e1, x-2188"}],
        "kept_open_work": [{"id": "w-open", "node": "x-9", "status": "in_review"}],
        "kept_open_do_row": [{"id": "w-door", "node": "x-8"}],
        "kept_not_spawn": [{"id": "w-adopt", "reason": "adopted"}],
        "kept_operator": ["w-op"],
        "kept_crowned": ["w-crown"],
        "kept_no_provenance": ["w-lost"],
        "kept_active": [{"id": "w-live", "age_s": 10}],
        "kept_transcript_unresolved": [
            {"id": "w-dark", "held_s": 7200, "nodes_done": False},
            {"id": "w-dark-done", "held_s": 7 * 3600, "nodes_done": True},
        ],
        "stop_refused": [{"id": "w-stuck", "reason": "the stop did not confirm"}],
        "kept_no_receipt": [{"id": "w-norc", "reason": "no staged receipt"}],
    }


def _runner_with(summary: dict):
    return lambda: json.dumps(summary)


def test_retired_row_maps_to_true_with_node_and_basis():
    out = verdicts([("w-done", None)], runner=_runner_with(_summary()))
    v = out["w-done"]
    assert v.retire is True
    assert v.node == "x-70e1"
    assert v.node_basis == "graph"
    assert "x-70e1" in v.reason


def test_every_keep_bucket_maps_to_named_not_retirable():
    out = verdicts(
        [
            ("w-open", None),
            ("w-door", None),
            ("w-adopt", None),
            ("w-op", None),
            ("w-crown", None),
            ("w-lost", None),
            ("w-live", None),
            ("w-dark", None),
            ("w-dark-done", None),
            ("w-stuck", None),
            ("w-norc", None),
        ],
        runner=_runner_with(_summary()),
    )
    assert out["w-open"].retire is False
    assert out["w-open"].node == "x-9"
    assert out["w-open"].reason == "status=in_review"
    assert out["w-adopt"].reason == "not a spawn row: origin adopted"
    assert out["w-op"].reason == "operator row"
    assert out["w-lost"].reason == "no-node"
    assert out["w-live"].reason == "active: written 10s ago"
    # (x-1b90 change 3) The hold names its age; the decision rides only an
    # old hold on done work.
    assert out["w-dark"].reason == "transcript unresolved for 2h0m"
    assert (
        out["w-dark-done"].reason
        == "transcript unresolved for 7h0m; needs a decision: fno agents rm w-dark-done"
    )
    assert "stop refused" in out["w-stuck"].reason
    for name in ("w-door", "w-crown", "w-dark", "w-dark-done", "w-norc"):
        assert out[name].retire is False


def test_unreadable_projection_fails_closed_for_every_row():
    def boom():
        raise RuntimeError("binary missing")

    out = verdicts([("a", None), ("b", None)], runner=boom)
    for name in ("a", "b"):
        v = out[name]
        assert v.retire is False
        assert "rust-reap-unreadable" in v.reason


def test_row_outside_the_summary_is_never_retirable():
    out = verdicts([("ghost", None)], runner=_runner_with(_summary()))
    assert out["ghost"] == Retirement(None, None, False, "not in sweep summary")


def test_default_runner_shells_the_installed_binary(monkeypatch):
    # The wiring seam: _default_runner resolves the SAME binary the rest of
    # the fleet uses. A missing binary is the fail-closed path, not a crash.
    import fno.agents.retirement as retirement

    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary", lambda: None, raising=False
    )
    out = retirement.verdicts([("x", None)])
    assert out["x"].retire is False
    assert "rust-reap-unreadable" in out["x"].reason
