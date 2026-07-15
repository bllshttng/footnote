"""Decision-matrix tests for `fno backlog advance` (merge-triggered auto-continue).

Node ab-3cd195b6. Covers every branch of advance()'s decision matrix plus the
load-bearing invariants:

- AC1-UI / LD#12: EXACTLY ONE decision event per run.
- AC1-CLAIM: the dispatch reservation is a TTL claim that stays LIVE after
  advance returns (a concurrent reconcile sees already-claimed; the worker is
  never duplicated).
- AC1-FR: the same merge observed twice dispatches once.
- AC1-ERR / AC2-FR: a spawn failure releases the reservation (node stays
  re-dispatchable) and never raises.
- AC2-HP: disabled by default dispatches nothing.
- AC2-EDGE: a live walk suppresses advance.

Claim isolation: every test routes claims under a tmp FNO_CLAIMS_ROOT +
FNO_REPO_ROOT and uses advance's OWN key/root helpers so the test writes claims
exactly where advance reads them.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from typer.testing import CliRunner

from fno.backlog import advance as adv
from fno.claims.core import acquire_claim, claim_status
from fno.cli import app

runner = CliRunner()


class _FakeProc:
    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


_RECEIPT = '{"name":"tgt-2222aaaa","short_id":"abc12345","provider":"claude","status":"live"}\n'


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Isolate all claims + canonical-root resolution under tmp_path."""
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")  # armed by default for tests
    events_path = tmp_path / ".fno" / "events.jsonl"
    return events_path


def _events(events_path: Path) -> list[dict]:
    if not events_path.exists():
        return []
    return [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]


def _hold(key: str) -> None:
    """Acquire a live TTL claim at KEY using advance's own root routing."""
    acquire_claim(key, "test-holder", ttl_ms=60_000, root=adv._claims_root_for(key))


NODE = {"id": "ab-2222aaaa", "title": "next", "project": "fno", "_resolved_cwd": "/tmp/x"}


# ---------------------------------------------------------------------------
# Decision matrix
# ---------------------------------------------------------------------------


def test_disabled_dispatches_nothing(iso, monkeypatch):
    """AC2-HP: disabled -> advance_skipped{disabled}, no spawn."""
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "0")
    spawned = []
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: spawned.append(a) or "x")
    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)

    res = adv.advance(closed_node_id="ab-1111aaaa", project="fno", events_path=iso)

    assert res.decision == "skipped" and res.reason == "disabled"
    assert spawned == []
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "advance_skipped"
    assert evs[0]["data"]["reason"] == "disabled"


def test_no_work(iso, monkeypatch):
    monkeypatch.setattr(adv, "_next_node", lambda project: None)
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    res = adv.advance(project="fno", events_path=iso)

    assert res.decision == "skipped" and res.reason == "no-work"
    assert len(_events(iso)) == 1


def test_next_error_skips_never_guesses(iso, monkeypatch):
    def boom(project):
        raise RuntimeError("gh exploded")

    monkeypatch.setattr(adv, "_next_node", boom)
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    res = adv.advance(project="fno", events_path=iso)

    assert res.decision == "skipped" and res.reason == "next-error"


def test_walker_live_suppresses(iso, monkeypatch):
    """AC2-EDGE: a live walk owns the project -> skip."""
    _hold(adv._walker_key())
    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    res = adv.advance(project="fno", events_path=iso)

    assert res.decision == "skipped" and res.reason == "walker-live"


def test_node_already_claimed(iso, monkeypatch):
    """A live node:<id> claim means a worker is already running -> skip."""
    _hold(f"node:{NODE['id']}")
    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    res = adv.advance(project="fno", events_path=iso)

    assert res.decision == "skipped" and res.reason == "already-claimed"


def test_dispatch_reservation_held(iso, monkeypatch):
    """A peer's live dispatch:<id> reservation -> already-claimed, no spawn."""
    _hold(f"dispatch:{NODE['id']}")
    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    res = adv.advance(project="fno", events_path=iso)

    assert res.decision == "skipped" and res.reason == "already-claimed"


def test_dispatched_happy_path_and_claim_survives(iso, monkeypatch):
    """AC1-HP + AC1-CLAIM: dispatch + reservation stays LIVE after advance returns."""
    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", lambda node_id, node_cwd, node_slug=None, model=None, provider=None: "deadbeef")

    res = adv.advance(closed_node_id="ab-1111aaaa", project="fno", events_path=iso)

    assert res.decision == "dispatched"
    assert res.node_id == NODE["id"] and res.short_id == "deadbeef"
    # AC1-CLAIM: the dispatch reservation is live AFTER advance returns.
    key = f"dispatch:{NODE['id']}"
    assert claim_status(key, root=adv._claims_root_for(key)).get("state") == "live"
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "advance_dispatched"
    assert evs[0]["data"]["node_id"] == NODE["id"]
    assert evs[0]["data"]["short_id"] == "deadbeef"


def test_idempotent_same_merge_twice(iso, monkeypatch):
    """AC1-FR: a second advance for the same node does not double-dispatch."""
    calls = []
    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", lambda node_id, node_cwd, node_slug=None, model=None, provider=None: calls.append(node_id) or "sid")

    first = adv.advance(project="fno", events_path=iso)
    second = adv.advance(project="fno", events_path=iso)

    assert first.decision == "dispatched"
    assert second.decision == "skipped" and second.reason == "already-claimed"
    assert calls == [NODE["id"]]  # spawned exactly once


