"""Tests for fno.agents.discover — P1 live-session discovery.

Covers AC1-HP (legible handle + status), AC1-ERR (malformed skipped),
AC1-EDGE (sync-conflict + transcripts ignored, strict pattern), AC1-EDGE2
(worktree -> parent repo; retired stale alias), AC1-FR (vanished mid-scan is
not-live), AC5-FR (all-malformed -> zero rows, no crash).

Liveness is controlled via a fake psutil so the suite is deterministic and
host-independent.
"""
from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from fno.agents import discover
from fno.paths_testing import use_tmpdir


class _FakeProc:
    def __init__(self, create_time: float):
        self._ct = create_time

    def create_time(self) -> float:
        return self._ct


class _FakePsutil:
    """Minimal psutil stand-in: only the PIDs in ``alive`` exist."""

    class NoSuchProcess(Exception):
        pass

    def __init__(self, alive: dict[int, float]):
        self._alive = alive

    def Process(self, pid: int):  # noqa: N802 (mirror psutil API)
        if pid not in self._alive:
            raise self.NoSuchProcess(pid)
        return _FakeProc(self._alive[pid])


def _write_session(
    sdir: Path,
    pid: int,
    *,
    session_id: str,
    job_id: str,
    cwd: str,
    status: str = "idle",
    proc_start: str | None = None,
    create_time: float | None = None,
) -> float:
    """Write a <pid>.json registry file; return its create_time epoch.

    ``proc_start`` defaults to the ctime string of ``create_time`` so the
    reuse-safe liveness check matches by construction.
    """
    ct = create_time if create_time is not None else time.time() - 30
    sdir.mkdir(parents=True, exist_ok=True)
    payload = {
        "pid": pid,
        "sessionId": session_id,
        "cwd": cwd,
        "procStart": proc_start if proc_start is not None else time.ctime(ct),
        "kind": "bg",
        "name": job_id,
        "jobId": job_id,
        "agent": "claude",
        "status": status,
    }
    (sdir / f"{pid}.json").write_text(json.dumps(payload), encoding="utf-8")
    return ct


def _run(sdir, alive, tmp_path, **kw):
    return discover.discover_live_sessions(
        sessions_dir=sdir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive),
        project_resolver=kw.pop("project_resolver", lambda c: None),
        **kw,
    )


def test_ac1_hp_three_live_sessions(tmp_path, monkeypatch):
    """AC1-HP: each live session appears with a legible handle + status."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct1 = _write_session(sdir, 101, session_id="uuid-target", job_id="aaaa1111",
                          cwd="/Users/x/code/proj", status="busy")
    ct2 = _write_session(sdir, 102, session_id="uuid-think", job_id="bbbb2222",
                          cwd="/Users/x/code/proj", status="idle")
    ct3 = _write_session(sdir, 103, session_id="uuid-plain", job_id="cccc3333",
                          cwd="/Users/x/code/proj", status="waiting")
    sessions = _run(sdir, {101: ct1, 102: ct2, 103: ct3}, tmp_path)
    assert len(sessions) == 3
    by_short = {s.short_id: s for s in sessions}
    assert set(by_short) == {"aaaa1111", "bbbb2222", "cccc3333"}
    assert by_short["aaaa1111"].status == "busy"
    # Handle is legible (an alias), not a raw UUID.
    for s in sessions:
        assert s.handle
        assert "uuid-" not in s.handle


def test_ac1_err_malformed_skipped(tmp_path, monkeypatch):
    """AC1-ERR: a malformed <pid>.json is skipped; the rest list normally."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct = _write_session(sdir, 201, session_id="uuid-ok", job_id="ok111111",
                        cwd="/Users/x/code/proj")
    sdir.mkdir(parents=True, exist_ok=True)
    (sdir / "202.json").write_text("{ this is not json", encoding="utf-8")
    sessions = _run(sdir, {201: ct, 202: time.time()}, tmp_path)
    assert [s.short_id for s in sessions] == ["ok111111"]


