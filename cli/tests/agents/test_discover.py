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
import subprocess
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


def test_dead_pid_is_enumerated_but_family1_decides_routing(tmp_path, monkeypatch):
    """A stale process sidecar remains an identity candidate, never a verdict."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    _write_session(sdir, 401, session_id="uuid-dead", job_id="dead0000",
                   cwd="/Users/x/code/proj")
    # 401 is NOT in the alive map -> dead.
    sessions = _run(sdir, {}, tmp_path)
    assert [s.short_id for s in sessions] == ["dead0000"]
    assert sessions[0].is_alive is False
    resolved, _ = discover.resolve_or_suggest(
        "dead0000", sessions_dir=sdir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil({}), project_resolver=lambda _cwd: None,
        truth_fn=lambda _session: {"state": "working"},
    )
    assert resolved is not None
    # And a live one alongside survives.
    ct2 = _write_session(sdir, 402, session_id="uuid-live", job_id="live0000",
                         cwd="/Users/x/code/proj")
    sessions = _run(sdir, {402: ct2}, tmp_path)
    assert [s.short_id for s in sessions] == ["dead0000", "live0000"]


def test_pid_reuse_rejected_by_proc_start_mismatch(tmp_path, monkeypatch):
    """A reused PID remains only an unclassified enumeration candidate."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    # File records procStart for an OLD create time; the PID is alive now but
    # with a DIFFERENT (newer) create time -> reused -> not live.
    old_ct = time.time() - 10_000
    _write_session(sdir, 501, session_id="uuid-reused", job_id="reuse000",
                   cwd="/Users/x/code/proj", proc_start=time.ctime(old_ct))
    new_ct = time.time() - 5  # different process now holds pid 501
    sessions = _run(sdir, {501: new_ct}, tmp_path)
    assert [s.short_id for s in sessions] == ["reuse000"]
    assert sessions[0].is_alive is False


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


def test_ac1_edge2_alias_stable_and_pid_miss_does_not_retire(tmp_path, monkeypatch):
    """A process miss cannot retire identity without family-1 death."""
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
    # A PID miss alone preserves both identities.
    run({902: ct2})
    stored = json.loads(name_map.read_text(encoding="utf-8"))
    assert "uuid-keep" in stored
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
    # A PID-empty scan yields unclassified candidates and leaves aliases intact.
    assert all(not session.is_alive for session in run({}))
    assert name_map.read_text(encoding="utf-8") == before


def _resolve(handle, sdir, alive, tmp_path):
    return discover.resolve_or_suggest(
        handle,
        sessions_dir=sdir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive),
        project_resolver=lambda c: "proj",
        truth_fn=lambda _session: {"state": "working"},
    )


def test_unclassified_discovered_session_is_not_alive():
    session = discover.DiscoveredSession(
        session_id="feedface", short_id="feedface", handle="feedface",
        pid=0, cwd="/tmp", project=None, status=None,
    )
    assert session.truth_state == "unknown"
    assert session.is_alive is False


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
        if cwd_path and cwd_path.rstrip("/").endswith("/code/me/fno"):
            return "fno"
        return None

    monkeypatch.setattr(intake, "detect_project_from_settings", fake_detect)
    ct = _write_session(
        sdir, 111, session_id="uuid-wt", job_id="wt000001",
        cwd="/Users/x/code/me/fno/.claude/worktrees/feat-x",
    )
    sessions = discover.discover_live_sessions(
        sessions_dir=sdir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil({111: ct}),
    )
    assert sessions[0].project == "fno"


# ---------------------------------------------------------------------------
# x-a1d5: canonical transcript-store fallback (~/.claude/projects)
#
# The sidecar is gone; liveness comes from a running ``claude`` process whose
# cwd maps to a projects subdir. A fake psutil supplies both process_iter (for
# the projects fallback) and create_time (for any sidecar row).
# ---------------------------------------------------------------------------

import os  # noqa: E402
import re  # noqa: E402


def _enc(cwd: str) -> str:
    return re.sub(r"[^a-zA-Z0-9]", "-", cwd)


class _FakeProcsProc:
    def __init__(self, cwd=None, create_time=None):
        self._cwd = cwd
        self._ct = create_time

    def cwd(self):
        if self._cwd is None:
            raise _FakeProcsPsutil.NoSuchProcess(0)
        return self._cwd

    def create_time(self):
        if self._ct is None:
            raise _FakeProcsPsutil.NoSuchProcess(0)
        return self._ct


class _IterItem:
    def __init__(self, pid, cmdline):
        self.info = {"pid": pid, "cmdline": cmdline}


class _FakeProcsPsutil:
    """psutil stand-in supporting process_iter + Process(pid).cwd()/create_time().

    ``procs`` maps pid -> (cmdline, cwd); ``alive`` maps pid -> create_time for
    any sidecar liveness check.
    """

    class NoSuchProcess(Exception):
        pass

    def __init__(self, procs=None, alive=None):
        self._procs = procs or {}
        self._alive = alive or {}

    def process_iter(self, _fields=None):
        return [_IterItem(pid, cmd) for pid, (cmd, _cwd) in self._procs.items()]

    def Process(self, pid):  # noqa: N802 (mirror psutil API)
        cwd = self._procs.get(pid, (None, None))[1]
        ct = self._alive.get(pid)
        if cwd is None and ct is None and pid not in self._procs:
            raise self.NoSuchProcess(pid)
        return _FakeProcsProc(cwd=cwd, create_time=ct)


def _write_transcript(projects_dir, *, cwd, session_id, mtime_age=5.0, body_cwd=True):
    pdir = projects_dir / _enc(cwd)
    pdir.mkdir(parents=True, exist_ok=True)
    f = pdir / f"{session_id}.jsonl"
    line = {"type": "user", "sessionId": session_id}
    if body_cwd:
        line["cwd"] = cwd
    f.write_text(json.dumps(line) + "\n", encoding="utf-8")
    mt = time.time() - mtime_age
    os.utime(f, (mt, mt))
    return f


def _claude_proc(pid, cwd, *, session_id=None):
    cmd = ["claude"]
    if session_id:
        cmd += ["--session-id", session_id]
    return pid, (cmd, cwd)


def _run_projects(tmp_path, projects_dir, procs, **kw):
    """Discover with an absent sidecar so the projects fallback fires."""
    pmap = dict(procs)
    return discover.discover_live_sessions(
        sessions_dir=tmp_path / "no-sessions-here",
        projects_dir=projects_dir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakeProcsPsutil(procs=pmap),
        project_resolver=kw.pop("project_resolver", lambda c: None),
        **kw,
    )