def test_spawn_failure_releases_reservation(iso, monkeypatch):
    """AC1-ERR / AC2-FR: a spawn failure releases dispatch:<id> + emits failed."""
    def boom(node_id, node_cwd, node_slug=None, model=None, provider=None):
        raise adv.SpawnError("daemon unreachable")

    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", boom)

    res = adv.advance(project="fno", events_path=iso)

    assert res.decision == "failed" and res.node_id == NODE["id"]
    # Reservation released -> node is re-dispatchable on the next trigger.
    key = f"dispatch:{NODE['id']}"
    assert claim_status(key, root=adv._claims_root_for(key)).get("state") == "free"
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "advance_failed"


def test_spawn_already_running_releases_and_skips(iso, monkeypatch):
    """A name-collision (peer beat us) -> already-claimed, reservation released."""
    def collide(node_id, node_cwd, node_slug=None, model=None, provider=None):
        raise adv.SpawnAlreadyRunning("tgt-... already exists")

    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", collide)

    res = adv.advance(project="fno", events_path=iso)

    assert res.decision == "skipped" and res.reason == "already-claimed"
    key = f"dispatch:{NODE['id']}"
    assert claim_status(key, root=adv._claims_root_for(key)).get("state") == "free"


def test_failed_then_retry_dispatches(iso, monkeypatch):
    """AC2-FR (explicit): after a spawn failure releases the reservation, a
    second advance actually re-dispatches (the chain self-heals, not just
    'the reservation is free')."""
    n = {"calls": 0}

    def spawn(node_id, node_cwd, node_slug=None, model=None, provider=None):
        n["calls"] += 1
        if n["calls"] == 1:
            raise adv.SpawnError("transient daemon blip")
        return "sid2"

    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)
    monkeypatch.setattr(adv, "_spawn_worker", spawn)

    first = adv.advance(project="fno", events_path=iso)
    second = adv.advance(project="fno", events_path=iso)

    assert first.decision == "failed"
    assert second.decision == "dispatched" and second.short_id == "sid2"


def test_advance_cli_empty_model_exits_2(monkeypatch):
    """AC2-ERR at the advance verb: an empty --model is a usage error, no dispatch."""
    monkeypatch.setattr(
        adv, "advance", lambda **k: pytest.fail("must not dispatch on an empty --model")
    )
    r = runner.invoke(app, ["backlog", "advance", "--model", "  "])
    assert r.exit_code == 2
    assert "must not be empty" in r.output


def test_advance_threads_node_pins_to_spawn(iso, monkeypatch):
    """A node's own model/provider annotation reaches the dispatched worker."""
    captured = {}

    def spawn(node_id, node_cwd, node_slug=None, model=None, provider=None):
        captured.update(model=model, provider=provider)
        return "sid"

    node = {**NODE, "model": "glm-4.7", "provider": "codex"}
    monkeypatch.setattr(adv, "_next_node", lambda project: node)
    monkeypatch.setattr(adv, "_spawn_worker", spawn)
    res = adv.advance(project="fno", events_path=iso)
    assert res.decision == "dispatched"
    assert captured == {"model": "glm-4.7", "provider": "codex"}


def test_direct_dependents_carry_model_tier(monkeypatch):
    """Codex P2: the reduced dependent dict must carry model_tier so the tier
    resolver sees it on the cross-project/dependent dispatch path."""
    graph = [
        {"id": "ab-closed11", "project": "fno"},
        {"id": "ab-dep00001", "project": "fno", "blocked_by": ["ab-closed11"],
         "_status": "ready", "model_tier": "high", "cwd": "/w"},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda p: graph)
    deps = adv._direct_dependents("ab-closed11", "fno")
    assert deps and deps[0]["id"] == "ab-dep00001"
    assert deps[0]["model_tier"] == "high"


def test_advance_resolves_node_tier_to_model(iso, monkeypatch):
    """AC3-HP wiring: a node's model_tier resolves to a concrete --model at the
    advance spawn (no snapshot -> the deterministic static table)."""
    captured = {}

    def spawn(node_id, node_cwd, node_slug=None, model=None, provider=None):
        captured.update(model=model)
        return "sid"

    # Force the static table so the resolution is deterministic in CI.
    from fno.adapters.providers import benchmarks as _bm
    monkeypatch.setattr(_bm, "load_snapshot", lambda path=None: None)

    node = {**NODE, "model_tier": "low"}
    monkeypatch.setattr(adv, "_next_node", lambda project: node)
    monkeypatch.setattr(adv, "_spawn_worker", spawn)
    res = adv.advance(project="fno", events_path=iso)
    assert res.decision == "dispatched"
    assert captured["model"] == "glm-4.7"  # STATIC_TIERS['low'][0]


def test_advance_cli_pin_overrides_node(iso, monkeypatch):
    """Locked Decision 1: a dispatch-time model/provider outranks node annotations."""
    captured = {}

    def spawn(node_id, node_cwd, node_slug=None, model=None, provider=None):
        captured.update(model=model, provider=provider)
        return "sid"

    node = {**NODE, "model": "node-model", "provider": "gemini"}
    monkeypatch.setattr(adv, "_next_node", lambda project: node)
    monkeypatch.setattr(adv, "_spawn_worker", spawn)
    res = adv.advance(project="fno", events_path=iso, model="cli-model", provider="codex")
    assert res.decision == "dispatched"
    assert captured == {"model": "cli-model", "provider": "codex"}


def test_release_raises_still_emits_and_never_raises(iso, monkeypatch):
    """LD#12 regression: if release_claim itself raises on the spawn-failure
    path, advance still emits exactly one decision event and does not raise."""
    monkeypatch.setattr(adv, "_next_node", lambda project: NODE)

    def boom_spawn(node_id, node_cwd, node_slug=None, model=None, provider=None):
        raise adv.SpawnError("spawn boom")

    monkeypatch.setattr(adv, "_spawn_worker", boom_spawn)

    import fno.claims.core as core

    def boom_release(*a, **k):
        raise OSError("cannot unlink lock")

    monkeypatch.setattr(core, "release_claim", boom_release)

    res = adv.advance(project="fno", events_path=iso)  # must NOT raise

    assert res.decision == "failed"
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "advance_failed"


