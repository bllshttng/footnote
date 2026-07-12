"""Tests for the status-sink fanout (x-2057).

Layer 2 of the status-breakpoints protocol: a dumb, config-driven dispatcher
that sweeps ``.fno/events.jsonl`` and routes x-dbaf protocol-family events to
external sinks. This file covers all ACs across the six user stories; the
``-k`` filters in the plan's per-task verify lines select the relevant subset.
"""
from __future__ import annotations

import pytest

from fno.config import ConfigBlock, StatusFanoutConfig, StatusSinkConfig


# ── shared fixtures/helpers for tick tests ──────────────────────────────────


def _write_events(root, lines: list[dict]) -> None:
    fno_dir = root / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    import json as _json

    with (fno_dir / "events.jsonl").open("w", encoding="utf-8") as fh:
        for obj in lines:
            fh.write(_json.dumps(obj) + "\n")


def _ev(ts: str, type_: str, **envelope) -> dict:
    e = {"ts": ts, "v": 1, "type": type_, "source": "target", "data": {}}
    e.update(envelope)
    return e


class _Recorder:
    """A recording dispatch_fn; classifies by a per-name script or delivers."""

    def __init__(self, script=None):
        self.calls: list[tuple[str, str]] = []  # (sink_name, event_ts)
        self._script = script or {}

    def __call__(self, sink, event):
        self.calls.append((sink.name, event["ts"]))
        from fno import status_fanout as sf

        return self._script.get(sink.name, (sf.DELIVERED, ""))


# ── US1: config model ───────────────────────────────────────────────────────


def test_config_fanout_defaults() -> None:
    f = StatusFanoutConfig()
    assert f.interval_secs == 5
    assert f.http_timeout_secs == 5
    assert f.retries == 2


def test_config_sink_minimal_text_webhook_valid() -> None:
    s = StatusSinkConfig(
        name="ops-discord",
        type="text-webhook",
        events=["blocked"],
        url="https://discord.com/api/webhooks/x",
        template="{from} blocked on {node}",
        field="content",
    )
    assert s.name == "ops-discord"
    assert s.enabled is True  # default on


def test_config_empty_sinks_is_default_noop() -> None:
    assert ConfigBlock().status_sinks == []


def test_config_status_sinks_nonlist_coerces_empty() -> None:
    # A container-level typo (a scalar where a list belongs) fails safe to [],
    # never bricks settings load for the whole project.
    assert ConfigBlock(status_sinks=42).status_sinks == []


def test_config_duplicate_sink_name_rejected() -> None:
    with pytest.raises(ValueError, match="[Dd]uplicate"):
        ConfigBlock(
            status_sinks=[
                {"name": "dup", "type": "backlog-progress"},
                {"name": "dup", "type": "backlog-progress"},
            ]
        )


def test_config_bad_match_key_rejected_names_allowed_keys() -> None:
    # AC1-UI: a match key outside the envelope whitelist fails validation with a
    # message naming the allowed keys.
    with pytest.raises(ValueError) as ei:
        StatusSinkConfig(
            name="s",
            type="backlog-progress",
            match={"projct": "fno"},  # typo
        )
    msg = str(ei.value)
    assert "projct" in msg
    assert "project" in msg  # the allowed-keys list is surfaced


def test_config_valid_match_keys_pass() -> None:
    s = StatusSinkConfig(
        name="s",
        type="backlog-progress",
        match={"project": "fno", "outcome": "FAILED"},
    )
    assert s.match == {"project": "fno", "outcome": "FAILED"}


def test_config_match_on_data_rejected() -> None:
    # `data` is an envelope key but is a nested object, not an equality target.
    with pytest.raises(ValueError):
        StatusSinkConfig(name="s", type="backlog-progress", match={"data": "x"})


def test_config_webhook_requires_exactly_one_of_url_url_env_both() -> None:
    with pytest.raises(ValueError, match="url"):
        StatusSinkConfig(
            name="s",
            type="json-webhook",
            url="https://x",
            url_env="OPS_URL",
        )