def test_x_a1d5_fallback_surfaces_live_session(tmp_path, monkeypatch):
    """AC1: with no sidecar, a live claude proc's transcript still surfaces."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    sid = "02a5c8bc-c83c-4bb0-a473-19f85d0f3671"
    _write_transcript(projects, cwd="/Users/x/code/proj", session_id=sid)
    procs = [_claude_proc(4242, "/Users/x/code/proj", session_id=sid)]
    sessions = _run_projects(tmp_path, projects, procs)
    assert len(sessions) == 1
    s = sessions[0]
    assert s.session_id == sid
    assert s.short_id == "02a5c8bc"  # session_id[:8], the addressable handle
    assert s.cwd == "/Users/x/code/proj"  # from the live process
    assert s.pid == 4242  # real pid from the running claude
    assert s.agent == "claude"


def test_x_a1d5_session_id_from_newest_transcript_not_argv(tmp_path, monkeypatch):
    """Live id == newest transcript filename, even when argv --session-id differs.

    Proven on the real host: a claude ran with --session-id X but its on-disk
    transcript was a different id. The filename is canonical, argv is not.
    """
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    _write_transcript(projects, cwd="/Users/x/code/proj",
                      session_id="real-transcript-id", mtime_age=2)
    procs = [_claude_proc(4242, "/Users/x/code/proj", session_id="argv-only-id")]
    sessions = _run_projects(tmp_path, projects, procs)
    assert [s.session_id for s in sessions] == ["real-transcript-id"]


def test_x_a1d5_sidecar_and_projects_candidates_are_unioned(tmp_path, monkeypatch):
    """A stale sidecar candidate cannot hide a projects-only live session."""
    use_tmpdir(monkeypatch, tmp_path)
    sdir = tmp_path / "sessions"
    ct = _write_session(sdir, 770, session_id="uuid-sidecar", job_id="side0001",
                        cwd="/Users/x/code/proj")
    projects = tmp_path / "projects"
    _write_transcript(projects, cwd="/Users/x/code/other", session_id="ghost-sid")
    sessions = discover.discover_live_sessions(
        sessions_dir=sdir,
        projects_dir=projects,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakeProcsPsutil(
            procs={999: (["claude"], "/Users/x/code/other")},
            alive={770: ct},
        ),
        project_resolver=lambda c: None,
    )
    assert {s.short_id for s in sessions} == {"side0001", "ghost-si"}


def test_x_a1d5_stale_transcript_not_surfaced(tmp_path, monkeypatch):
    """Transcript age alone is neither discovery exclusion nor proof of death."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    _write_transcript(projects, cwd="/Users/x/code/proj", session_id="stale-sid",
                      mtime_age=discover._DEFAULT_RECENCY_SECONDS + 120)
    procs = [_claude_proc(4242, "/Users/x/code/proj")]
    sessions = _run_projects(tmp_path, projects, procs)
    assert [s.session_id for s in sessions] == ["stale-sid"]
    assert sessions[0].truth_state == "unknown"


def test_x_a1d5_noise_ignored(tmp_path, monkeypatch):
    """sync-conflict copies, non-jsonl files, and subdirs never become sessions."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    real = _write_transcript(projects, cwd="/Users/x/code/proj",
                             session_id="real-sid", mtime_age=20)
    pdir = real.parent
    # A FRESHER sync-conflict copy must still lose to name-based skipping.
    conflict = pdir / "real-sid.sync-conflict-20260626.jsonl"
    conflict.write_text(json.dumps({"sessionId": "conflict"}) + "\n", encoding="utf-8")
    (pdir / "summary.md").write_text("# notes", encoding="utf-8")
    (pdir / "tool-results").mkdir()  # UUID subdir analog, never a session
    procs = [_claude_proc(4242, "/Users/x/code/proj")]
    sessions = _run_projects(tmp_path, projects, procs)
    assert [s.session_id for s in sessions] == ["real-sid"]


def test_x_a1d5_no_dir_for_live_cwd_yields_nothing(tmp_path, monkeypatch):
    """A live claude with no projects dir yet (brand-new) surfaces no row."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    projects.mkdir()
    procs = [_claude_proc(4242, "/Users/x/code/brand-new")]
    assert _run_projects(tmp_path, projects, procs) == []


def test_x_a1d5_bg_infra_proc_excluded(tmp_path, monkeypatch):
    """A --bg-pty-host helper shares the claude binary but is not a session."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    _write_transcript(projects, cwd="/Users/x/code/proj", session_id="real-sid")
    procs = {
        16637: (
            ["/Users/x/.local/share/claude/versions/2.1.193", "--bg-pty-host",
             "/tmp/x.sock"],
            "/Users/x/code/proj",
        ),
    }
    assert _run_projects(tmp_path, projects, list(procs.items())) == []


def test_x_a1d5_resolve_adopts_projects_session(tmp_path, monkeypatch):
    """AC2: a projects-only session is addressable (resolve by its short-id)."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    sid = "8255f76d-75be-4a5f-bfc1-39a710fb8a01"
    _write_transcript(projects, cwd="/Users/x/code/proj", session_id=sid)
    match, _ = discover.resolve_or_suggest(
        "8255f76d",
        sessions_dir=tmp_path / "no-sessions-here",
        projects_dir=projects,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakeProcsPsutil(procs={4242: (["claude"], "/Users/x/code/proj")}),
        project_resolver=lambda c: None,
        truth_fn=lambda _session: {"state": "working"},
    )
    assert match is not None
    assert match.session_id == sid


def test_x_a1d5_underscore_cwd_dir_preserved(tmp_path, monkeypatch):
    """Codex P2: a CC build that preserves '_' in the dir name still resolves.

    The transcript lives under the underscore-PRESERVING dir; discovery must try
    both encodings and find it (the underscore-collapsing form would miss).
    """
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    cwd = "/Users/x/code/my_app"
    # Seed under the underscore-preserving dir name only.
    pdir = projects / re.sub(r"[^a-zA-Z0-9_]", "-", cwd)
    pdir.mkdir(parents=True, exist_ok=True)
    f = pdir / "us-sid.jsonl"
    f.write_text(json.dumps({"sessionId": "us-sid"}) + "\n", encoding="utf-8")
    procs = [_claude_proc(4242, cwd)]
    sessions = _run_projects(tmp_path, projects, procs)
    assert [s.session_id for s in sessions] == ["us-sid"]


