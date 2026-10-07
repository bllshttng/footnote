"""The route_resolve transport: payload shape, verbatim passthrough, and the
degrade-open stance. The band math, the grid fold, and the precedence chain
live in Rust (crates/fno-agents/src/route_slot.rs, route_gather.rs); the
characterization goldens in crates/fno-agents/tests/fixtures/route_gather/ pin
what the verb gathers. What stays here is the contract no Rust test can pin:
the one payload the transport sends and the answers it hands back verbatim.
"""
from __future__ import annotations

import pytest

from fno import route_resolve as rr


@pytest.fixture
def slot_answer(monkeypatch):
    """Stub fno.route_slot_client.route_slot_call; returns the calls list."""

    calls: list = []

    def install(answer=None, *, unavailable=None):
        def fake(payload, *a, **kw):
            calls.append(payload)
            if unavailable is not None:
                from fno.route_slot_client import RouteSlotUnavailable

                raise RouteSlotUnavailable(str(unavailable))
            return dict(answer or {})

        monkeypatch.setattr("fno.route_slot_client.route_slot_call", fake)
        return calls

    return install


def test_resolve_slot_sends_explicit_inputs_only(slot_answer):
    """The config-derived payload keys (declared rows, slot table, policy)
    are the verb's to gather: the transport sends the explicit inputs and
    nothing else."""
    calls = slot_answer({"candidate": {"name": "flash-x"}, "chain": [], "verdict": "armed"})
    node = {"difficulty": "high", "priority": "p1", "plan_path": "/p.md", "extra": "dropped"}
    meta: dict = {}
    rr.resolve_slot(
        "target", node, {"claude": "ok"},
        substrate="thread", permission_mode="yolo", constrain_harness="codex",
        role="review", model_occupied=True, explicit_lane=True,
        work_verb="pr", explicit_model_value="m1", explicit_route_value="r",
        explicit_vendor_value="v", meta=meta,
    )
    payload = calls[0]
    assert "meta" not in payload
    assert payload["rung_base"] == "agents.profiles.target"
    assert payload["node"] == {
        "difficulty": "high", "priority": "p1", "plan_path": "/p.md",
        "model": "", "provider": "", "effort": "",
    }
    assert payload["capacity"] == {"claude": "ok"}
    assert payload["substrate"] == "thread"
    assert payload["constrain_harness"] == "codex"
    assert payload["model_occupied"] is True
    assert payload["work_verb"] == "pr"
    assert "declared_rows" not in payload and "lanes" not in payload
    assert "settings" not in payload and "inventory" not in payload


def test_resolve_slot_returns_the_answer_verbatim(slot_answer):
    candidate = {"name": "flash-x", "harness": "claude", "model": "glm"}
    chain = ["slot=agents.profiles.target lanes[0] flash-x", "slot note kept"]
    meta: dict = {}
    slot_answer({
        "candidate": candidate, "chain": chain, "verdict": "armed",
        "refusal_terminal": {"class": "strict", "text": "no"},
        "exhausted_payload": {"reason": "slot_exhausted"},
        "fingerprint": "abc123",
    })
    out_candidate, out_chain, verdict = rr.resolve_slot("target", None, None, meta=meta)
    assert out_candidate == candidate
    assert out_chain == chain, "chain lines ride verbatim"
    assert verdict == "armed"
    assert meta["refusal"] == {"class": "strict", "text": "no"}
    assert meta["exhausted"] == {"reason": "slot_exhausted"}
    assert meta["fingerprint"] == "abc123"


def test_resolve_slot_unavailable_names_the_fault(slot_answer):
    slot_answer(unavailable="no binary")
    candidate, chain, verdict = rr.resolve_slot("target", None, None)
    assert candidate is None
    assert chain == ["slot=route-slot-unavailable (no binary)"]
    assert verdict == "unarmed"


def test_resolve_slot_unavailable_names_strict_routing(slot_answer, monkeypatch):
    """Under enforce_inventory the fault is a named strict-routing refusal,
    not a silent open spawn."""
    monkeypatch.setattr(
        rr, "_routing_enforced", lambda: True
    )
    meta: dict = {}
    slot_answer(unavailable="no binary")
    rr.resolve_slot("target", None, None, meta=meta)
    assert meta["refusal"]["text"].endswith("(strict routing: config routing.enforce_inventory)")