def test_config_webhook_requires_exactly_one_of_url_url_env_neither() -> None:
    with pytest.raises(ValueError, match="url"):
        StatusSinkConfig(name="s", type="json-webhook")


def test_config_url_env_alone_valid() -> None:
    s = StatusSinkConfig(name="s", type="json-webhook", url_env="OPS_URL")
    assert s.url_env == "OPS_URL"
    assert s.url is None


def test_config_backlog_progress_needs_no_url() -> None:
    # backlog-progress is a local write; neither url nor url_env applies.
    s = StatusSinkConfig(name="s", type="backlog-progress")
    assert s.url is None and s.url_env is None


def test_config_unknown_type_rejected() -> None:
    with pytest.raises(ValueError, match="type"):
        StatusSinkConfig(name="s", type="carrier-pigeon", url="https://x")


# ── US2: tick core (cursor / rotation / isolation / dry-run) ─────────────────


def _text_sink(name="s", events=("blocked",), **kw):
    return StatusSinkConfig(
        name=name, type="text-webhook", events=list(events),
        url="https://x", template="{data.reason}", **kw,
    )


def test_tick_empty_sinks_is_clean_noop(tmp_path):
    from fno import status_fanout as sf

    _write_events(tmp_path, [_ev("2026-07-12T00:00:00Z", "blocked")])
    res = sf.run_tick(tmp_path, [])
    assert res.sinks == []
    # No cursor dir created for a no-op.
    assert not (tmp_path / ".fno" / "status-sinks").exists()


def test_tick_fresh_cursor_starts_at_eof_no_backfill(tmp_path):
    from fno import status_fanout as sf

    _write_events(tmp_path, [
        _ev("2026-07-12T00:00:01Z", "blocked", **{"node": "x-1"}),
        _ev("2026-07-12T00:00:02Z", "blocked", **{"node": "x-2"}),
    ])
    rec = _Recorder()
    res = sf.run_tick(tmp_path, [_text_sink()], dispatch_fn=rec)
    # Fresh sink initializes at EOF -> nothing historical replayed.
    assert rec.calls == []
    assert res.sinks[0].dispatched == 0
    # Cursor persisted at EOF ts so the next tick has a floor.
    cur = (tmp_path / ".fno" / "status-sinks" / "s.cursor").read_text().strip()
    assert cur == "2026-07-12T00:00:02Z"


def test_tick_matched_event_delivers_once_and_advances(tmp_path):
    from fno import status_fanout as sf

    # Pre-seed a cursor so the blocked event is "new".
    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "s.cursor").write_text("2026-07-12T00:00:00Z")
    _write_events(tmp_path, [_ev("2026-07-12T00:00:05Z", "blocked", **{"node": "x-9"})])

    rec = _Recorder()
    res = sf.run_tick(tmp_path, [_text_sink()], dispatch_fn=rec)
    assert rec.calls == [("s", "2026-07-12T00:00:05Z")]
    assert res.sinks[0].matched == 1 and res.sinks[0].dispatched == 1
    assert (ss / "s.cursor").read_text().strip() == "2026-07-12T00:00:05Z"


def test_tick_match_filter_and_events_filter(tmp_path):
    from fno import status_fanout as sf

    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "s.cursor").write_text("2026-07-12T00:00:00Z")
    _write_events(tmp_path, [
        _ev("2026-07-12T00:00:01Z", "task_started", **{"project": "fno"}),   # wrong kind
        _ev("2026-07-12T00:00:02Z", "blocked", **{"project": "other"}),      # wrong match
        _ev("2026-07-12T00:00:03Z", "blocked", **{"project": "fno"}),        # hit
    ])
    sink = _text_sink(events=("blocked",), match={"project": "fno"})
    rec = _Recorder()
    res = sf.run_tick(tmp_path, [sink], dispatch_fn=rec)
    assert [ts for _, ts in rec.calls] == ["2026-07-12T00:00:03Z"]
    assert res.sinks[0].matched == 1