def test_x_a1d5_exclude_adopted_session_by_full_id(tmp_path, monkeypatch):
    """Codex P2: an adopted (registered) session is not re-listed by projects/."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    sid = "adopted-session-uuid"
    _write_transcript(projects, cwd="/Users/x/code/proj", session_id=sid)
    procs = [_claude_proc(4242, "/Users/x/code/proj")]
    sessions = _run_projects(tmp_path, projects, procs, exclude_session_ids={sid})
    assert sessions == []


# --------------------------------------------------------------------------
# US2/US4: codex disk-discovery — a hand-started codex session is
# mail-able even without the registry hook. US3: cross-harness resolution.
# --------------------------------------------------------------------------


def _write_codex_rollout(codex_dir, *, session_id, cwd, mtime_age=5.0, meta=True):
    """Write a rollout jsonl whose first line is a session_meta record."""
    day = codex_dir / "2026" / "07" / "09"
    day.mkdir(parents=True, exist_ok=True)
    f = day / f"rollout-2026-07-09T00-00-00-{session_id}.jsonl"
    if meta:
        line = {"type": "session_meta", "payload": {"id": session_id, "cwd": cwd}}
    else:
        line = {"type": "turn_context", "payload": {"nope": 1}}
    f.write_text(json.dumps(line) + "\n", encoding="utf-8")
    mt = time.time() - mtime_age
    os.utime(f, (mt, mt))
    return f


def _run_codex(tmp_path, codex_dir, **kw):
    """Discover with empty claude stores + fake psutil so only codex rows surface."""
    return discover.discover_live_sessions(
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex_dir,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=kw.pop("project_resolver", lambda c: None),
        **kw,
    )


def test_us2_codex_rollout_surfaces_live_session(tmp_path):
    codex = tmp_path / "codex"
    _write_codex_rollout(
        codex, session_id="019f48e1-5b09-72a0-9bc8-6b364bcf4ae4",
        cwd="/Users/x/proj", mtime_age=5.0,
    )
    sessions = _run_codex(tmp_path, codex)
    assert len(sessions) == 1
    s = sessions[0]
    assert s.agent == "codex"
    assert s.session_id == "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    assert s.short_id == "019f48e1"
    assert s.cwd == "/Users/x/proj"


def test_us2_codex_old_watching_rollout_still_surfaces(tmp_path):
    codex = tmp_path / "codex"
    rollout = _write_codex_rollout(
        codex, session_id="019f48e1-dead", cwd="/x", mtime_age=10_000.0,
    )
    with rollout.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps({
            "type": "response_item",
            "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "<watching pr=7>"}],
            },
        }) + "\n")
    stale = time.time() - 10_000.0
    os.utime(rollout, (stale, stale))

    sessions = _run_codex(tmp_path, codex)

    assert [s.session_id for s in sessions] == ["019f48e1-dead"]
    assert sessions[0].truth_state == "watching"
    assert sessions[0].is_alive is True


def test_codex_truth_reuses_discovered_rollout_path(tmp_path, monkeypatch):
    """One discovery scan serves tail content and mtime classification."""
    codex = tmp_path / "codex"
    rollout = _write_codex_rollout(
        codex, session_id="019f48e1-direct", cwd="/x", mtime_age=10_000.0
    )
    with rollout.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps({
            "type": "response_item",
            "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "<watching pr=7>"}],
            },
        }) + "\n")
    from fno.agents import peek

    monkeypatch.setattr(
        peek,
        "_codex_rollout_path",
        lambda *_args, **_kwargs: pytest.fail("rollout store was rescanned"),
    )

    sessions = _run_codex(tmp_path, codex)

    assert sessions[0].truth_state == "watching"
    assert sessions[0].transcript_path == str(rollout)


def test_resolver_only_discovery_does_not_classify_every_candidate(
    tmp_path, monkeypatch
):
    codex = tmp_path / "codex"
    _write_codex_rollout(codex, session_id="019f48e1-target", cwd="/x")

    match, _suggestions = discover.resolve_or_suggest(
        "019f48e1",
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=lambda _cwd: pytest.fail("project metadata was resolved"),
        truth_fn=lambda _session: pytest.fail("candidate was classified"),
        require_alive=False,
    )

    assert match is not None
    assert match.session_id == "019f48e1-target"


def test_resolver_only_full_session_id_uses_bare_fast_path(tmp_path):
    codex = tmp_path / "codex"
    session_id = "019f48e1-target-full-session-id"
    _write_codex_rollout(codex, session_id=session_id, cwd="/x")

    match, _suggestions = discover.resolve_or_suggest(
        session_id,
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=lambda _cwd: pytest.fail("project metadata was resolved"),
        require_alive=False,
    )

    assert match is not None
    assert match.session_id == session_id


def test_resolver_only_registered_id_skips_transcript_store_scan(
    tmp_path, monkeypatch
):
    from fno.agents.registry import AgentEntry, write_registry

    registry = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="registered-codex",
                harness="codex",
                harness_session_id="019f48e1-registered",
                cwd="/x",
                log_path="/tmp/codex.log",
            )
        ],
        path=registry,
    )
    monkeypatch.setattr(
        discover,
        "_discover_from_codex",
        lambda *_args, **_kwargs: pytest.fail("transcript store was scanned"),
    )

    match, _suggestions = discover.resolve_or_suggest(
        "019f48e1",
        registry_path=registry,
        require_alive=False,
    )

    assert match is not None
    assert match.session_id == "019f48e1-registered"
    assert match.agent == "codex"


@pytest.mark.parametrize(
    "truth_state,expected",
    [("working", "live"), ("done", "orphaned"), ("unknown", "unknown")],
)
def test_discovered_row_status_projects_family1_truth(truth_state, expected):
    session = discover.DiscoveredSession(
        session_id="feedface",
        short_id="feedface",
        handle="feedface",
        pid=0,
        cwd="/tmp",
        project=None,
        status="busy",
        truth_state=truth_state,
    )

    assert session.to_row()["status"] == expected


def test_loaded_daemon_thread_does_not_override_stalled_transcript(tmp_path, monkeypatch):
    codex = tmp_path / "codex"
    sid = "019f4d0c-1111-2222-3333-444444444444"
    rollout = _write_codex_rollout(
        codex, session_id=sid, cwd="/old/repo", mtime_age=10_000.0
    )
    with rollout.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps({
            "type": "response_item",
            "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "still working"}],
            },
        }) + "\n")
    stale = time.time() - 10_000.0
    os.utime(rollout, (stale, stale))
    monkeypatch.setattr(
        discover,
        "_discover_from_codex_daemon",
        lambda: [
            {
                "session_id": sid,
                "short_id": sid[:8],
                "pid": 0,
                "cwd": "/live/repo",
                "status": None,
                "agent": "codex",
            }
        ],
    )

    resolved, _ = discover.resolve_or_suggest(
        "019f4d0c",
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=lambda c: None,
    )

    assert resolved is None


def test_daemon_row_is_enriched_by_recent_rollout(tmp_path, monkeypatch):
    codex = tmp_path / "codex"
    sid = "019f4d0c-aaaa-bbbb-cccc-dddddddddddd"
    _write_codex_rollout(codex, session_id=sid, cwd="/rollout/repo")
    monkeypatch.setattr(
        discover,
        "_discover_from_codex_daemon",
        lambda: [
            {
                "session_id": sid,
                "short_id": sid[:8],
                "pid": 0,
                "cwd": "",
                "status": None,
                "agent": "codex",
            }
        ],
    )

    sessions = _run_codex(tmp_path, codex)

    assert len(sessions) == 1
    assert sessions[0].cwd == "/rollout/repo"


@pytest.mark.parametrize(
    "completed",
    [
        subprocess.CompletedProcess([], 1, stdout="", stderr="unsupported"),
        subprocess.CompletedProcess([], 0, stdout="not-json", stderr=""),
        subprocess.CompletedProcess(
            [], 0, stdout='{"available":false,"reason":"no-daemon"}', stderr=""
        ),
        subprocess.CompletedProcess(
            [], 0, stdout='{"available":true,"threads":[]}', stderr=""
        ),
    ],
)
def test_daemon_probe_failure_or_empty_is_lenient(monkeypatch, completed):
    from fno.agents import rust_runtime

    monkeypatch.setattr(
        rust_runtime, "resolve_installed_binary", lambda: Path("/fake/fno-agents")
    )
    monkeypatch.setattr(discover.subprocess, "run", lambda *a, **kw: completed)

    assert discover._discover_from_codex_daemon() == []


def test_daemon_probe_shapes_valid_rows_and_skips_bad_entries(monkeypatch):
    from fno.agents import rust_runtime

    output = {
        "available": True,
        "threads": [
            {"session_id": "short", "cwd": None},
            {"session_id": "short", "cwd": "/duplicate"},
            {"session_id": "019f4d0c-full", "cwd": "/repo"},
            {"session_id": 7, "cwd": "/bad"},
        ],
    }
    monkeypatch.setattr(
        rust_runtime, "resolve_installed_binary", lambda: Path("/fake/fno-agents")
    )
    monkeypatch.setattr(
        discover.subprocess,
        "run",
        lambda *a, **kw: subprocess.CompletedProcess(
            [], 0, stdout=json.dumps(output), stderr=""
        ),
    )

    rows = discover._discover_from_codex_daemon()

    assert [(r["session_id"], r["short_id"], r["cwd"]) for r in rows] == [
        ("short", "short", ""),
        ("019f4d0c-full", "019f4d0c", "/repo"),
    ]


def test_us2_codex_malformed_meta_skipped_not_fatal(tmp_path):
    codex = tmp_path / "codex"
    _write_codex_rollout(codex, session_id="019f48e1-nometa", cwd="/x", meta=False)
    _write_codex_rollout(
        codex, session_id="019abcde-good", cwd="/y", mtime_age=3.0,
    )
    sessions = _run_codex(tmp_path, codex)
    assert [s.short_id for s in sessions] == ["019abcde"]


def test_us3_resolve_bare_short_id_across_harnesses(tmp_path):
    """A codex session answers to the same bare short-id a claude one does - the
    address does not encode the harness. The retired prefixed form is refused,
    and the refusal leads with the bare id so the caller can fix what it built."""
    codex = tmp_path / "codex"
    _write_codex_rollout(
        codex, session_id="019f48e1-5b09-72a0-9bc8-6b364bcf4ae4", cwd="/x",
    )
    seams = dict(
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=lambda c: None,
        truth_fn=lambda _session: {"state": "working"},
    )
    resolved, _ = discover.resolve_or_suggest("019f48e1", **seams)
    assert resolved is not None
    assert resolved.agent == "codex"
    assert resolved.session_id == "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"

    refused, suggestions = discover.resolve_or_suggest("codex-019f48e1", **seams)
    assert refused is None
    assert suggestions[0] == "019f48e1"


def test_retired_shape_refused_even_when_stored_as_friendly_alias(tmp_path):
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    codex = tmp_path / "codex"
    _write_codex_rollout(codex, session_id=sid, cwd="/x")
    name_map = tmp_path / ".fno" / "session-names.json"
    name_map.parent.mkdir(parents=True)
    name_map.write_text(json.dumps({sid: "codex-019f48e1"}), encoding="utf-8")

    resolved, suggestions = discover.resolve_or_suggest(
        "codex-019f48e1",
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex,
        name_map_path=name_map,
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=lambda c: None,
    )

    assert resolved is None
    assert suggestions[0] == "019f48e1"
    assert json.loads(name_map.read_text(encoding="utf-8"))[sid] == "session-019f48e1"


@pytest.mark.parametrize("project", ["claude", "codex", "gemini", "agy", "opencode"])
def test_default_alias_never_generates_retired_handle_shape(project):
    from fno.harness_identity import LEGACY_HANDLE_RE

    alias = discover._default_alias(project, "deadbeef")

    assert not LEGACY_HANDLE_RE.fullmatch(alias)


def test_ac2_edge_harness_prefix_no_longer_disambiguates(tmp_path):
    """The harness prefix used to break an 8-hex collision across harnesses. It
    is not an address any more, so it is refused like any other retired form and
    that disambiguator is gone.

    The collision itself stays unresolved rather than silently mis-delivering to
    whichever row scanned first: two live sessions sharing an 8-hex prefix is
    ~n^2/2^32 across live sessions, and first-match-wins was already the bare-id
    behavior before this change."""
    codex = tmp_path / "codex"
    _write_codex_rollout(codex, session_id="abcd1234-codex-side", cwd="/x")
    projects = tmp_path / "claude-projects"
    _write_transcript(projects, cwd="/Users/y/repo", session_id="abcd1234-cccc-dddd")
    resolved, suggestions = discover.resolve_or_suggest(
        "codex-abcd1234",
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=projects,
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakeProcsPsutil(procs={4242: (["claude"], "/Users/y/repo")}),
        project_resolver=lambda c: None,
    )
    assert resolved is None
    assert suggestions[0] == "abcd1234"


# --------------------------------------------------------------------------
# x-605c: daemon-roster (US1) + fno-agents registry (US2) resolution sources
# --------------------------------------------------------------------------


def _write_roster(daemon_dir: Path, workers: dict[str, dict]) -> None:
    daemon_dir.mkdir(parents=True, exist_ok=True)
    (daemon_dir / "roster.json").write_text(
        json.dumps({"proto": 1, "workers": workers}), encoding="utf-8"
    )


def _empty_seams(tmp_path: Path) -> dict:
    """discover seams that make every disk source contribute zero rows, so a
    test isolates the roster/registry sources under test."""
    return dict(
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=tmp_path / "no-codex",
        opencode_storage_dir=tmp_path / "no-opencode",
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil({}),
        project_resolver=lambda c: None,
        truth_fn=lambda _session: {"state": "working"},
    )


def test_us1_roster_source_resolves_bg_worker(tmp_path, monkeypatch):
    """US1/AC1-HP: a rostered bg worker (no pid-sidecar) resolves by handle."""
    use_tmpdir(monkeypatch, tmp_path)
    daemon = tmp_path / "daemon"
    _write_roster(
        daemon,
        {
            "9a063cd3": {
                "sessionId": "9a063cd3-69d4-415a-ada5-649b0164189c",
                "pid": 4242,
                "cwd": "/Users/x/code/proj",
            }
        },
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    resolved, _ = discover.resolve_or_suggest("9a063cd3", **_empty_seams(tmp_path))
    assert resolved is not None
    assert resolved.agent == "claude"
    assert resolved.short_id == "9a063cd3"
    assert resolved.session_id == "9a063cd3-69d4-415a-ada5-649b0164189c"


def test_us1_torn_roster_yields_zero_rows(tmp_path, monkeypatch):
    """AC2-ERR: a torn roster contributes no rows and never raises."""
    use_tmpdir(monkeypatch, tmp_path)
    daemon = tmp_path / "daemon"
    daemon.mkdir(parents=True, exist_ok=True)
    (daemon / "roster.json").write_text("{not json", encoding="utf-8")
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    resolved, suggestions = discover.resolve_or_suggest(
        "9a063cd3", **_empty_seams(tmp_path)
    )
    assert resolved is None
    assert suggestions == []


def test_us2_registry_handle_resolves(tmp_path, monkeypatch):
    """US2/AC2-HP: a registry row named x-foo resolves by its claude-<short8>."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="x-foo",
                harness="claude",
                cwd="/Users/x/code/proj",
                log_path="/tmp/x-foo.log",
                short_id="9a063cd3",
                harness_session_id="9a063cd3-69d4-415a-ada5-649b0164189c",
            )
        ],
        path=reg,
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "no-daemon"))
    resolved, _ = discover.resolve_or_suggest(
        "9a063cd3", registry_path=reg, **_empty_seams(tmp_path)
    )
    assert resolved is not None
    assert resolved.short_id == "9a063cd3"
    # Resolves without the registered name; the bare short id also works.
    by_short, _ = discover.resolve_or_suggest(
        "9a063cd3", registry_path=reg, **_empty_seams(tmp_path)
    )
    assert by_short is not None