def test_ac1_edge_strict_pattern_and_sync_conflicts(tmp_path, monkeypatch):
    """AC1-EDGE: only ^\\d+\\.json$ live rows; sync-conflicts + .md ignored."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct = _write_session(sdir, 301, session_id="uuid-real", job_id="real1234",
                        cwd="/Users/x/code/proj")
    sdir.mkdir(parents=True, exist_ok=True)
    # noise files that must never be parsed
    (sdir / "301.sync-conflict-20260609.json").write_text(
        json.dumps({"pid": 999, "sessionId": "uuid-conflict", "jobId": "noise",
                    "procStart": time.ctime()}),
        encoding="utf-8",
    )
    (sdir / "abc-def-transcript.md").write_text("# transcript", encoding="utf-8")
    (sdir / "notapid.json").write_text("{}", encoding="utf-8")
    sessions = _run(sdir, {301: ct, 999: time.time()}, tmp_path)
    assert [s.short_id for s in sessions] == ["real1234"]


def test_ac1_fr_vanished_or_dead_pid_not_live(tmp_path, monkeypatch):
    """AC1-FR: a session whose PID is no longer running is dropped, not phantom."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct = _write_session(sdir, 401, session_id="uuid-dead", job_id="dead0000",
                        cwd="/Users/x/code/proj")
    # 401 is NOT in the alive map -> dead.
    sessions = _run(sdir, {}, tmp_path)
    assert sessions == []
    # And a live one alongside survives.
    ct2 = _write_session(sdir, 402, session_id="uuid-live", job_id="live0000",
                         cwd="/Users/x/code/proj")
    sessions = _run(sdir, {402: ct2}, tmp_path)
    assert [s.short_id for s in sessions] == ["live0000"]


def test_pid_reuse_rejected_by_proc_start_mismatch(tmp_path, monkeypatch):
    """A reused PID (create-time mismatch) is not shown live (reuse guard)."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    # File records procStart for an OLD create time; the PID is alive now but
    # with a DIFFERENT (newer) create time -> reused -> not live.
    old_ct = time.time() - 10_000
    _write_session(sdir, 501, session_id="uuid-reused", job_id="reuse000",
                   cwd="/Users/x/code/proj", proc_start=time.ctime(old_ct))
    new_ct = time.time() - 5  # different process now holds pid 501
    sessions = _run(sdir, {501: new_ct}, tmp_path)
    assert sessions == []


def test_utc_proc_start_matches_live(tmp_path, monkeypatch):
    """CC renders procStart in UTC (verified 2.1.169); a UTC string is live."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct = time.time() - 42
    # procStart written as UTC asctime, like the real registry.
    _write_session(sdir, 555, session_id="uuid-utc", job_id="utc00001",
                   cwd="/Users/x/code/proj", proc_start=time.asctime(time.gmtime(ct)),
                   create_time=ct)
    sessions = _run(sdir, {555: ct}, tmp_path)
    assert [s.short_id for s in sessions] == ["utc00001"]


def test_ac5_fr_all_malformed_zero_rows(tmp_path, monkeypatch):
    """AC5-FR: every <pid>.json malformed -> zero discovered rows, no crash."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    sdir.mkdir(parents=True, exist_ok=True)
    for pid in (601, 602, 603):
        (sdir / f"{pid}.json").write_text("garbage}{", encoding="utf-8")
    sessions = _run(sdir, {601: 1.0, 602: 2.0, 603: 3.0}, tmp_path)
    assert sessions == []


def test_absent_sessions_dir_is_empty(tmp_path, monkeypatch):
    """Boundary: an absent ~/.claude/sessions dir lists zero, never errors."""
    use_tmpdir(monkeypatch, tmp_path)
    sessions = _run(tmp_path / "nope", {}, tmp_path)
    assert sessions == []


def test_dedup_on_session_id_not_pid(tmp_path, monkeypatch):
    """Invariant: two live files sharing a sessionId yield one row."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct1 = _write_session(sdir, 701, session_id="uuid-same", job_id="same0001",
                         cwd="/Users/x/code/proj")
    ct2 = _write_session(sdir, 702, session_id="uuid-same", job_id="same0002",
                         cwd="/Users/x/code/proj")
    sessions = _run(sdir, {701: ct1, 702: ct2}, tmp_path)
    assert len(sessions) == 1