def test_tick_dry_run_matches_but_sends_nothing(tmp_path):
    from fno import status_fanout as sf

    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "s.cursor").write_text("2026-07-12T00:00:00Z")
    _write_events(tmp_path, [_ev("2026-07-12T00:00:05Z", "blocked")])
    rec = _Recorder()
    res = sf.run_tick(tmp_path, [_text_sink()], dry_run=True, dispatch_fn=rec)
    assert rec.calls == []               # nothing sent
    assert res.sinks[0].matched == 1     # but counted
    assert (ss / "s.cursor").read_text().strip() == "2026-07-12T00:00:00Z"  # not advanced


def test_tick_malformed_line_skipped_and_counted(tmp_path):
    from fno import status_fanout as sf

    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(parents=True)
    ss = fno_dir / "status-sinks"
    ss.mkdir()
    (ss / "s.cursor").write_text("2026-07-12T00:00:00Z")
    with (fno_dir / "events.jsonl").open("w") as fh:
        fh.write("not json at all\n")
        import json as _json
        fh.write(_json.dumps(_ev("2026-07-12T00:00:05Z", "blocked")) + "\n")
    rec = _Recorder()
    res = sf.run_tick(tmp_path, [_text_sink()], dispatch_fn=rec)
    assert res.skipped_lines == 1
    assert res.sinks[0].dispatched == 1  # tick did not abort


def test_tick_rotation_drains_dot1_before_active(tmp_path):
    from fno import status_fanout as sf

    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(parents=True)
    ss = fno_dir / "status-sinks"
    ss.mkdir()
    (ss / "s.cursor").write_text("2026-07-12T00:00:00Z")
    import json as _json
    # Rotated history holds an older blocked event; active holds a newer one.
    with (fno_dir / "events.jsonl.1").open("w") as fh:
        fh.write(_json.dumps(_ev("2026-07-12T00:00:01Z", "blocked", **{"node": "old"})) + "\n")
    with (fno_dir / "events.jsonl").open("w") as fh:
        fh.write(_json.dumps(_ev("2026-07-12T00:00:09Z", "blocked", **{"node": "new"})) + "\n")
    rec = _Recorder()
    res = sf.run_tick(tmp_path, [_text_sink()], dispatch_fn=rec)
    # Both delivered, rotated-first order.
    assert [ts for _, ts in rec.calls] == [
        "2026-07-12T00:00:01Z", "2026-07-12T00:00:09Z"]
    assert res.sinks[0].dispatched == 2


def test_tick_short_circuit_holds_cursor_for_retry(tmp_path):
    from fno import status_fanout as sf

    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "s.cursor").write_text("2026-07-12T00:00:00Z")
    _write_events(tmp_path, [
        _ev("2026-07-12T00:00:01Z", "blocked", **{"node": "a"}),
        _ev("2026-07-12T00:00:02Z", "blocked", **{"node": "b"}),
        _ev("2026-07-12T00:00:03Z", "blocked", **{"node": "c"}),
    ])
    # First event short-circuits: remaining batch is not attempted, cursor holds.
    rec = _Recorder(script={"s": (sf.SHORT_CIRCUIT, "timeout")})
    res = sf.run_tick(tmp_path, [_text_sink()], dispatch_fn=rec)
    assert len(rec.calls) == 1                    # only the first attempted
    assert res.sinks[0].short_circuited is True
    # Cursor NOT advanced past the short-circuited event.
    assert (ss / "s.cursor").read_text().strip() == "2026-07-12T00:00:00Z"


def test_tick_per_sink_isolation_one_raises(tmp_path):
    from fno import status_fanout as sf

    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "bad.cursor").write_text("2026-07-12T00:00:00Z")
    (ss / "good.cursor").write_text("2026-07-12T00:00:00Z")
    _write_events(tmp_path, [_ev("2026-07-12T00:00:05Z", "blocked")])

    calls = []

    def dispatch(sink, event):
        calls.append(sink.name)
        if sink.name == "bad":
            raise RuntimeError("boom")
        return sf.DELIVERED, ""

    bad = _text_sink(name="bad")
    good = _text_sink(name="good")
    res = sf.run_tick(tmp_path, [bad, good], dispatch_fn=dispatch)
    names = {sr.name: sr for sr in res.sinks}
    assert names["good"].dispatched == 1          # good sink unaffected
    assert names["bad"].dropped == 1              # bad sink's raise counted as drop
    # bad's error logged.
    assert (ss / "bad.errors.jsonl").exists()