def test_ac1_edge_source_overlap_dedups(tmp_path, monkeypatch):
    """AC1-EDGE: one session present in registry AND roster yields exactly one row."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    daemon = tmp_path / "daemon"
    _write_roster(daemon, {"9a063cd3": {"sessionId": sid, "pid": 4242, "cwd": "/x"}})
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="x-foo",
                harness="claude",
                cwd="/x",
                log_path="/tmp/x.log",
                short_id="9a063cd3",
                harness_session_id=sid,
            )
        ],
        path=reg,
    )
    sessions = discover.discover_live_sessions(registry_path=reg, **_empty_seams(tmp_path))
    assert [s.session_id for s in sessions] == [sid]


def test_opencode_row_without_captured_id_yields_no_live_recipient(tmp_path, monkeypatch):
    """AC3-EDGE: opencode joining HARNESS_SESSION_ID_FIELDS must not widen discovery.

    The harness gate now admits opencode rows, so an id-less one (backfill
    missed, or a pane spawned before capture existed) reaches the `if not sid`
    guard instead of the harness guard. It must still contribute nothing, or
    mail would queue to a handle nobody drains.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="oc-live-only", harness="opencode", cwd="/x",
                log_path="/tmp/oc.log", harness_session_id=None,
            )
        ],
        path=reg,
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "no-daemon"))
    assert discover.discover_live_sessions(registry_path=reg, **_empty_seams(tmp_path)) == []