def test_exactly_one_event_every_path(iso, monkeypatch, tmp_path):
    """AC1-UI / LD#12: every decision path emits exactly one event - iterated."""
    scenarios = [
        ("disabled", {"FNO_AUTO_CONTINUE": "0"}, lambda p: NODE, lambda *a, **k: "s", "advance_skipped"),
        ("no_work", {}, lambda p: None, lambda *a, **k: "s", "advance_skipped"),
        ("dispatched", {}, lambda p: NODE, lambda *a, **k: "s", "advance_dispatched"),
    ]
    for name, env, nxt, spawn, expected in scenarios:
        ev = tmp_path / f".fno/events-{name}.jsonl"
        for k, v in env.items():
            monkeypatch.setenv(k, v)
        if not env:
            monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")
        monkeypatch.setattr(adv, "_next_node", nxt)
        monkeypatch.setattr(adv, "_spawn_worker", spawn)
        adv.advance(project="fno", events_path=ev)
        evs = _events(ev)
        assert len(evs) == 1, f"{name}: expected one event, got {len(evs)}"
        assert evs[0]["type"] == expected, f"{name}: got {evs[0]['type']}"


# ---------------------------------------------------------------------------
# _spawn_worker: the dispatch boundary (AC3 - never mocked elsewhere)
# ---------------------------------------------------------------------------


def test_spawn_worker_argv_with_cwd(monkeypatch):
    captured = {}

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _FakeProc(0, _RECEIPT)

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    sid = adv._spawn_worker("ab-2222aaaa", "/work/dir")

    assert sid == "abc12345"
    cmd = captured["cmd"]
    assert cmd[:5] == ["fno-py", "agents", "spawn", "--provider", "claude"]
    assert "--cwd" in cmd and "/work/dir" in cmd
    assert "--fresh" not in cmd
    assert cmd[-2] == "target-ab-2222aaaa"
    assert cmd[-1] == "/target no-merge ab-2222aaaa"  # no-merge rides as a token
    # subscription lane only - never the API-credit/-p lane.
    assert "-p" not in cmd and "--print" not in cmd and "--bare" not in cmd


def test_spawn_worker_threads_model_and_provider(monkeypatch):
    """A per-node/dispatch pin reaches the spawn cmd as --model / --provider."""
    captured = {}

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _FakeProc(0, _RECEIPT)

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    adv._spawn_worker("ab-2222aaaa", "/w", model="glm-4.7", provider="codex")
    cmd = captured["cmd"]
    assert cmd[cmd.index("--provider") + 1] == "codex"
    assert cmd[cmd.index("--model") + 1] == "glm-4.7"


def test_spawn_worker_default_provider_claude(monkeypatch):
    """Byte-for-byte default: no provider pin -> --provider claude."""
    captured = {}

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _FakeProc(0, _RECEIPT)

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    adv._spawn_worker("ab-2222aaaa", "/w")
    cmd = captured["cmd"]
    assert cmd[cmd.index("--provider") + 1] == "claude"
    assert "--model" not in cmd


def test_spawn_worker_argv_fresh_when_no_cwd(monkeypatch):
    captured = {}

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _FakeProc(0, _RECEIPT)

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    sid = adv._spawn_worker("ab-2222aaaa", None)

    assert sid == "abc12345"
    cmd = captured["cmd"]
    assert "--fresh" in cmd and "--cwd" not in cmd
    assert cmd[-1] == "/target no-merge ab-2222aaaa"


def test_worker_agent_name_carries_verb_id_and_slug():
    # Provenance-carrying name: target-<full-node-id>-<slug>, degrading to
    # target-<full-node-id> when the node has no slug.
    assert adv._worker_agent_name("ab-2222aaaa", "cargo-bootstrapper") == \
        "target-ab-2222aaaa-cargo-bootstrapper"
    assert adv._worker_agent_name("ab-2222aaaa", None) == "target-ab-2222aaaa"
    assert adv._worker_agent_name("ab-2222aaaa", "") == "target-ab-2222aaaa"
    # Parity with the shell dispatchers (codex P2 / gemini HIGH, PR #525): an
    # unsanitized title fallback (caps/spaces/punct) must normalize identically,
    # and a slug longer than the 30-char cut must truncate (graph slugs reach 48)
    # so the Python name never diverges from dispatch-node.sh's.
    assert adv._worker_agent_name("ab-2222aaaa", "Cargo Bootstrapper!!") == \
        "target-ab-2222aaaa-cargo-bootstrapper"
    assert adv._worker_agent_name("ab-2222aaaa", "x" * 35) == \
        "target-ab-2222aaaa-" + "x" * 30


def test_spawn_worker_name_includes_slug(monkeypatch):
    captured = {}

    def fake_run(cmd, **kw):
        captured["cmd"] = cmd
        return _FakeProc(0, _RECEIPT)

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    adv._spawn_worker("ab-2222aaaa", None, "cargo-bootstrapper")
    assert captured["cmd"][-2] == "target-ab-2222aaaa-cargo-bootstrapper"


def test_spawn_worker_name_collision_raises_already_running(monkeypatch):
    monkeypatch.setattr(
        adv.subprocess, "run",
        lambda cmd, **kw: _FakeProc(2, "", "agent tgt-x already exists"),
    )
    with pytest.raises(adv.SpawnAlreadyRunning):
        adv._spawn_worker("ab-2222aaaa", None)


