"""The stale-row lane after the fleet-task port: the fold moved to
``crates/fno-agents/src/fleet_task.rs`` and ``reconcile_channel`` is ONE
transport call through ``verb_call("fleet-task", ...)``.

The fold's behavior (asked / duplicate / superseded / closed, the rows it
writes, the legacy retire) is characterized in Rust:
``crates/fno-agents/tests/fleet_task_reconcile_parity.rs`` and the
``fleet_task`` unit tests. What stays testable here is the shim contract:
the payload the lane sends and the outcome tuple it returns, plus one
end-to-end round trip against the built binary.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

_NOW = 1_800_000_000.0


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )


def _reconcile(root: Path, identities: "list[str]", *, empty: bool):
    from fno.agents.stale_escalate import reconcile_channel

    return reconcile_channel(
        [],
        root=root,
        session_id="watchdog-test",
        cwd=root,
        marker="watchdog-stale",
        subject="stale set",
        identities=identities,
        question=lambda key: f"q({key})",
        ask=lambda key: f"run({key})",
        asker=None,
    ) if empty else _reconcile_pairs(root, identities)


def _reconcile_pairs(root: Path, identities: "list[str]"):
    from fno.agents.stale_escalate import reconcile_channel

    return reconcile_channel(
        ["row"],
        root=root,
        session_id="watchdog-test",
        cwd=root,
        marker="watchdog-stale",
        subject="stale set",
        identities=identities,
        question=lambda key: f"q({key})",
        ask=lambda key: f"run({key})",
    )


def test_reconcile_channel_sends_the_fleet_task_payload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import fno.rust_binary

    captured: dict = {}

    def fake_verb_call(verb, payload, *args, **kwargs):
        captured["verb"] = verb
        captured["payload"] = payload
        return {"outcome": "asked", "id": "ft-1234abcd"}

    monkeypatch.setattr(fno.rust_binary, "verb_call", fake_verb_call)

    outcome, qid = _reconcile_pairs(tmp_path, ["i2", "i1"])

    assert outcome == "asked"
    assert qid == "ft-1234abcd"
    assert captured["verb"] == "fleet-task"
    payload = captured["payload"]
    assert payload["op"] == "reconcile"
    assert payload["lane"] == "watchdog-stale"
    from fno.agents.stale_escalate import dedupe_key

    key = dedupe_key(["i1", "i2"])
    assert payload["key"] == key
    assert payload["cwd"] == str(tmp_path)
    assert payload["text"] == f"q({key})"
    assert payload["run"] == f"run({key})"
    assert payload["empty"] is False


def test_reconcile_channel_marks_an_empty_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import fno.rust_binary

    captured: dict = {}

    def fake_verb_call(verb, payload, *args, **kwargs):
        captured["payload"] = payload
        return {"outcome": "closed"}

    monkeypatch.setattr(fno.rust_binary, "verb_call", fake_verb_call)

    outcome, qid = _reconcile(tmp_path, ["i1"], empty=True)

    assert outcome == "closed"
    assert qid == ""
    assert captured["payload"]["empty"] is True


def test_stale_lane_passes_the_verdicts_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The lane-level flow still runs classification for real; the fold is
    the transport, so its outcome rides through unchanged."""
    import fno.rust_binary
    from fno.agents import stale_lane as se
    from fno.agents import watchdog as wd

    def fake_verb_call(verb, payload, *args, **kwargs):
        assert verb == "fleet-task"
        assert payload["lane"] == "watchdog-stale"
        assert payload["empty"] is False
        return {"outcome": "asked", "id": "ft-lanebeef"}

    monkeypatch.setattr(fno.rust_binary, "verb_call", fake_verb_call)

    from fno.agents.watchdog import Row, TailFacts

    row = Row("dddd4444-0000", "k1", "stopped", None, "/tmp/k1")
    tail = TailFacts(
        [(_NOW - 61 * 1440, "stopped mid turn")],
        _NOW - 61 * 1440,
        "stopped mid turn",
        "assistant",
        "stopped mid turn",
    )
    payload, out_rows = wd.run_sweep(
        now_s=_NOW,
        rows_provider=lambda: ([row], []),
        transcript_fn=lambda sid: {row.row_id: tail}.get(sid),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    assert not payload.get("refused"), payload
    stale_pairs = [
        (wd.Verdict(**data), r)
        for data, r in zip(payload["verdicts"], out_rows)
        if data["verdict"] == wd.STALE
    ]
    outcome, qid = se.reconcile_stale(
        stale_pairs, root=tmp_path, session_id="watchdog-test", cwd=tmp_path
    )
    assert outcome == "asked"
    assert qid == "ft-lanebeef"


def test_reconcile_channel_end_to_end_against_the_binary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """One live round trip through the built fno-agents binary: the store
    gains one open fleet_task row, never an operator_question row; the same
    set again is a duplicate. Skips when no binary is built - the Rust
    characterization carries the behavior."""
    import fno.rust_binary

    try:
        if fno.rust_binary.find_dev_binary() is None and fno.rust_binary.resolve_binary() is None:
            pytest.skip("fno-agents binary not built")
    except Exception:  # noqa: BLE001 - resolution trouble reads as absent
        pytest.skip("fno-agents binary not resolvable")

    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))
    (tmp_path / "agents").mkdir()

    outcome, qid = _reconcile_pairs(tmp_path, ["i1"])
    assert outcome == "asked"
    assert qid.startswith("ft-")

    rows = [
        json.loads(line)
        for line in (tmp_path / "questions.jsonl").read_text().splitlines()
        if line.strip()
    ]
    kinds = [r["type"] for r in rows]
    assert "fleet_task" in kinds
    assert "operator_question" not in kinds

    outcome2, qid2 = _reconcile_pairs(tmp_path, ["i1"])
    assert outcome2 == "duplicate"
    assert qid2 == qid
