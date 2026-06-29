"""Group 2 (x-e4ac / US2 / AC2): the persistent session registry.

The CC-discovery half (``discover_live_sessions``) is already covered by the
agents suite, so it is stubbed here -- these tests isolate the registry's own
behavior: persist a footnote-owned peer, survive a reload, fold discovered
claude sessions in, and let a persisted peer win a session-id clash.
"""
from __future__ import annotations

from fno.agents.discover import DiscoveredSession
from fno.relay import registry as reg
from fno.relay.registry import RegistryEntry


def _peer(sid="B", provider="claude", pid=4242, handle="pty:4242"):
    return RegistryEntry(
        session_id=sid, provider=provider, pid=pid,
        cwd="/tmp/wt", inject_handle=handle, status="idle", name="bob",
    )


def test_register_then_load_returns_handle_and_provider(tmp_path):
    # AC2-HP core: a registered session carries its inject_handle + provider.
    path = tmp_path / "registry.json"
    reg.register(_peer(), path=path)
    loaded = reg.load(path)
    assert loaded["B"].inject_handle == "pty:4242"
    assert loaded["B"].provider == "claude"


def test_persistence_survives_reload(tmp_path):
    # "survives restarts": a fresh load of the same file re-reads the peer.
    path = tmp_path / "registry.json"
    reg.register(_peer(), path=path)
    assert reg.load(path)["B"] == _peer()


def test_load_missing_or_corrupt_yields_empty(tmp_path):
    missing = tmp_path / "nope.json"
    assert reg.load(missing) == {}
    garbage = tmp_path / "garbage.json"
    garbage.write_text("{not json", encoding="utf-8")
    assert reg.load(garbage) == {}


def test_register_upserts(tmp_path):
    path = tmp_path / "registry.json"
    reg.register(_peer(handle="pty:1"), path=path)
    reg.register(_peer(handle="pty:2"), path=path)
    assert reg.load(path)["B"].inject_handle == "pty:2"
    assert len(reg.load(path)) == 1


def test_unregister(tmp_path):
    path = tmp_path / "registry.json"
    reg.register(_peer(), path=path)
    reg.unregister("B", path=path)
    assert reg.load(path) == {}
    reg.unregister("B", path=path)  # missing -> silent no-op


def test_index_folds_discovered_claude_sessions(tmp_path, monkeypatch):
    monkeypatch.setattr(reg, "discover_live_sessions", lambda: [
        DiscoveredSession(
            session_id="live-1", short_id="dead", handle="alice",
            pid=99, cwd="/tmp/a", project="fno", status="busy", agent="claude",
        )
    ])
    idx = reg.index(path=tmp_path / "registry.json")
    assert idx["live-1"].provider == "claude"
    assert idx["live-1"].inject_handle is None  # hand-started: not footnote-owned
    assert idx["live-1"].status == "busy"


def test_transcript_path_round_trips(tmp_path):
    path = tmp_path / "registry.json"
    reg.register(RegistryEntry(session_id="B", provider="claude", pid=1,
                               transcript_path="/x/B.jsonl"), path=path)
    assert reg.load(path)["B"].transcript_path == "/x/B.jsonl"


def test_transcript_path_for_globs_by_session_id(tmp_path):
    proj = tmp_path / "projects"
    # claude encodes both "/" and "." in the cwd to "-", so the dir name is
    # unpredictable; glob by the <session_id>.jsonl filename instead.
    d = proj / "-Users-bb16--claude-jobs-x"
    d.mkdir(parents=True)
    (d / "sid-123.jsonl").write_text("{}")
    assert reg.transcript_path_for("sid-123", projects_dir=proj) == str(d / "sid-123.jsonl")
    assert reg.transcript_path_for("missing", projects_dir=proj) is None


def test_index_populates_transcript_path(tmp_path, monkeypatch):
    monkeypatch.setattr(reg, "discover_live_sessions", lambda: [
        DiscoveredSession(session_id="live-1", short_id="d", handle="a",
                          pid=9, cwd="/t", project=None, status="idle", agent="claude")
    ])
    monkeypatch.setattr(reg, "transcript_path_for", lambda sid, **k: f"/tx/{sid}.jsonl")
    idx = reg.index(path=tmp_path / "registry.json")
    assert idx["live-1"].transcript_path == "/tx/live-1.jsonl"