def test_opencode_row_with_captured_id_resolves_live(tmp_path, monkeypatch):
    """AC3-EDGE counterpart: a captured ses_ id DOES make the row addressable.

    This is the half-wire hazard the node names: admitting opencode to the
    harness map resolves such a row as live, so the store probe must ship in
    the same change to tell a live pane from a dead one.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    sid = "ses_09679f284ffeJv7NdBAoLQLnLZ"
    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="oc", harness="opencode", cwd="/x",
                log_path="/tmp/oc.log", harness_session_id=sid,
            )
        ],
        path=reg,
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "no-daemon"))
    sessions = discover.discover_live_sessions(registry_path=reg, **_empty_seams(tmp_path))
    assert [s.session_id for s in sessions] == [sid]


def test_us2_registry_dead_status_rows_excluded(tmp_path, monkeypatch):
    """codex review: an orphaned/exited registry row must NOT resolve as live."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="x-dead", harness="claude", cwd="/x", log_path="/tmp/d.log",
                short_id="deadd00d", harness_session_id="deadd00d-1111-2222-3333-444444444444",
                status="orphaned",
            )
        ],
        path=reg,
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "no-daemon"))
    resolved, _ = discover.resolve_or_suggest(
        "claude-deadd00d", registry_path=reg, **_empty_seams(tmp_path)
    )
    assert resolved is None


def test_registry_orphaned_status_cannot_hide_family1_live_session(tmp_path, monkeypatch):
    """A stale registry verdict yields to transcript truth on every read caller."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    sid = "feedface-1111-2222-3333-444444444444"
    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="x-live", harness="claude", cwd="/x", log_path="/tmp/live.log",
                short_id="feedface", harness_session_id=sid, status="orphaned",
            )
        ],
        path=reg,
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "no-daemon"))

    resolved, _ = discover.resolve_or_suggest(
        "feedface",
        registry_path=reg,
        **_empty_seams(tmp_path),
    )

    assert resolved is not None
    assert resolved.truth_state == "working"


def test_us2_registry_short_id_is_jobid_not_uuid_prefix(tmp_path, monkeypatch):
    """codex review: short_id must be the authoritative jobId,
    not the uuid's first 8 hex, when the two differ."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="x-foo", harness="claude", cwd="/x", log_path="/tmp/f.log",
                short_id="j0b1d001",  # jobId
                harness_session_id="aaaabbbb-1111-2222-3333-444444444444",  # uuid[:8]=aaaabbbb
            )
        ],
        path=reg,
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "no-daemon"))
    # Resolves by the jobId short handle...
    by_job, _ = discover.resolve_or_suggest(
        "j0b1d001", registry_path=reg, **_empty_seams(tmp_path)
    )
    assert by_job is not None
    assert by_job.short_id == "j0b1d001"
    assert by_job.session_id == "aaaabbbb-1111-2222-3333-444444444444"
    # ...and by the canonical handle derived from the uuid, which differs from
    # the jobId short_id, so this exercises the derived-handle branch on its own.
    by_canon, _ = discover.resolve_or_suggest(
        "aaaabbbb", registry_path=reg, **_empty_seams(tmp_path)
    )
    assert by_canon is not None


# --------------------------------------------------------------------------
# US6 — opencode disk discovery
# --------------------------------------------------------------------------


def _write_opencode_session(
    storage: Path,
    *,
    session_id: str,
    cwd: str,
    mtime_age: float,
    project_id: str = "proj0001",
    messages: int = 1,
    info: bool = True,
) -> Path:
    """Build a storage tree matching a real opencode install (1.0.223).

    Layout verified on disk: session info at ``session/<projectID>/<ses>.json``
    with the cwd under ``directory``; messages at ``message/<ses>/<msg>.json``.
    """
    sdir = storage / "session" / project_id
    sdir.mkdir(parents=True, exist_ok=True)
    f = sdir / f"{session_id}.json"
    body = (
        {"id": session_id, "directory": cwd, "time": {"created": 1, "updated": 2}}
        if info
        else {"no": "id"}
    )
    f.write_text(json.dumps(body), encoding="utf-8")
    mt = time.time() - mtime_age
    mdir = storage / "message" / session_id
    if messages:
        mdir.mkdir(parents=True, exist_ok=True)
        for i in range(messages):
            m = mdir / f"msg_{i}.json"
            m.write_text(
                json.dumps({"id": f"msg_{i}", "role": "user", "time": {"created": i}}),
                encoding="utf-8",
            )
            os.utime(m, (mt, mt))
        os.utime(mdir, (mt, mt))
    os.utime(f, (mt, mt))
    return f


def _run_opencode(tmp_path, storage, **kw):
    """Discover with every other source empty so only opencode rows surface."""
    return discover.discover_live_sessions(
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=tmp_path / "no-codex",
        opencode_storage_dir=storage,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=kw.pop("project_resolver", lambda c: None),
        **kw,
    )


def test_us6_opencode_session_surfaces_live(tmp_path):
    """AC-HP: a session touched inside the recency window is discovered."""
    storage = tmp_path / "opencode"
    sid = "ses_47ba2e9d1ffel6XimfURzbam25"
    _write_opencode_session(storage, session_id=sid, cwd="/Users/x/proj", mtime_age=5.0)
    sessions = _run_opencode(tmp_path, storage)
    assert len(sessions) == 1
    s = sessions[0]
    assert s.agent == "opencode"
    assert s.session_id == sid
    assert s.short_id == sid[:8]
    assert s.cwd == "/Users/x/proj"  # from `directory`, not `cwd`
    assert s.pid == 0  # no OS handle, mirroring the codex lane


def test_us6_opencode_stale_session_is_enumerated_for_family1(tmp_path):
    """Age is classification evidence, never an enumeration exclusion."""
    storage = tmp_path / "opencode"
    _write_opencode_session(
        storage, session_id="ses_dead", cwd="/x", mtime_age=10_000.0
    )
    sessions = _run_opencode(tmp_path, storage)
    assert [s.session_id for s in sessions] == ["ses_dead"]
    assert sessions[0].truth_state == "unknown"


def test_us6_opencode_fresh_messages_keep_stale_info_live(tmp_path):
    """A session mid-turn stays live off its message dir even when the info
    file has not been rewritten (why discovery maxes the two mtimes)."""
    storage = tmp_path / "opencode"
    sid = "ses_talking"
    f = _write_opencode_session(
        storage, session_id=sid, cwd="/x", mtime_age=10_000.0
    )
    mdir = storage / "message" / sid
    fresh = time.time() - 5.0
    os.utime(mdir, (fresh, fresh))
    assert f.stat().st_mtime < fresh  # info file genuinely stale
    sessions = _run_opencode(tmp_path, storage)
    assert [s.session_id for s in sessions] == [sid]


def test_us6_opencode_malformed_info_skipped_not_fatal(tmp_path):
    """AC-ERR: an info file with no ``id`` contributes no row and never raises."""
    storage = tmp_path / "opencode"
    _write_opencode_session(
        storage, session_id="ses_bad", cwd="/x", mtime_age=5.0, info=False
    )
    (storage / "session" / "proj0001" / "torn.json").write_text("{not json", encoding="utf-8")
    assert _run_opencode(tmp_path, storage) == []


def test_us6_opencode_absent_store_contributes_nothing(tmp_path):
    """Zero-effect on a host with no opencode install."""
    assert _run_opencode(tmp_path, tmp_path / "never-installed") == []


def _touch(path: Path, age: float) -> None:
    mt = time.time() - age
    os.utime(path, (mt, mt))