def test_resolve_inventory_degrades_to_empty(slot_answer):
    calls = slot_answer({
        "rows": [
            {"name": "flash-x", "harness": "claude", "model": "glm", "band": "low",
             "percentile": 12.5},
            {"name": "", "harness": "claude", "model": "no-name"},
            "not-a-row",
        ],
        "objective": "best-available", "prefer_harness": "codex", "declared": True,
    })
    inv = rr.resolve_inventory()
    assert set(inv.rows) == {"flash-x"}, "a nameless row is dropped"
    row = inv.rows["flash-x"]
    assert row.percentile == 12.5 and row.band == "low" and row.harness == "claude"
    assert inv.objective == "best-available" and inv.declared is True
    assert calls[0] == {"mode": "inventory"}


def test_dispatch_model_mode_payload(slot_answer):
    calls = slot_answer({
        "model": "resolved-x", "source": "task-difficulty(high)", "chain": ["model=grid"],
    })
    model, source, _chain = rr.resolve_dispatch_model(
        task_difficulty="high", provider="claude"
    )
    assert (model, source) == ("resolved-x", "task-difficulty(high)")
    assert calls[0] == {
        "mode": "dispatch_model", "explicit": None, "task_model": None,
        "task_difficulty": "high", "plan_model": None, "plan_difficulty": None,
        "provider": "claude",
    }


def test_dispatch_model_pins_answer_without_the_verb(slot_answer):
    """A pin is superuser authority: it answers in-process, no verb round
    trip - the old seam's contract, kept on the transport."""
    calls = slot_answer({"model": "grid-x", "source": "x", "chain": []})
    assert rr.resolve_dispatch_model(explicit="pin-e") == ("pin-e", "explicit", ["explicit"])
    assert rr.resolve_dispatch_model(task_model="pin-t") == ("pin-t", "task-pin", ["task-pin"])
    assert calls == [], "a pin never consults the verb"


def test_node_model_reads_the_resolver_and_degrades(monkeypatch):
    """The precedence chain (a node pin against explicit and difficulty) runs
    in the verb; node_model returns its answer verbatim, and a resolver error
    degrades to the pin or None, never blocks a dispatch."""
    seen = {}

    def fake(**kw):
        seen.update(kw)
        return ("resolved-x", "task-pin", [])

    monkeypatch.setattr(rr, "resolve_dispatch_model", fake)
    assert rr.node_model({"model": "glm-5.2"}) == "resolved-x"
    assert seen["task_model"] == "glm-5.2" and seen["provider"] == "claude"
    assert rr.node_model({"difficulty": "medium"}, provider="codex") == "resolved-x"
    assert seen["task_difficulty"] == "medium" and seen["provider"] == "codex"

    def _boom(**kw):
        raise RuntimeError("resolver died")

    monkeypatch.setattr(rr, "resolve_dispatch_model", _boom)
    assert rr.node_model({"model": "glm-5.2"}) == "glm-5.2"
    assert rr.node_model({}) is None


def test_slot_states_sends_the_states_mode_and_shapes_lanes(slot_answer):
    calls = slot_answer({
        "on_exhausted": "queue", "would_take": "slot=lanes[0]", "routing": "armed",
        "chain": ["slot note agents.profiles.target prefers strong bands"],
        "lane_states": [
            {"rung": "agents.profiles.target.lanes[0]", "name": "flash-x",
             "state": "ok", "identity": "zai-main", "source": "config"},
            {"rung": "l1", "name": "n2"},
        ],
    })
    out = rr.slot_states("target")
    assert calls[0]["mode"] == "states" and calls[0]["rung_base"] == "agents.profiles.target"
    assert out["on_exhausted"] == "queue" and out["routing"] == "armed"
    assert out["note"] == "prefers strong bands", "the note is the verb's vocabulary, verbatim"
    assert out["lanes"][0] == {
        "rung": "agents.profiles.target.lanes[0]", "name": "flash-x", "state": "ok",
        "identity": "zai-main", "source": "config",
    }
    assert out["lanes"][1]["state"] == "unknown", "an absent state reads unknown"