def test_persisted_peer_wins_session_id_clash(tmp_path, monkeypatch):
    path = tmp_path / "registry.json"
    reg.register(_peer(sid="dup", handle="pty:owned"), path=path)
    monkeypatch.setattr(reg, "discover_live_sessions", lambda: [
        DiscoveredSession(
            session_id="dup", short_id="x", handle="ghost",
            pid=1, cwd="/x", project=None, status="idle", agent="claude",
        )
    ])
    idx = reg.index(path=path)
    assert idx["dup"].inject_handle == "pty:owned"  # persisted handle survives


# ---------------------------------------------------------------------------
# G4 / x-3f34: the cross-harness bridge -- live non-claude interactive agents
# workers surfaced as routable relay peers (keyed by short_id, worker:<id> handle).
# ---------------------------------------------------------------------------

def _write_agents_registry(home, rows):
    home.mkdir(parents=True, exist_ok=True)
    (home / "registry.json").write_text(__import__("json").dumps({"agents": rows}))


def _no_discovery(monkeypatch):
    monkeypatch.setattr(reg, "discover_live_sessions", lambda: [])


def test_index_surfaces_live_codex_worker(tmp_path, monkeypatch):
    # A live interactive codex worker becomes an addressable relay peer keyed by its
    # short_id, with a worker:<short_id> inject handle the daemon routes through.
    _no_discovery(monkeypatch)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))
    _write_agents_registry(tmp_path / "agents", [
        {"short_id": "phasesta", "provider": "codex", "host_mode": "interactive",
         "status": "live", "name": "phasestall", "pid": 1234},
    ])
    idx = reg.index(path=tmp_path / "registry.json")
    assert "phasesta" in idx
    e = idx["phasesta"]
    assert e.provider == "codex"
    assert e.inject_handle == "worker:phasesta"
    assert e.name == "phasestall"
    assert e.pid == 1234


def test_index_bridge_is_provider_generic(tmp_path, monkeypatch):
    # AC4-FR: the bridge is not codex-specific -- a gemini (or any non-claude)
    # interactive worker surfaces the same way, with ZERO bridge code per harness.
    _no_discovery(monkeypatch)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))
    _write_agents_registry(tmp_path / "agents", [
        {"short_id": "gemwork1", "provider": "gemini", "host_mode": "interactive",
         "status": "live", "name": "gemmy"},
        {"short_id": "shellw", "provider": "shell", "host_mode": "interactive",
         "status": "live", "name": "sh"},
    ])
    idx = reg.index(path=tmp_path / "registry.json")
    assert idx["gemwork1"].inject_handle == "worker:gemwork1"
    assert idx["shellw"].provider == "shell"  # an open-set harness surfaces too


def test_index_bridge_skips_claude_dead_exec_and_unsafe(tmp_path, monkeypatch):
    _no_discovery(monkeypatch)
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))
    _write_agents_registry(tmp_path / "agents", [
        # claude rides the discover lane, not this bridge
        {"short_id": "clw", "provider": "claude", "host_mode": "interactive", "status": "live"},
        # dead worker holds no live PTY
        {"short_id": "deadw", "provider": "codex", "host_mode": "interactive", "status": "exited"},
        # exec (-p / dispatch) is not a worker.submit target
        {"short_id": "execw", "provider": "codex", "host_mode": "exec", "status": "live"},
        # unsafe short_id (socket path segment) must never surface
        {"short_id": "../../etc", "provider": "codex", "host_mode": "interactive", "status": "live"},
        # the one good row
        {"short_id": "okw", "provider": "codex", "host_mode": "interactive", "status": "live"},
    ])
    idx = reg.index(path=tmp_path / "registry.json")
    assert set(idx) == {"okw"}


def test_index_bridge_tolerates_missing_or_corrupt_agents_registry(tmp_path, monkeypatch):
    # A missing / corrupt agents registry must never deny lookup -- the bridge yields
    # nothing and the discovered/persisted sources still populate the index.
    monkeypatch.setattr(reg, "discover_live_sessions", lambda: [
        DiscoveredSession(session_id="live-1", short_id="d", handle="a", pid=9,
                          cwd="/t", project=None, status="idle", agent="claude"),
    ])
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))  # no registry.json written
    idx = reg.index(path=tmp_path / "registry.json")
    assert "live-1" in idx  # discovery still works
    # corrupt (valid JSON, not an object)
    (tmp_path / "agents").mkdir(parents=True, exist_ok=True)
    (tmp_path / "agents" / "registry.json").write_text("[]")
    assert "live-1" in reg.index(path=tmp_path / "registry.json")