def test_spawn_worker_other_failure_raises_spawn_error(monkeypatch):
    monkeypatch.setattr(
        adv.subprocess, "run",
        lambda cmd, **kw: _FakeProc(1, "", "daemon unreachable"),
    )
    with pytest.raises(adv.SpawnError):
        adv._spawn_worker("ab-2222aaaa", None)


def test_spawn_worker_skips_noise_line_mentioning_short_id(monkeypatch):
    """A non-JSON stdout line that merely mentions short_id must not abort the
    parse; the real receipt on a later line still wins (gemini review)."""
    noisy = 'note: writing short_id to log\n' + _RECEIPT
    monkeypatch.setattr(adv.subprocess, "run", lambda cmd, **kw: _FakeProc(0, noisy))
    assert adv._spawn_worker("ab-2222aaaa", None) == "abc12345"


def test_spawn_worker_exit0_no_receipt_raises_spawn_error(monkeypatch):
    monkeypatch.setattr(
        adv.subprocess, "run",
        lambda cmd, **kw: _FakeProc(0, "some banner noise\n", ""),
    )
    with pytest.raises(adv.SpawnError):
        adv._spawn_worker("ab-2222aaaa", None)


# ---------------------------------------------------------------------------
# cmd_advance: the `fno backlog advance` CLI verb
# ---------------------------------------------------------------------------


def test_cmd_advance_json_output(monkeypatch):
    import fno.backlog.advance as advmod

    monkeypatch.setattr(
        advmod, "advance",
        lambda **k: advmod.AdvanceResult("skipped", "advance_skipped", reason="disabled"),
    )
    result = runner.invoke(app, ["backlog", "advance", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["decision"] == "skipped" and payload["reason"] == "disabled"


def test_cmd_advance_human_output(monkeypatch):
    import fno.backlog.advance as advmod

    monkeypatch.setattr(
        advmod, "advance",
        lambda **k: advmod.AdvanceResult(
            "dispatched", "advance_dispatched", node_id="ab-2222aaaa", short_id="sid"
        ),
    )
    result = runner.invoke(app, ["backlog", "advance"])
    assert result.exit_code == 0, result.output
    assert "dispatched ab-2222aaaa" in result.output and "short_id=sid" in result.output


def test_cmd_advance_always_exits_zero_on_unexpected_error(monkeypatch):
    """The verb's docstring promises 'always exits 0'; an escaped exception
    must not traceback the CLI."""
    import fno.backlog.advance as advmod

    def boom(**k):
        raise RuntimeError("kaboom")

    monkeypatch.setattr(advmod, "advance", boom)
    result = runner.invoke(app, ["backlog", "advance"])
    assert result.exit_code == 0


def test_advance_result_rejects_invalid_pair():
    """AdvanceResult guards against a mismatched (decision, event) pair."""
    with pytest.raises(ValueError):
        adv.AdvanceResult("dispatched", "advance_skipped")


# ---------------------------------------------------------------------------
# _next_node: _resolved_cwd enrichment (codex P2 - launch from mapped root)
# ---------------------------------------------------------------------------


def test_next_node_enriches_resolved_cwd(monkeypatch):
    """`fno backlog next` omits _resolved_cwd; _next_node fetches it via get so
    the worker launches from the mapped project root."""
    calls = []

    def fake_run(cmd, **kw):
        calls.append(cmd[:3])
        if cmd[:3] == ["fno-py", "backlog", "next"]:
            return _FakeProc(0, json.dumps({"id": "ab-2222aaaa", "cwd": "/raw"}))
        if cmd[:3] == ["fno-py", "backlog", "get"]:
            return _FakeProc(0, json.dumps(
                {"id": "ab-2222aaaa", "cwd": "/raw", "_resolved_cwd": "/mapped/root"}))
        return _FakeProc(1)

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    node = adv._next_node("fno")
    assert node["_resolved_cwd"] == "/mapped/root"
    assert ["fno-py", "backlog", "get"] in calls


def test_next_node_get_failure_is_nonfatal(monkeypatch):
    def fake_run(cmd, **kw):
        if cmd[:3] == ["fno-py", "backlog", "next"]:
            return _FakeProc(0, json.dumps({"id": "ab-2222aaaa", "cwd": "/raw"}))
        return _FakeProc(1, "", "get exploded")

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    node = adv._next_node("fno")
    assert node["id"] == "ab-2222aaaa"  # still returns; _spawn_worker falls back to .cwd
    assert not node.get("_resolved_cwd")


def test_next_node_skips_get_when_already_resolved(monkeypatch):
    calls = []

    def fake_run(cmd, **kw):
        calls.append(cmd[:3])
        return _FakeProc(0, json.dumps(
            {"id": "ab-2222aaaa", "cwd": "/raw", "_resolved_cwd": "/already"}))

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    node = adv._next_node("fno")
    assert node["_resolved_cwd"] == "/already"
    assert ["fno-py", "backlog", "get"] not in calls  # no redundant get


# ---------------------------------------------------------------------------
# advance_dependents: cross-project successor dispatch (G1 / AC5-FR)
# ---------------------------------------------------------------------------


_DEP = {
    "id": "ab-3333bbbb", "project": "web", "slug": "frontend-bit", "cwd": "/raw/web",
    "cross_project": True,  # closed node is project "etl"; _direct_dependents tags this
}


def _map_project(monkeypatch, mapping):
    """Stub project_root_from_settings (imported inside _dispatch_one_dependent)."""
    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "project_root_from_settings", lambda p: mapping.get(p))


def test_dependents_cross_project_dispatch(iso, monkeypatch):
    """AC5-FR happy path: a now-unblocked foreign dependent is spawned --cwd its
    own mapped root; one advance_dispatched{cross_project} event."""
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_DEP])
    _map_project(monkeypatch, {"web": "/mapped/web"})
    captured = {}

    def fake_spawn(node_id, node_cwd, node_slug=None, model=None, provider=None):
        captured["args"] = (node_id, node_cwd, node_slug)
        return "depsid01"

    monkeypatch.setattr(adv, "_spawn_worker", fake_spawn)

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )

    assert len(results) == 1 and results[0].decision == "dispatched"
    # --cwd resolves to the dependent's MAPPED root, not its raw recorded cwd.
    assert captured["args"] == ("ab-3333bbbb", "/mapped/web", "frontend-bit")
    # dispatch reservation lives on after return (dedup vs a peer trigger).
    key = f"dispatch:{_DEP['id']}"
    assert claim_status(key, root=adv._claims_root_for(key)).get("state") == "live"
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "advance_dispatched"
    assert evs[0]["data"]["node_id"] == _DEP["id"]
    assert evs[0]["data"]["cross_project"] is True
    assert evs[0]["data"]["closed_node_id"] == "ab-1111aaaa"