def test_us6_opencode_streaming_turn_stays_live_via_part_mtime(tmp_path):
    """A long turn writes into part/<msg_id>/, which moves neither the session
    info nor the message dir. Without the deeper look such a session ages out
    of discovery and becomes unaddressable while still alive."""
    storage = tmp_path / "opencode"
    sid = "ses_streaming"
    info = _write_opencode_session(
        storage, session_id=sid, cwd="/x", mtime_age=1800.0, messages=1
    )
    # Both cheap signals are stale (well past the 600s window)...
    assert info.stat().st_mtime < time.time() - 600
    assert (storage / "message" / sid).stat().st_mtime < time.time() - 600
    # ...but the newest message's parts are being written right now.
    pdir = storage / "part" / "msg_0"
    pdir.mkdir(parents=True)
    (pdir / "prt_000.json").write_text('{"type":"text","text":"streaming"}', encoding="utf-8")
    _touch(pdir, 5.0)
    assert [s.session_id for s in _run_opencode(tmp_path, storage)] == [sid]


def test_us6_opencode_old_session_with_fresh_part_reaches_family1(tmp_path):
    """Enumeration cannot hide a content signal written after stale metadata."""
    storage = tmp_path / "opencode"
    sid = "ses_ancient"
    _write_opencode_session(
        storage, session_id=sid, cwd="/x", mtime_age=86_400.0 * 30, messages=1
    )
    pdir = storage / "part" / "msg_0"
    pdir.mkdir(parents=True)
    (pdir / "prt_000.json").write_text('{"type":"text","text":"x"}', encoding="utf-8")
    _touch(pdir, 5.0)
    sessions = _run_opencode(tmp_path, storage)
    assert [s.session_id for s in sessions] == [sid]
    assert sessions[0].truth_state == "working"


def test_us6_opencode_dedups_and_honors_exclusions(tmp_path):
    """The dedup/exclusion guard is this lane's only expression of the
    "live but un-adopted" contract, so pin both halves of it."""
    storage = tmp_path / "opencode"
    sid = "ses_dupe"
    # Same session id recorded under two project dirs -> one row, not two.
    _write_opencode_session(
        storage, session_id=sid, cwd="/x", mtime_age=5.0, project_id="projA"
    )
    _write_opencode_session(
        storage, session_id=sid, cwd="/x", mtime_age=5.0, project_id="projB"
    )
    assert [s.session_id for s in _run_opencode(tmp_path, storage)] == [sid]
    # An already-adopted session is excluded from the discovered lane.
    assert _run_opencode(tmp_path, storage, exclude_session_ids={sid}) == []


# --------------------------------------------------------------------------
# opencode SQLite store (current opencode; the JSON tree above is legacy)
# --------------------------------------------------------------------------


def _write_opencode_db(storage: Path, sessions, messages=(), parts=()) -> Path:
    """Build an opencode.db matching the real 1.14.50 schema.

    Timestamps are milliseconds, as opencode stores them.
    """
    import sqlite3

    storage.mkdir(parents=True, exist_ok=True)
    db = storage.parent / "opencode.db"
    con = sqlite3.connect(db)
    con.execute(
        "CREATE TABLE session (id TEXT, directory TEXT, time_created INTEGER,"
        " time_updated INTEGER)"
    )
    con.execute(
        "CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER,"
        " data TEXT)"
    )
    con.execute(
        "CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT,"
        " time_created INTEGER, data TEXT)"
    )
    for sid, directory, age in sessions:
        updated = int((time.time() - age) * 1000)
        con.execute(
            "INSERT INTO session VALUES (?,?,?,?)", (sid, directory, updated, updated)
        )
    for mid, sid, created, data in messages:
        con.execute(
            "INSERT INTO message VALUES (?,?,?,?)", (mid, sid, created, json.dumps(data))
        )
    for pid, mid, created, data in parts:
        con.execute(
            "INSERT INTO part VALUES (?,?,?,?,?)",
            (pid, mid, "", created, json.dumps(data)),
        )
    con.commit()
    con.close()
    return db


def test_opencode_db_surfaces_live_session(tmp_path):
    """The SQLite store is where current opencode writes; time_updated is an
    explicit activity timestamp, so no mtime inference is involved."""
    storage = tmp_path / "opencode" / "storage"
    _write_opencode_db(
        storage,
        [("ses_live", "/Users/x/proj", 5.0), ("ses_stale", "/Users/x/old", 10_000.0)],
    )
    sessions = _run_opencode(tmp_path, storage)
    assert [(s.session_id, s.cwd, s.agent) for s in sessions] == [
        ("ses_live", "/Users/x/proj", "opencode"),
        ("ses_stale", "/Users/x/old", "opencode"),
    ]
    assert [s.truth_state for s in sessions] == ["unknown", "unknown"]


def test_opencode_db_wins_over_legacy_tree(tmp_path):
    """A host mid-migration has both; the database is authoritative because the
    JSON tree stops being written once opencode moves to SQLite."""
    storage = tmp_path / "opencode" / "storage"
    _write_opencode_session(
        storage, session_id="ses_legacy", cwd="/legacy", mtime_age=5.0
    )
    _write_opencode_db(storage, [("ses_db", "/current", 5.0)])
    assert [s.session_id for s in _run_opencode(tmp_path, storage)] == ["ses_db"]


def test_opencode_legacy_tree_used_when_no_db(tmp_path):
    """An install old enough to have no database still resolves."""
    storage = tmp_path / "opencode" / "storage"
    _write_opencode_session(
        storage, session_id="ses_old", cwd="/legacy", mtime_age=5.0
    )
    assert [s.session_id for s in _run_opencode(tmp_path, storage)] == ["ses_old"]


def test_opencode_db_unreadable_degrades_to_no_rows(tmp_path):
    """A corrupt or future-schema database contributes nothing, never raises."""
    storage = tmp_path / "opencode" / "storage"
    storage.mkdir(parents=True)
    (storage.parent / "opencode.db").write_text("not a database", encoding="utf-8")
    assert _run_opencode(tmp_path, storage) == []


def test_opencode_empty_db_does_not_resurrect_legacy_sessions(tmp_path):
    """A database that exists but yields nothing means "nothing is live", NOT
    "no database". Falling back here would surface the legacy tree's long-dead
    sessions as live."""
    storage = tmp_path / "opencode" / "storage"
    _write_opencode_session(
        storage, session_id="ses_legacy", cwd="/legacy", mtime_age=5.0
    )
    _write_opencode_db(storage, [])  # real store, no live sessions
    assert _run_opencode(tmp_path, storage) == []


def test_opencode_broken_db_does_not_resurrect_legacy_sessions(tmp_path):
    """Same for a locked/corrupt/schema-drifted store: an error reads as empty,
    and must not be mistaken for "this host has no database"."""
    storage = tmp_path / "opencode" / "storage"
    _write_opencode_session(
        storage, session_id="ses_legacy", cwd="/legacy", mtime_age=5.0
    )
    storage.mkdir(parents=True, exist_ok=True)
    (storage.parent / "opencode.db").write_text("not a database", encoding="utf-8")
    assert _run_opencode(tmp_path, storage) == []


# ---------------------------------------------------------------------------
# resolve_reachable (x-e864): the liveness-blind rung below discovery.
# Each source gets its own test because a source that silently never fires is
# indistinguishable from one that fired and missed -- the exact bug class this
# node exists to kill. Two of these caught real defects: the roster reader fell
# back to Path('') (which is Path('.'), not falsy, so it read ./roster.json),
# and the graph reader imported a module path that does not exist.
# ---------------------------------------------------------------------------


