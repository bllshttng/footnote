"""Wave-4-trigger metric: compute_metrics verdict cases.

compute_metrics is pure over synthetic event dicts (no journal, no gh),
mirroring how should_close is tested.
"""
import json

from fno.backlog.batch import compute_metrics, read_batch_events


def _ship_event(*results: dict) -> dict:
    """A journaled active_backlog_batch_ship envelope (data.stdout is nested JSON)."""
    return {
        "ts": "2026-07-01T00:00:00Z",
        "type": "active_backlog_batch_ship",
        "source": "loop",
        "data": {"stdout": json.dumps({"shipped": list(results)})},
    }


def _shipped(domain: str, members: list[str]) -> dict:
    return {"action": "shipped", "domain": domain, "members": members}


def _abandon_event(domain: str, member_count: int, *, detail: str = "ok") -> dict:
    return {
        "ts": "2026-07-01T00:00:00Z",
        "type": "active_backlog_batch_abandon",
        "source": "loop",
        "data": {
            "domain": domain,
            "detail": detail,
            "member_count": member_count,
            "members": [f"ab-m{i}" for i in range(member_count)],
        },
    }


def test_keep_v1_when_shipping_cleanly():
    # AC (happy): 2 shipped 3-member batches, 0 abandons.
    events = [
        _ship_event(_shipped("code", ["a", "b", "c"])),
        _ship_event(_shipped("code", ["d", "e", "f"])),
    ]
    m = compute_metrics(events)
    t = m["totals"]
    assert t["runs_saved"] == 4
    assert t["runs_wasted"] == 0
    assert t["net"] == 4
    assert m["verdict"] == "keep-v1"
    assert m["domains"]["code"]["verdict"] == "keep-v1"


def test_build_wave4_when_waste_exceeds_savings():
    # AC (build-wave4): 1 shipped 3-member batch (saved 2), 1 abandoned
    # 3-member batch (wasted 3) -> net -1.
    events = [
        _ship_event(_shipped("code", ["a", "b", "c"])),
        _abandon_event("code", 3),
    ]
    m = compute_metrics(events)
    t = m["totals"]
    assert t["runs_saved"] == 2
    assert t["runs_wasted"] == 3
    assert t["net"] == -1
    assert m["verdict"] == "build-wave4"


def test_no_data_when_no_batch_events():
    # AC (edge/empty): never divides by zero.
    m = compute_metrics([])
    assert m["verdict"] == "no-data"
    assert m["domains"] == {}
    assert m["totals"]["abandon_rate"] == 0.0


def test_disable_batching_when_persistently_negative_and_high_abandon_rate():
    events = [
        _ship_event(_shipped("code", ["a", "b"])),  # saved 1
        _abandon_event("code", 2),
        _abandon_event("code", 2),
        _abandon_event("code", 2),  # wasted 6, abandon_rate 3/4
    ]
    m = compute_metrics(events)
    assert m["totals"]["net"] == -5
    assert m["verdict"] == "disable-batching"


def test_ship_failure_abandon_counts_all_members_as_wasted():
    # A PR-open failure abandons via ship-closeable: action=abandoned in the
    # nested stdout, all members clean -> all wasted.
    events = [
        _ship_event(
            {"action": "abandoned", "domain": "code", "members": ["a", "b", "c"]}
        )
    ]
    m = compute_metrics(events)
    t = m["totals"]
    assert t["batches_abandoned"] == 1
    assert t["runs_wasted"] == 3


def test_failed_abandon_call_and_junk_are_ignored():
    # detail != ok means the abandon CLI failed (nothing was abandoned);
    # a solo 1-member ship earns 0 saved runs but stays keep-v1 (no waste).
    events = [
        _abandon_event("code", 3, detail="no open batch for domain 'code'"),
        _ship_event(_shipped("code", ["only"])),
        {"ts": "2026-07-01T00:00:00Z", "type": "active_backlog_batch_ship",
         "source": "loop", "data": {"stdout": "not json"}},
        {"ts": "2026-07-01T00:00:00Z", "type": "active_backlog_batch_ship",
         "source": "loop", "data": {"error": "gh unavailable"}},
    ]
    m = compute_metrics(events)
    t = m["totals"]
    assert t["batches_abandoned"] == 0
    assert t["runs_wasted"] == 0
    assert t["runs_saved"] == 0
    assert m["verdict"] == "keep-v1"


def test_read_batch_events_filters_kind_since_and_junk(tmp_path):
    p = tmp_path / "events.jsonl"
    lines = [
        json.dumps(_abandon_event("code", 1)),  # ts 2026-07-01, in window
        json.dumps({**_abandon_event("code", 1), "ts": "2026-06-01T00:00:00Z"}),
        json.dumps({"ts": "2026-07-01T00:00:00Z", "type": "loop_check", "data": {}}),
        "{not json",
        "",
    ]
    p.write_text("\n".join(lines) + "\n", encoding="utf-8")
    evs = read_batch_events(p, since="2026-06-15T00:00:00Z")
    assert len(evs) == 1
    assert evs[0]["data"]["member_count"] == 1
    assert read_batch_events(tmp_path / "missing.jsonl") == []