def test_dependents_unmapped_project_refused(iso, monkeypatch):
    """Boundaries: an unmapped foreign project is refused (not guessed), naming
    the project; no spawn."""
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_DEP])
    _map_project(monkeypatch, {})  # "web" unmapped
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )

    assert results[0].decision == "skipped" and results[0].reason == "unmapped-project"
    evs = _events(iso)
    assert evs[0]["data"]["reason"] == "unmapped-project"
    assert evs[0]["data"]["detail"] == "web"  # surfaced by name


def test_dependents_no_project_skipped(iso, monkeypatch):
    dep = {"id": "ab-3333bbbb", "project": None, "slug": "x", "cross_project": True}
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [dep])
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )
    assert results[0].decision == "skipped" and results[0].reason == "no-project"


def test_dependents_disabled_is_noop(iso, monkeypatch):
    """Disabled -> [] and NO event (advance() already recorded the decision)."""
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "0")
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: pytest.fail("must not read graph"))

    results = adv.advance_dependents(closed_node_id="ab-1111aaaa", events_path=iso)
    assert results == []
    assert _events(iso) == []


def test_dependents_walker_live_is_noop(iso, monkeypatch):
    _hold(adv._walker_key())
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: pytest.fail("must not read graph"))
    results = adv.advance_dependents(closed_node_id="ab-1111aaaa", events_path=iso)
    assert results == []


def test_dependents_already_claimed_skips(iso, monkeypatch):
    _hold(f"node:{_DEP['id']}")
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_DEP])
    _map_project(monkeypatch, {"web": "/mapped/web"})
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )
    assert results[0].decision == "skipped" and results[0].reason == "already-claimed"


def test_dependents_idempotent_double_call(iso, monkeypatch):
    """Concurrency: the same merge observed twice dispatches the dependent once
    (dispatch:<id> TTL reservation survives the first call)."""
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_DEP])
    _map_project(monkeypatch, {"web": "/mapped/web"})
    calls = []
    monkeypatch.setattr(adv, "_spawn_worker", lambda nid, cwd, slug=None, model=None, provider=None: calls.append(nid) or "sid")

    first = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )
    second = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )

    assert first[0].decision == "dispatched"
    assert second[0].decision == "skipped" and second[0].reason == "already-claimed"
    assert calls == [_DEP["id"]]  # spawned exactly once


def test_dependents_spawn_failure_releases_reservation(iso, monkeypatch):
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_DEP])
    _map_project(monkeypatch, {"web": "/mapped/web"})

    def boom(nid, cwd, slug=None):
        raise adv.SpawnError("daemon down")

    monkeypatch.setattr(adv, "_spawn_worker", boom)

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )
    assert results[0].decision == "failed"
    key = f"dispatch:{_DEP['id']}"
    assert claim_status(key, root=adv._claims_root_for(key)).get("state") == "free"
    assert _events(iso)[0]["type"] == "advance_failed"


# ---------------------------------------------------------------------------
# RC1 (x-33b2): same-project dependent dispatch + closed_project provenance
# ---------------------------------------------------------------------------

_SAME_DEP = {
    "id": "ab-4444cccc", "project": "fno", "slug": "e4-3-leaf", "cwd": "/repo/fno",
    "cross_project": False,  # _direct_dependents tags a same-project dep this way
}


def test_dependents_same_project_dispatch(iso, monkeypatch):
    """AC1-HP: a now-unblocked SAME-project dependent is spawned --cwd its OWN
    project root, resolved work-map-first like the `next` path (codex P2), with
    one advance_dispatched event whose cross_project is False."""
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_SAME_DEP])
    # Work-map root is the authority; it resolves to the node's OWN project root.
    _map_project(monkeypatch, {"fno": "/mapped/fno"})
    captured = {}

    def fake_spawn(node_id, node_cwd, node_slug=None, model=None, provider=None):
        captured["args"] = (node_id, node_cwd, node_slug)
        return "samesid1"

    monkeypatch.setattr(adv, "_spawn_worker", fake_spawn)

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="fno", events_path=iso
    )

    assert len(results) == 1 and results[0].decision == "dispatched"
    # --cwd is the work-map-resolved OWN project root (not a foreign root).
    assert captured["args"] == ("ab-4444cccc", "/mapped/fno", "e4-3-leaf")
    evs = _events(iso)
    assert len(evs) == 1 and evs[0]["type"] == "advance_dispatched"
    assert evs[0]["data"]["node_id"] == "ab-4444cccc"
    assert evs[0]["data"]["cross_project"] is False