def _stale(path):
    old = time.time() - 7200
    os.utime(path, (old, old))


def test_resolve_reachable_finds_an_asleep_transcript(tmp_path, monkeypatch):
    from fno.agents import discover

    sid = "5b17e2f0-1c44-4d9a-8e3b-2f6a7c081d55"
    proj = tmp_path / "projects" / "-Users-x-proj"
    proj.mkdir(parents=True)
    t = proj / f"{sid}.jsonl"
    t.write_text("{}\n", encoding="utf-8")
    _stale(t)

    found, ambiguous = discover.resolve_reachable(sid[:8], projects_dir=tmp_path / "projects")

    assert ambiguous == []
    assert found is not None and found.session_id == sid
    assert found.source == "transcript"


def test_resolve_reachable_reads_the_roster_without_an_env_override(tmp_path, monkeypatch):
    """The roster source must resolve its own default dir.

    Regression pin: the fallback used to be ``Path(os.environ.get(k, ""))``,
    and ``Path("")`` is ``Path(".")`` -- truthy -- so with the env var unset the
    reader looked for ./roster.json and this source never fired in production.
    """
    from fno.agents import discover

    sid = "aa11bb22-3344-5566-7788-99aabbccddee"
    monkeypatch.delenv("FNO_CLAUDE_DAEMON_DIR", raising=False)
    home = tmp_path / "home"
    daemon = home / ".claude" / "daemon"
    daemon.mkdir(parents=True)
    (daemon / "roster.json").write_text(
        json.dumps({"proto": 1, "workers": {sid[:8]: {"sessionId": sid, "pid": 1}}}),
        encoding="utf-8",
    )
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))

    empty_projects = tmp_path / "no-projects"
    empty_projects.mkdir()
    found, _ = discover.resolve_reachable(sid[:8], projects_dir=empty_projects)

    assert found is not None, "the roster source did not fire on its default dir"
    assert found.session_id == sid and found.source == "roster"


def test_resolve_reachable_reads_backlog_session_stamps(tmp_path, monkeypatch):
    """The graph source must actually import and parse.

    Regression pin: it imported ``fno.graph.io`` (no such module) and indexed
    the result as a dict-with-nodes, while ``load_graph`` lives in
    ``fno.graph.load`` and returns a flat ``list[dict]``. Both failures were
    swallowed, so the source returned [] forever.
    """
    from fno.agents import discover

    sid = "ccdd1122-3344-5566-7788-99aabbccddee"
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps({"entries": [
            {"id": "x-0001", "sessions": [
                {"phase": "ship", "harness": "claude", "session_id": sid}
            ]}
        ]}),
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.graph.load.GRAPH_JSON", graph)

    empty_projects = tmp_path / "no-projects"
    empty_projects.mkdir()
    found, _ = discover.resolve_reachable(sid[:8], projects_dir=empty_projects)

    assert found is not None, "the graph source did not fire"
    assert found.session_id == sid and found.source == "graph"


def test_resolve_reachable_reports_ambiguity_instead_of_guessing(tmp_path, monkeypatch):
    from fno.agents import discover

    a = "c0ffee11-1111-2222-3333-444444444444"
    b = "c0ffee11-9999-8888-7777-666666666666"
    projects = tmp_path / "projects"
    for sid in (a, b):
        proj = projects / f"-Users-x-{sid[-4:]}"
        proj.mkdir(parents=True)
        t = proj / f"{sid}.jsonl"
        t.write_text("{}\n", encoding="utf-8")
        _stale(t)

    found, ambiguous = discover.resolve_reachable("c0ffee11", projects_dir=projects)

    assert found is None
    assert sorted(ambiguous) == sorted([a, b])


def test_resolve_reachable_misses_cleanly_on_an_unknown_token(tmp_path, monkeypatch):
    """A clean miss across READABLE stores is the only case that earns exit 16."""
    from fno.agents import discover

    projects = tmp_path / "projects"
    projects.mkdir()
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))

    found, ambiguous = discover.resolve_reachable(
        "deadbeef", projects_dir=projects, registry_path=tmp_path / "registry.json"
    )

    assert found is None and ambiguous == []


def test_resolve_reachable_does_not_match_on_a_short_prefix(tmp_path):
    """A loose prefix match would sweep half the store into an ambiguity error."""
    from fno.agents import discover

    sid = "5b17e2f0-1c44-4d9a-8e3b-2f6a7c081d55"
    proj = tmp_path / "projects" / "-Users-x-proj"
    proj.mkdir(parents=True)
    t = proj / f"{sid}.jsonl"
    t.write_text("{}\n", encoding="utf-8")
    _stale(t)

    found, _ = discover.resolve_reachable("5b", projects_dir=tmp_path / "projects")

    assert found is None


def test_unreadable_store_raises_instead_of_reporting_absence(tmp_path, monkeypatch):
    """The mail-loss guard: a store that ERRORS must not read as 'not found'.

    The caller turns a clean miss into exit 16 and queues nothing, so absence
    has to be proven rather than assumed. A torn registry is the realistic
    trigger: it raises, and reporting that as "no rows" would drop mail for a
    session the registry actually knows about.
    """
    from fno.agents import discover
    from fno.agents.registry import RegistryVersionError

    projects = tmp_path / "projects"
    projects.mkdir()
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))

    def _torn(*_a, **_k):
        raise RegistryVersionError("schema drift")

    monkeypatch.setattr("fno.agents.registry.load_registry", _torn)

    with pytest.raises(discover.StoreReadError) as err:
        discover.resolve_reachable("deadbeef", projects_dir=projects)
    assert "registry" in err.value.failed


def test_an_absent_store_is_a_clean_answer_not_a_read_failure(tmp_path, monkeypatch):
    """An absent transcript store means no claude session ever ran here.

    That is definitive, so it must stay a clean miss. Classifying it as
    unreadable would make every typo queue durably on a host that has never
    run claude, stranding envelopes and destroying the exit-16 typo guard.
    """
    from fno.agents import discover

    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))

    found, ambiguous = discover.resolve_reachable(
        "deadbeef",
        projects_dir=tmp_path / "never-created",
        registry_path=tmp_path / "registry.json",
    )

    assert found is None and ambiguous == []


def test_one_hit_plus_a_degraded_store_is_unproven_not_unique(tmp_path, monkeypatch):
    """A lone hit is not proof of uniqueness while a store is unreadable.

    The unreadable store could hold a session colliding on the same short id,
    so waking the one hit would be exactly the guess the never-guess rule
    forbids. The candidate rides along on the error so the caller can still
    address a durable copy to a real session instead of demoting blind.
    """
    from fno.agents import discover

    sid = "aa11bb22-3344-5566-7788-99aabbccddee"
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    (daemon / "roster.json").write_text(
        json.dumps({"proto": 1, "workers": {sid[:8]: {"sessionId": sid, "pid": 1}}}),
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))

    def _torn(*_a, **_k):
        raise OSError("registry unreadable")

    monkeypatch.setattr("fno.agents.registry.load_registry", _torn)
    projects = tmp_path / "projects"
    projects.mkdir()

    with pytest.raises(discover.StoreReadError) as err:
        discover.resolve_reachable(sid[:8], projects_dir=projects)
    assert "registry" in err.value.failed
    assert err.value.resolved is not None
    assert err.value.resolved.session_id == sid