def test_exclude_registered_short_ids(tmp_path, monkeypatch):
    """A session already in the fno registry is excluded (no double-listing)."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct1 = _write_session(sdir, 801, session_id="uuid-adopted", job_id="adopted1",
                         cwd="/Users/x/code/proj")
    ct2 = _write_session(sdir, 802, session_id="uuid-free", job_id="free0001",
                         cwd="/Users/x/code/proj")
    sessions = _run(sdir, {801: ct1, 802: ct2}, tmp_path,
                    exclude_short_ids={"adopted1"})
    assert [s.short_id for s in sessions] == ["free0001"]


def test_ac1_edge2_alias_stable_and_retires_dead(tmp_path, monkeypatch):
    """AC1-EDGE2: alias is stable across calls; a dead session retires its alias
    on the next scan that still sees a live session."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    name_map = tmp_path / ".fno" / "session-names.json"
    ct1 = _write_session(sdir, 901, session_id="uuid-keep", job_id="keep0001",
                         cwd="/Users/x/code/proj")
    ct2 = _write_session(sdir, 902, session_id="uuid-stay", job_id="stay0001",
                         cwd="/Users/x/code/proj")

    def run(alive):
        return discover.discover_live_sessions(
            sessions_dir=sdir, name_map_path=name_map,
            psutil_mod=_FakePsutil(alive), project_resolver=lambda c: "proj",
        )

    first = {s.short_id: s.handle for s in run({901: ct1, 902: ct2})}
    assert first["keep0001"] == "proj-keep0001"
    # Same sessions, second call -> identical alias (stable).
    second = {s.short_id: s.handle for s in run({901: ct1, 902: ct2})}
    assert second["keep0001"] == first["keep0001"]
    # 901 exits while 902 stays live -> 901's alias retired from the map.
    run({902: ct2})
    stored = json.loads(name_map.read_text(encoding="utf-8"))
    assert "uuid-keep" not in stored
    assert "uuid-stay" in stored


def test_empty_scan_preserves_alias_map(tmp_path, monkeypatch):
    """A transient all-empty scan must NOT wipe the persisted alias map (P2 fix);
    only a non-empty scan prunes dead entries."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    name_map = tmp_path / ".fno" / "session-names.json"
    ct = _write_session(sdir, 911, session_id="uuid-a", job_id="aaaa0001",
                        cwd="/Users/x/code/proj")

    def run(alive):
        return discover.discover_live_sessions(
            sessions_dir=sdir, name_map_path=name_map,
            psutil_mod=_FakePsutil(alive), project_resolver=lambda c: "proj",
        )

    run({911: ct})
    before = name_map.read_text(encoding="utf-8")
    # All sessions appear dead this scan -> map left intact, not wiped.
    assert run({}) == []
    assert name_map.read_text(encoding="utf-8") == before


def _resolve(handle, sdir, alive, tmp_path):
    return discover.resolve_or_suggest(
        handle,
        sessions_dir=sdir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive),
        project_resolver=lambda c: "proj",
    )


def test_us2_resolve_by_hex_and_alias(tmp_path, monkeypatch):
    """US2: a send handle resolves by hex short-id AND by friendly alias."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct = _write_session(sdir, 321, session_id="uuid-tgt", job_id="tgt00001",
                        cwd="/Users/x/code/proj", status="idle")
    # by hex
    match, sugg = _resolve("tgt00001", sdir, {321: ct}, tmp_path)
    assert match is not None and match.short_id == "tgt00001"
    assert match.project == "proj" and sugg == []
    # by friendly alias
    match2, _ = _resolve("proj-tgt00001", sdir, {321: ct}, tmp_path)
    assert match2 is not None and match2.short_id == "tgt00001"


def test_us2_resolve_unknown_returns_suggestions(tmp_path, monkeypatch):
    """AC2-ERR: an unknown handle resolves to None + closest live handles."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct = _write_session(sdir, 654, session_id="uuid-think", job_id="think001",
                        cwd="/Users/x/code/proj")
    match, sugg = _resolve("think00X", sdir, {654: ct}, tmp_path)
    assert match is None
    # The close match is the alias or the hex, both carry "think001".
    assert any("think001" in s for s in sugg)


def test_ac1_edge2_worktree_resolves_to_parent_repo(tmp_path, monkeypatch):
    """AC1-EDGE2: a .claude/worktrees cwd attributes to the parent repo project."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    # Real resolver, but stub detect_project_from_settings to map the root.
    import fno.graph._intake as intake

    def fake_detect(cwd_path=None):
        if cwd_path and cwd_path.rstrip("/").endswith("/code/me/abilities"):
            return "fno"
        return None

    monkeypatch.setattr(intake, "detect_project_from_settings", fake_detect)
    ct = _write_session(
        sdir, 111, session_id="uuid-wt", job_id="wt000001",
        cwd="/Users/x/code/me/abilities/.claude/worktrees/feat-x",
    )
    sessions = discover.discover_live_sessions(
        sessions_dir=sdir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil({111: ct}),
    )
    assert sessions[0].project == "fno"