def test_dependents_same_project_falls_back_to_recorded_cwd(iso, monkeypatch):
    """codex P2: when the same-project node is unmapped in the work map, the route
    falls back to the node's recorded cwd (never a foreign root)."""
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_SAME_DEP])
    _map_project(monkeypatch, {})  # "fno" unmapped -> fall back to recorded cwd
    captured = {}
    monkeypatch.setattr(
        adv, "_spawn_worker",
        lambda nid, cwd, slug=None, model=None, provider=None: captured.update(cwd=cwd) or "sid",
    )

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="fno", events_path=iso
    )
    assert results[0].decision == "dispatched"
    assert captured["cwd"] == "/repo/fno"  # _SAME_DEP's recorded cwd


def test_dependents_same_project_no_cwd_skips(iso, monkeypatch):
    """A same-project dependent with neither a mapped project nor a recorded cwd
    is fail-closed (never guessed to canonical main), surfaced as skipped{no-cwd}."""
    dep = {"id": "ab-4444cccc", "project": "fno", "slug": "x", "cross_project": False}
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [dep])
    _map_project(monkeypatch, {})  # unmapped AND dep has no cwd
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="fno", events_path=iso
    )
    assert results[0].decision == "skipped" and results[0].reason == "no-cwd"


def test_dependents_fail_closed_on_unknown_closed_project(iso, monkeypatch):
    """AC1-ERR / Failure Modes: closed_project=None means we cannot classify a
    dependent, so we dispatch NOTHING (prefer that over misrouting a same-project
    node cross-project onto a protected branch). One skip event, no graph read."""
    monkeypatch.setattr(
        adv, "_direct_dependents",
        lambda cid, cproj: pytest.fail("must not read graph when closed_project is None"),
    )
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    # None (the normal last-resort) AND an empty-string project both fail closed:
    # an empty string would otherwise tag every dependent cross_project=True and
    # misroute it (gemini: `not closed_project` over `is None`).
    for bad in (None, ""):
        kwargs = {"closed_node_id": "ab-1111aaaa", "events_path": iso}
        if bad is not None:
            kwargs["closed_project"] = bad
        results = adv.advance_dependents(**kwargs)
        assert len(results) == 1
        assert results[0].decision == "skipped"
        assert results[0].reason == "closed-project-unknown"


def test_dependents_dispatch_independent_of_next_selection(iso, monkeypatch):
    """AC1-EDGE: the dependent path never consults `fno backlog next`, so an
    already-claimed/epic global head can never starve a genuinely-unblocked
    same-project dependent."""
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_SAME_DEP])
    monkeypatch.setattr(
        adv, "_next_node",
        lambda project: pytest.fail("advance_dependents must not select via `next`"),
    )
    monkeypatch.setattr(adv, "_spawn_worker", lambda nid, cwd, slug=None, model=None, provider=None: "edgesid")

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="fno", events_path=iso
    )
    assert results[0].decision == "dispatched" and results[0].node_id == "ab-4444cccc"


def test_direct_dependents_tags_same_and_cross_project(monkeypatch):
    """RC1 unit: _direct_dependents returns BOTH same- and cross-project ready
    dependents, each tagged with cross_project (no longer excludes same-project)."""
    import fno.graph.store as store
    import fno.paths as paths

    entries = [
        {"id": "ab-same", "project": "fno", "blocked_by": ["ab-A"], "_status": "ready",
         "slug": "same", "cwd": "/repo/fno"},
        {"id": "ab-cross", "project": "web", "blocked_by": ["ab-A"], "_status": "ready",
         "slug": "cross", "cwd": "/repo/web"},
        {"id": "ab-blocked", "project": "fno", "blocked_by": ["ab-A"], "_status": "blocked"},
        {"id": "ab-other", "project": "fno", "blocked_by": ["ab-Z"], "_status": "ready"},
    ]
    monkeypatch.setattr(store, "read_graph", lambda p: entries)
    monkeypatch.setattr(paths, "graph_json", lambda: Path("/unused/graph.json"))

    deps = adv._direct_dependents("ab-A", "fno")
    by_id = {d["id"]: d for d in deps}
    assert set(by_id) == {"ab-same", "ab-cross"}  # blocked + other-blocker excluded
    assert by_id["ab-same"]["cross_project"] is False
    assert by_id["ab-cross"]["cross_project"] is True


def test_cmd_advance_resolves_closed_project_from_graph(monkeypatch):
    """AC1-ERR (integration): `fno backlog advance --closed A` with NO --project
    resolves closed_project from A's graph record, not from the omitted flag, so
    advance_dependents gets the closed node's real project."""
    import fno.backlog.advance as advmod
    import fno.backlog.reconcile_dispatch as recmod
    import fno.graph.cli as gcli
    import fno.graph.store as store

    monkeypatch.setattr(
        advmod, "advance",
        lambda **k: advmod.AdvanceResult("skipped", "advance_skipped", reason="disabled"),
    )
    captured = {}
    monkeypatch.setattr(
        advmod, "advance_dependents",
        lambda **k: captured.update(k) or [],
    )
    monkeypatch.setattr(recmod, "dispatch_reconcile_for_blocker", lambda **k: None)
    monkeypatch.setattr(
        store, "read_graph",
        lambda p: [{"id": "ab-A", "project": "fno"}],
    )
    monkeypatch.setattr(gcli, "_graph_path", lambda: Path("/unused/graph.json"))

    result = runner.invoke(app, ["backlog", "advance", "--closed", "ab-A"])
    assert result.exit_code == 0, result.output
    # closed_project came from A's graph record ("fno"), NOT the omitted --project.
    assert captured.get("closed_project") == "fno"
    assert captured.get("closed_node_id") == "ab-A"