def test_registry_source_yields_the_resumable_uuid_not_the_job_id(tmp_path, monkeypatch):
    """`AgentEntry.session_id` is harness-polymorphic: for claude it resolves to
    `short_id`, the 8-hex daemon transport key, NOT a resumable uuid. Waking on
    that value would run `claude -r <jobId>` against a session that does not
    exist, so this source must read `harness_session_id` directly.
    """
    from fno.agents import discover

    sid = "bb22cc33-4455-6677-8899-aabbccddeeff"

    class _Row:
        harness = "claude"
        harness_session_id = sid
        short_id = "deadbeef"

        @property
        def session_id(self):  # what the buggy version read
            return self.short_id

    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda *_a, **_k: [_Row()]
    )
    projects = tmp_path / "projects"
    projects.mkdir()

    found, _ = discover.resolve_reachable(sid[:8], projects_dir=projects)

    assert found is not None
    assert found.session_id == sid, "resolved the job id instead of the uuid"
    assert found.agent == "claude"


def test_registry_source_carries_a_non_claude_harness_through(tmp_path, monkeypatch):
    """The harness must survive resolution so the wake lane can refuse it.

    The registry holds rows for every provider; handing a codex thread id to a
    claude resume would revive the wrong session entirely.
    """
    from fno.agents import discover

    sid = "cc33dd44-5566-7788-99aa-bbccddeeff00"

    class _Row:
        harness = "codex"
        harness_session_id = sid
        short_id = "cc33dd44"

    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda *_a, **_k: [_Row()]
    )
    projects = tmp_path / "projects"
    projects.mkdir()

    found, _ = discover.resolve_reachable(sid[:8], projects_dir=projects)

    assert found is not None and found.agent == "codex"


# ---------------------------------------------------------------------------
# Review-round fixes (PR 501): every store consulted before uniqueness, alias
# survival, cwd propagation, case-insensitive tokens, malformed-store guards.
# ---------------------------------------------------------------------------


def test_short_id_colliding_across_two_stores_is_ambiguous_not_unique(
    tmp_path, monkeypatch
):
    """Returning on the first source that answers is itself a guess.

    A transcript hit plus a DIFFERENT uuid in the registry sharing the same
    short id would look unique under an early return, and the never-guess rule
    would be violated by the control flow rather than by a bad choice.
    """
    from fno.agents import discover

    a = "c0ffee11-1111-2222-3333-444444444444"
    b = "c0ffee11-9999-8888-7777-666666666666"

    projects = tmp_path / "projects"
    proj = projects / "-Users-x-proj"
    proj.mkdir(parents=True)
    t = proj / f"{a}.jsonl"
    t.write_text("{}\n", encoding="utf-8")
    _stale(t)

    class _Row:
        harness = "claude"
        harness_session_id = b
        short_id = "c0ffee11"
        cwd = "/Users/x/other"

    monkeypatch.setattr("fno.agents.registry.load_registry", lambda *_a, **_k: [_Row()])

    found, ambiguous = discover.resolve_reachable("c0ffee11", projects_dir=projects)

    assert found is None, "a cross-store short-id collision resolved as unique"
    assert sorted(ambiguous) == sorted([a, b])


def test_friendly_alias_still_resolves_once_the_session_is_asleep(
    tmp_path, monkeypatch
):
    """An address that worked while live must not vanish when the session sleeps."""
    from fno.agents import discover

    sid = "5b17e2f0-1c44-4d9a-8e3b-2f6a7c081d55"
    projects = tmp_path / "projects"
    proj = projects / "-Users-x-proj"
    proj.mkdir(parents=True)
    t = proj / f"{sid}.jsonl"
    t.write_text("{}\n", encoding="utf-8")
    _stale(t)

    name_map = tmp_path / "session-names.json"
    name_map.write_text(json.dumps({sid: "fno-5b17e2f0"}), encoding="utf-8")

    found, _ = discover.resolve_reachable(
        "fno-5b17e2f0", projects_dir=projects, name_map_path=name_map
    )

    assert found is not None, "a friendly alias stopped resolving once asleep"
    assert found.session_id == sid


def test_token_matching_is_case_insensitive(tmp_path):
    """Uuids and hex short ids are case-insensitive by definition."""
    from fno.agents import discover

    sid = "5b17e2f0-1c44-4d9a-8e3b-2f6a7c081d55"
    projects = tmp_path / "projects"
    proj = projects / "-Users-x-proj"
    proj.mkdir(parents=True)
    t = proj / f"{sid}.jsonl"
    t.write_text("{}\n", encoding="utf-8")
    _stale(t)

    found, _ = discover.resolve_reachable("5B17E2F0", projects_dir=projects)

    assert found is not None and found.session_id == sid


def test_roster_cwd_is_carried_so_a_wake_resumes_in_the_right_repo(
    tmp_path, monkeypatch
):
    """Claude resume is cwd-scoped: waking a cross-repo recipient from the
    sender's directory would fail to revive the resolved session."""
    from fno.agents import discover

    sid = "aa11bb22-3344-5566-7788-99aabbccddee"
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    (daemon / "roster.json").write_text(
        json.dumps({"proto": 1, "workers": {
            sid[:8]: {"sessionId": sid, "pid": 1, "cwd": "/Users/x/other-repo"}
        }}),
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    projects = tmp_path / "projects"
    projects.mkdir()

    found, _ = discover.resolve_reachable(sid[:8], projects_dir=projects)

    assert found is not None and found.cwd == "/Users/x/other-repo"


def test_type_drifted_roster_is_unreadable_not_empty(tmp_path, monkeypatch):
    """`workers` as a list would raise AttributeError straight out of the send."""
    from fno.agents import discover

    daemon = tmp_path / "daemon"
    daemon.mkdir()
    (daemon / "roster.json").write_text(
        json.dumps({"proto": 1, "workers": ["not", "a", "dict"]}), encoding="utf-8"
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    projects = tmp_path / "projects"
    projects.mkdir()

    with pytest.raises(discover.StoreReadError) as err:
        discover.resolve_reachable(
            "deadbeef", projects_dir=projects, registry_path=tmp_path / "reg.json"
        )
    assert "roster" in err.value.failed


def test_malformed_graph_is_reported_unreadable_not_empty(tmp_path, monkeypatch):
    """`sessions` as a non-list must not raise, and must not read as absent.

    Skipping the bad node while still reporting the store readable would let a
    corrupt entry hide the only durable record of the addressed session,
    turning a durable demotion into exit 16 with nothing queued.
    """
    from fno.agents import discover

    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps({"entries": [{"id": "x-0001", "sessions": 42}]}), encoding="utf-8"
    )
    monkeypatch.setattr("fno.graph.load.GRAPH_JSON", graph)
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    projects = tmp_path / "projects"
    projects.mkdir()

    with pytest.raises(discover.StoreReadError) as err:
        discover.resolve_reachable(
            "deadbeef", projects_dir=projects, registry_path=tmp_path / "reg.json"
        )
    assert "graph" in err.value.failed


def test_opencode_ids_keep_their_case_while_hex_folds(tmp_path):
    """Case folding is for hex ids only.

    An opencode id (`ses_...`) is mixed-case by construction, so folding it
    would let two sessions differing only in case collide -- and a wrong
    collision here wakes a stranger's session. Mirrors the normalization rule
    in `agents.store_fallback`; the two must not drift.
    """
    from fno.agents.discover import _token_matches

    # Hex folds: same session, different spelling.
    assert _token_matches("5B17E2F0", "5b17e2f0-1c44-4d9a-8e3b-2f6a7c081d55")
    assert _token_matches("5b17e2f0", "5B17E2F0-1C44-4D9A-8E3B-2F6A7C081D55")

    # opencode does NOT fold: case is meaningful, so these are distinct.
    assert _token_matches("ses_AbC123", "ses_AbC123")
    assert not _token_matches("ses_abc123", "ses_AbC123")
