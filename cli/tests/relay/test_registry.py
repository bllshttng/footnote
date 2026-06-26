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