def test_dependents_dependents_error_skips_never_guesses(iso, monkeypatch):
    def boom(cid, cproj):
        raise RuntimeError("graph read exploded")

    monkeypatch.setattr(adv, "_direct_dependents", boom)
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )
    assert results[0].decision == "skipped" and results[0].reason == "dependents-error"


def test_dependents_zero_dependents_is_clean_noop(iso, monkeypatch):
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [])
    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )
    assert results == []
    assert _events(iso) == []


# ---------------------------------------------------------------------------
# _direct_dependents: the edge-following filter (ready dependents, both projects)
# ---------------------------------------------------------------------------


def test_direct_dependents_filters_to_ready(monkeypatch):
    entries = [
        {"id": "A", "project": "etl", "_status": "done", "blocked_by": []},
        # ready cross-project direct dependent -> INCLUDED (cross_project True)
        {"id": "B", "project": "web", "_status": "ready", "blocked_by": ["A"],
         "slug": "bee", "cwd": "/w"},
        # ready same-project dependent -> NOW INCLUDED (RC1; cross_project False)
        {"id": "C", "project": "etl", "_status": "ready", "blocked_by": ["A"]},
        # cross-project but still blocked by another open node -> EXCLUDED
        {"id": "D", "project": "web", "_status": "blocked", "blocked_by": ["A", "X"]},
        # cross-project dependent with no plan (idea) -> EXCLUDED
        {"id": "E", "project": "web", "_status": "idea", "blocked_by": ["A"]},
        # not a dependent of A -> EXCLUDED
        {"id": "F", "project": "web", "_status": "ready", "blocked_by": []},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda path=None: entries)
    deps = adv._direct_dependents("A", "etl")
    by_id = {d["id"]: d for d in deps}
    assert set(by_id) == {"B", "C"}  # ready dependents in BOTH projects
    assert by_id["B"]["project"] == "web" and by_id["B"]["slug"] == "bee"
    assert by_id["B"]["cross_project"] is True
    assert by_id["C"]["cross_project"] is False


def test_direct_dependents_treats_missing_closed_project_as_cross(monkeypatch):
    """A closed node with no project still surfaces foreign dependents."""
    entries = [
        {"id": "B", "project": "web", "_status": "ready", "blocked_by": ["A"]},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda path=None: entries)
    deps = adv._direct_dependents("A", None)
    assert [d["id"] for d in deps] == ["B"]


def test_direct_dependents_skips_pr_in_flight(monkeypatch):
    """codex P2: a dependent already in review (pr_number set, not closed) still
    reads `ready`, but must NOT be re-dispatched - mirror _has_unmerged_open_pr."""
    entries = [
        # ready cross-project dep WITH an open PR -> EXCLUDED (in review)
        {"id": "B", "project": "web", "_status": "ready", "blocked_by": ["A"],
         "pr_number": 99, "completed_at": None},
        # ready cross-project dep with NO pr -> INCLUDED
        {"id": "C", "project": "web", "_status": "ready", "blocked_by": ["A"]},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda path=None: entries)
    deps = adv._direct_dependents("A", "etl")
    assert [d["id"] for d in deps] == ["C"]


def test_direct_dependents_skips_non_dict_and_idless(monkeypatch):
    """gemini medium: a malformed (non-dict / id-less) entry is skipped, not crashed."""
    entries = [
        "not-a-dict",
        {"project": "web", "_status": "ready", "blocked_by": ["A"]},  # no id
        {"id": "B", "project": "web", "_status": "ready", "blocked_by": ["A"]},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda path=None: entries)
    deps = adv._direct_dependents("A", "etl")
    assert [d["id"] for d in deps] == ["B"]


def test_direct_dependents_skips_epic_dependent(monkeypatch):
    """x-33b2: a now-unblocked dependent that is itself a container (some node's
    `parent`) is NOT dispatched on merge - build its leaves, not the box. Mirrors
    cmd_next's epic exclusion on the edge-following dependent path."""
    entries = [
        # E is a ready dependent of A, but it is also B's parent -> an epic.
        {"id": "E", "project": "web", "_status": "ready", "blocked_by": ["A"]},
        {"id": "B", "project": "web", "_status": "ready", "parent": "E"},
        # L is a ready leaf dependent of A (no children) -> dispatched.
        {"id": "L", "project": "web", "_status": "ready", "blocked_by": ["A"]},
    ]
    monkeypatch.setattr("fno.graph.store.read_graph", lambda path=None: entries)
    deps = adv._direct_dependents("A", "etl")
    assert [d["id"] for d in deps] == ["L"]  # epic E skipped, leaf L kept


def test_dependents_honors_dependent_repo_walker(iso, monkeypatch):
    """codex P2: a live walker in the DEPENDENT's repo suppresses the spawn (the
    walker will claim the node itself; spawning would double-launch there)."""
    monkeypatch.setattr(adv, "_direct_dependents", lambda cid, cproj: [_DEP])
    _map_project(monkeypatch, {"web": "/mapped/web"})
    monkeypatch.setattr(adv, "_walker_live_at", lambda root: root == "/mapped/web")
    monkeypatch.setattr(adv, "_spawn_worker", lambda *a, **k: pytest.fail("must not spawn"))

    results = adv.advance_dependents(
        closed_node_id="ab-1111aaaa", closed_project="etl", events_path=iso
    )
    assert results[0].decision == "skipped" and results[0].reason == "walker-live"