def test_tick_lock_blocks_overlapping_tick(tmp_path):
    from fno import status_fanout as sf

    _write_events(tmp_path, [_ev("2026-07-12T00:00:05Z", "blocked")])
    sink = _text_sink()
    lock = sf._TickLock(tmp_path)
    assert lock.acquire()
    try:
        res = sf.run_tick(tmp_path, [sink], dispatch_fn=_Recorder())
        assert res.locked_out is True
    finally:
        lock.release()


# ── US3: json-webhook adapter (failure classes) ─────────────────────────────


def _json_sink(name="j", cloudevents=False, url="https://x", url_env=None):
    return StatusSinkConfig(
        name=name, type="json-webhook", events=["run_summary"],
        url=url, url_env=url_env, cloudevents=cloudevents,
    )


def test_json_webhook_delivers_verbatim(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    posted = {}

    def fake_post(url, body, timeout):
        posted["url"] = url
        posted["body"] = body
        return sf._HttpResult(ok=True, status=200)

    monkeypatch.setattr(sf, "_post_json", fake_post)
    ev = _ev("2026-07-12T00:00:05Z", "run_summary", **{"node": "x-9", "outcome": "SUCCESS"})
    status, _ = sf._dispatch_json_webhook(_json_sink(), ev, StatusFanoutConfig())
    assert status == sf.DELIVERED
    assert posted["url"] == "https://x"
    assert posted["body"] == ev  # verbatim, unwrapped


def test_json_webhook_cloudevents_wrap(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    posted = {}
    monkeypatch.setattr(sf, "_post_json",
                        lambda u, b, t: (posted.update(body=b) or sf._HttpResult(ok=True, status=200)))
    ev = _ev("2026-07-12T00:00:05Z", "run_summary", **{"run": "R1"})
    sf._dispatch_json_webhook(_json_sink(cloudevents=True), ev, StatusFanoutConfig())
    body = posted["body"]
    assert set(body) == {"id", "source", "type", "time", "data"}
    assert body["type"] == "run_summary" and body["time"] == "2026-07-12T00:00:05Z"
    assert body["data"] == ev
    assert body["id"] == "R1:2026-07-12T00:00:05Z:run_summary"


def test_json_webhook_4xx_drops_immediately_no_retry(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    calls = {"n": 0}

    def fake_post(url, body, timeout):
        calls["n"] += 1
        return sf._HttpResult(ok=False, status=404)

    monkeypatch.setattr(sf, "_post_json", fake_post)
    monkeypatch.setattr(sf, "_sleep", lambda s: None)
    status, detail = sf._dispatch_json_webhook(
        _json_sink(), _ev("2026-07-12T00:00:05Z", "run_summary"), StatusFanoutConfig(retries=2))
    assert status == sf.DROPPED
    assert calls["n"] == 1  # permanent: no retry
    assert "404" in detail


def test_json_webhook_connect_class_retries_then_short_circuits(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    calls = {"n": 0}

    def fake_post(url, body, timeout):
        calls["n"] += 1
        return sf._HttpResult(ok=False, status=None)  # connect-class

    monkeypatch.setattr(sf, "_post_json", fake_post)
    monkeypatch.setattr(sf, "_sleep", lambda s: None)
    status, detail = sf._dispatch_json_webhook(
        _json_sink(), _ev("2026-07-12T00:00:05Z", "run_summary"), StatusFanoutConfig(retries=2))
    assert status == sf.SHORT_CIRCUIT
    assert calls["n"] == 3  # 1 + 2 retries
    assert "connect-class" in detail


def test_json_webhook_5xx_retries_then_short_circuits(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    monkeypatch.setattr(sf, "_post_json", lambda u, b, t: sf._HttpResult(ok=False, status=503))
    monkeypatch.setattr(sf, "_sleep", lambda s: None)
    status, _ = sf._dispatch_json_webhook(
        _json_sink(), _ev("2026-07-12T00:00:05Z", "run_summary"), StatusFanoutConfig(retries=1))
    assert status == sf.SHORT_CIRCUIT


def test_json_webhook_429_honors_retry_after(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    slept = []
    seq = [sf._HttpResult(ok=False, status=429, retry_after=2.0),
           sf._HttpResult(ok=True, status=200)]
    monkeypatch.setattr(sf, "_post_json", lambda u, b, t: seq.pop(0))
    monkeypatch.setattr(sf, "_sleep", lambda s: slept.append(s))
    status, _ = sf._dispatch_json_webhook(
        _json_sink(), _ev("2026-07-12T00:00:05Z", "run_summary"), StatusFanoutConfig(retries=2))
    assert status == sf.DELIVERED
    assert slept == [2.0]  # honored Retry-After, not the backoff schedule


def test_json_webhook_missing_url_env_short_circuits(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    monkeypatch.delenv("OPS_MISSING", raising=False)
    sink = _json_sink(url=None, url_env="OPS_MISSING")
    status, detail = sf._dispatch_json_webhook(
        sink, _ev("2026-07-12T00:00:05Z", "run_summary"), StatusFanoutConfig())
    assert status == sf.SHORT_CIRCUIT
    assert "OPS_MISSING" in detail


def test_json_webhook_url_env_resolves(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    monkeypatch.setenv("OPS_URL", "https://from-env")
    posted = {}
    monkeypatch.setattr(sf, "_post_json",
                        lambda u, b, t: (posted.update(url=u) or sf._HttpResult(ok=True, status=200)))
    sink = _json_sink(url=None, url_env="OPS_URL")
    sf._dispatch_json_webhook(sink, _ev("2026-07-12T00:00:05Z", "run_summary"), StatusFanoutConfig())
    assert posted["url"] == "https://from-env"


# ── US4: text-webhook adapter (custom Formatter, mentions guard) ─────────────


def test_render_dotted_data_path():
    from fno import status_fanout as sf

    ev = _ev("t", "blocked", **{"from": "worker", "node": "x-9"})
    ev["data"] = {"reason": "needs a decision"}
    out = sf._render_template("{from} blocked on {node}: {data.reason}", ev)
    assert out == "worker blocked on x-9: needs a decision"


def test_render_missing_field_is_empty_not_crash():
    from fno import status_fanout as sf

    ev = _ev("t", "blocked")  # no 'from', no data.reason
    out = sf._render_template("[{from}] {data.reason}{node}", ev)
    assert out == "[] "  # every missing ref renders empty, no KeyError/AttributeError


def test_render_dotted_on_nondict_is_empty():
    from fno import status_fanout as sf

    ev = _ev("t", "blocked", **{"from": "w"})
    # `from` is a string; `.foo` traversal must yield empty, not crash.
    assert sf._render_template("{from.foo}", ev) == ""


def test_text_webhook_discord_shaped_adds_allowed_mentions(monkeypatch):
    from fno import status_fanout as sf

    posted = {}
    monkeypatch.setattr(sf, "_post_json",
                        lambda u, b, t: (posted.update(body=b) or sf._HttpResult(ok=True, status=200)))
    ev = _ev("t", "blocked", **{"node": "x"})
    ev["data"] = {"reason": "@everyone ship it"}
    sink = StatusSinkConfig(name="d", type="text-webhook", events=["blocked"],
                            url="https://discord", template="{data.reason}", field="content")
    status, _ = sf._dispatch_text_webhook(sink, ev, StatusFanoutConfig())
    assert status == sf.DELIVERED
    assert posted["body"]["content"] == "@everyone ship it"
    assert posted["body"]["allowed_mentions"] == {"parse": []}  # no server ping


def test_text_webhook_slack_field_no_allowed_mentions(monkeypatch):
    from fno import status_fanout as sf

    posted = {}
    monkeypatch.setattr(sf, "_post_json",
                        lambda u, b, t: (posted.update(body=b) or sf._HttpResult(ok=True, status=200)))
    sink = StatusSinkConfig(name="s", type="text-webhook", events=["blocked"],
                            url="https://slack", template="hi", field="text")
    sf._dispatch_text_webhook(sink, _ev("t", "blocked"), StatusFanoutConfig())
    assert posted["body"] == {"text": "hi"}  # no allowed_mentions for non-Discord


def test_text_webhook_reuses_failure_classes(monkeypatch):
    from fno import status_fanout as sf

    monkeypatch.setattr(sf, "_post_json", lambda u, b, t: sf._HttpResult(ok=False, status=404))
    monkeypatch.setattr(sf, "_sleep", lambda s: None)
    sink = StatusSinkConfig(name="s", type="text-webhook", events=["blocked"],
                            url="https://x", template="hi", field="content")
    status, detail = sf._dispatch_text_webhook(sink, _ev("t", "blocked"), StatusFanoutConfig())
    assert status == sf.DROPPED and "404" in detail


# ── integration: run_tick through the real adapter router (mocked HTTP) ──────


def test_integration_tick_text_webhook_delivers_and_advances(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    posts = []
    monkeypatch.setattr(sf, "_post_json",
                        lambda u, b, t: (posts.append(b) or sf._HttpResult(ok=True, status=200)))
    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "d.cursor").write_text("2026-07-12T00:00:00Z")
    ev = _ev("2026-07-12T00:00:05Z", "blocked", **{"node": "x-9"})
    ev["data"] = {"reason": "why"}
    _write_events(tmp_path, [ev])
    sink = StatusSinkConfig(name="d", type="text-webhook", events=["blocked"],
                            url="https://x", template="{node}: {data.reason}", field="content")
    res = sf.run_tick(tmp_path, [sink])  # default dispatch_fn -> real router
    assert len(posts) == 1 and posts[0]["content"] == "x-9: why"
    assert res.sinks[0].dispatched == 1
    assert (ss / "d.cursor").read_text().strip() == "2026-07-12T00:00:05Z"


def test_integration_tick_connect_class_short_circuit_holds_batch(tmp_path, monkeypatch):
    # AC1-ERR end-to-end: 3 pending events, connect-class. First exhausts retries
    # and short-circuits; remaining 2 are unattempted; cursor does not advance.
    from fno import status_fanout as sf

    monkeypatch.setattr(sf, "_post_json", lambda u, b, t: sf._HttpResult(ok=False, status=None))
    monkeypatch.setattr(sf, "_sleep", lambda s: None)
    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "d.cursor").write_text("2026-07-12T00:00:00Z")
    _write_events(tmp_path, [
        _ev("2026-07-12T00:00:01Z", "blocked"),
        _ev("2026-07-12T00:00:02Z", "blocked"),
        _ev("2026-07-12T00:00:03Z", "blocked"),
    ])
    sink = StatusSinkConfig(name="d", type="text-webhook", events=["blocked"],
                            url="https://x", template="hi", field="content")
    res = sf.run_tick(tmp_path, [sink], StatusFanoutConfig(retries=1))
    assert res.sinks[0].short_circuited is True
    assert res.sinks[0].dispatched == 0
    assert (ss / "d.cursor").read_text().strip() == "2026-07-12T00:00:00Z"  # held
    assert (ss / "d.errors.jsonl").exists()


def test_integration_tick_permanent_4xx_drops_and_advances(tmp_path, monkeypatch):
    from fno import status_fanout as sf

    monkeypatch.setattr(sf, "_post_json", lambda u, b, t: sf._HttpResult(ok=False, status=404))
    monkeypatch.setattr(sf, "_sleep", lambda s: None)
    ss = tmp_path / ".fno" / "status-sinks"
    ss.mkdir(parents=True)
    (ss / "d.cursor").write_text("2026-07-12T00:00:00Z")
    _write_events(tmp_path, [_ev("2026-07-12T00:00:05Z", "blocked")])
    sink = StatusSinkConfig(name="d", type="text-webhook", events=["blocked"],
                            url="https://x", template="hi", field="content")
    res = sf.run_tick(tmp_path, [sink])
    assert res.sinks[0].dropped == 1
    assert (ss / "d.cursor").read_text().strip() == "2026-07-12T00:00:05Z"  # advanced past drop