# ---------------------------------------------------------------------------
# x-e9cf: the spawn command carries --substrate bg (the missed 4th dispatch
# surface). These exercise the REAL _spawn_worker - every other test patches it.
# ---------------------------------------------------------------------------


def _capture_spawn_argv(monkeypatch):
    """Patch advance's subprocess.run to capture argv and return a valid receipt."""
    captured = {}

    def fake_run(cmd, *a, **k):
        captured["cmd"] = cmd
        return _FakeProc(returncode=0, stdout=_RECEIPT)

    monkeypatch.setattr(adv.subprocess, "run", fake_run)
    return captured


def test_spawn_worker_passes_substrate_bg(monkeypatch):
    """AC1-HP: the fire-and-forget spawn carries `--substrate bg` immediately
    after `--provider claude` (the x-3ab8 default `pane` would stall it)."""
    captured = _capture_spawn_argv(monkeypatch)
    sid = adv._spawn_worker("ab-2222aaaa", "/tmp/x", "some-slug")
    assert sid == "abc12345"  # receipt parse unchanged
    cmd = captured["cmd"]
    assert "--substrate" in cmd and cmd[cmd.index("--substrate") + 1] == "bg"
    i = cmd.index("--provider")
    assert cmd[i : i + 4] == ["--provider", "claude", "--substrate", "bg"]


def test_spawn_worker_reconcile_keeps_substrate_bg(monkeypatch):
    """AC4-EDGE: the G4 `--reconcile` variant still carries `--substrate bg`
    (substrate is orthogonal to the /target ... --reconcile payload token)."""
    captured = _capture_spawn_argv(monkeypatch)
    sid = adv._spawn_worker(
        "ab-2222aaaa", "/tmp/x", "some-slug", reconcile_manifest="/tmp/m.md"
    )
    assert sid == "abc12345"
    cmd = captured["cmd"]
    i = cmd.index("--provider")
    assert cmd[i : i + 4] == ["--provider", "claude", "--substrate", "bg"]
    assert any("--reconcile /tmp/m.md ab-2222aaaa" in tok for tok in cmd)


def test_spawn_worker_error_contract_unchanged(monkeypatch):
    """AC1-ERR: the --substrate bg addition must not alter the error contract -
    exit 2 + 'already exists' -> SpawnAlreadyRunning; other non-zero -> SpawnError."""
    monkeypatch.setattr(
        adv.subprocess, "run",
        lambda *a, **k: _FakeProc(returncode=2, stderr="agent already exists"),
    )
    with pytest.raises(adv.SpawnAlreadyRunning):
        adv._spawn_worker("ab-2222aaaa", "/tmp/x", "slug")

    monkeypatch.setattr(
        adv.subprocess, "run",
        lambda *a, **k: _FakeProc(returncode=1, stderr="boom"),
    )
    with pytest.raises(adv.SpawnError):
        adv._spawn_worker("ab-2222aaaa", "/tmp/x", "slug")


# ---------------------------------------------------------------------------
# x-4391: config.dispatch.auto_merge drives the merge posture token
# ---------------------------------------------------------------------------


def _settings_ns(auto_merge=False, perm=""):
    import types

    return types.SimpleNamespace(
        agents=types.SimpleNamespace(spawn_permission_mode=perm),
        dispatch=types.SimpleNamespace(auto_merge=auto_merge),
    )


def test_spawn_worker_auto_merge_true_omits_no_merge(monkeypatch):
    """AC2-HP: config.dispatch.auto_merge=true -> /target <id> (no no-merge)."""
    import fno.config as _config

    captured = _capture_spawn_argv(monkeypatch)
    monkeypatch.setattr(_config, "load_settings_for_repo", lambda _p: _settings_ns(auto_merge=True))
    adv._spawn_worker("ab-2222aaaa", "/work/dir")
    assert captured["cmd"][-1] == "/target ab-2222aaaa"


def test_spawn_worker_auto_merge_reconcile_variant(monkeypatch):
    """Allow posture drops no-merge from the G4 --reconcile variant too."""
    import fno.config as _config

    captured = _capture_spawn_argv(monkeypatch)
    monkeypatch.setattr(_config, "load_settings_for_repo", lambda _p: _settings_ns(auto_merge=True))
    adv._spawn_worker("ab-2222aaaa", "/work/dir", reconcile_manifest="/tmp/m.md")
    assert captured["cmd"][-1] == "/target --reconcile /tmp/m.md ab-2222aaaa"


def test_spawn_worker_auto_merge_reads_dependent_cwd(monkeypatch):
    """AC2-EDGE: posture is read from the DEPENDENT node's project via
    load_settings_for_repo(node_cwd), never the caller / merged repo."""
    import fno.config as _config

    captured = _capture_spawn_argv(monkeypatch)
    seen = {}

    def _lsfr(p):
        seen["path"] = str(p)
        return _settings_ns(auto_merge=True)

    monkeypatch.setattr(_config, "load_settings_for_repo", _lsfr)
    adv._spawn_worker("ab-2222aaaa", "/dependent/repo")
    assert seen["path"] == "/dependent/repo"
    assert captured["cmd"][-1] == "/target ab-2222aaaa"


def test_spawn_worker_auto_merge_read_failure_no_merge(monkeypatch):
    """AC2-ERR: a settings read that raises degrades to no-merge (never grant
    merge on a failed read)."""
    import fno.config as _config

    captured = _capture_spawn_argv(monkeypatch)

    def _boom(_p):
        raise RuntimeError("corrupt toml")

    monkeypatch.setattr(_config, "load_settings_for_repo", _boom)
    adv._spawn_worker("ab-2222aaaa", "/work/dir")
    assert captured["cmd"][-1] == "/target no-merge ab-2222aaaa"
