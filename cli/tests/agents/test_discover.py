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


def test_x_a1d5_sidecar_present_skips_fallback(tmp_path, monkeypatch):
    """AC4: a working sidecar means the projects fallback is never consulted."""
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
    assert [s.short_id for s in sessions] == ["side0001"]


def test_x_a1d5_stale_transcript_not_surfaced(tmp_path, monkeypatch):
    """A live proc whose transcript went quiet past the window is dropped."""
    use_tmpdir(monkeypatch, tmp_path)
    projects = tmp_path / "projects"
    _write_transcript(projects, cwd="/Users/x/code/proj", session_id="stale-sid",
                      mtime_age=discover._DEFAULT_RECENCY_SECONDS + 120)
    procs = [_claude_proc(4242, "/Users/x/code/proj")]
    assert _run_projects(tmp_path, projects, procs) == []


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


def test_us2_codex_stale_rollout_not_surfaced(tmp_path):
    codex = tmp_path / "codex"
    _write_codex_rollout(
        codex, session_id="019f48e1-dead", cwd="/x", mtime_age=10_000.0,
    )
    assert _run_codex(tmp_path, codex) == []


def test_loaded_daemon_thread_resolves_with_stale_rollout(tmp_path, monkeypatch):
    codex = tmp_path / "codex"
    sid = "019f4d0c-1111-2222-3333-444444444444"
    _write_codex_rollout(codex, session_id=sid, cwd="/old/repo", mtime_age=10_000.0)
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
        "codex-019f4d0c",
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=lambda c: None,
    )

    assert resolved is not None
    assert resolved.session_id == sid
    assert resolved.cwd == "/live/repo"


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


def test_us3_resolve_cross_harness_handle(tmp_path):
    codex = tmp_path / "codex"
    _write_codex_rollout(
        codex, session_id="019f48e1-5b09-72a0-9bc8-6b364bcf4ae4", cwd="/x",
    )
    resolved, suggestions = discover.resolve_or_suggest(
        "codex-019f48e1",
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=tmp_path / "no-projects",
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil(alive={}),
        project_resolver=lambda c: None,
    )
    assert resolved is not None
    assert resolved.agent == "codex"
    assert resolved.session_id == "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"


def test_ac2_edge_harness_prefix_disambiguation(tmp_path):
    # A claude session and a codex session whose shortids both start abcd1234.
    codex = tmp_path / "codex"
    _write_codex_rollout(codex, session_id="abcd1234-codex-side", cwd="/x")
    projects = tmp_path / "claude-projects"
    _write_transcript(projects, cwd="/Users/y/repo", session_id="abcd1234-cccc-dddd")
    resolved, _ = discover.resolve_or_suggest(
        "codex-abcd1234",
        sessions_dir=tmp_path / "no-sessions",
        projects_dir=projects,
        codex_sessions_dir=codex,
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakeProcsPsutil(procs={4242: (["claude"], "/Users/y/repo")}),
        project_resolver=lambda c: None,
    )
    assert resolved is not None
    assert resolved.agent == "codex"  # codex- prefix picks the codex row, not claude


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
        name_map_path=tmp_path / ".fno" / "session-names.json",
        psutil_mod=_FakePsutil({}),
        project_resolver=lambda c: None,
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
    resolved, _ = discover.resolve_or_suggest("claude-9a063cd3", **_empty_seams(tmp_path))
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
        "claude-9a063cd3", **_empty_seams(tmp_path)
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
                provider="claude",
                cwd="/Users/x/code/proj",
                log_path="/tmp/x-foo.log",
                short_id="9a063cd3",
                claude_session_uuid="9a063cd3-69d4-415a-ada5-649b0164189c",
            )
        ],
        path=reg,
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "no-daemon"))
    resolved, _ = discover.resolve_or_suggest(
        "claude-9a063cd3", registry_path=reg, **_empty_seams(tmp_path)
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
                provider="claude",
                cwd="/x",
                log_path="/tmp/x.log",
                short_id="9a063cd3",
                claude_session_uuid=sid,
            )
        ],
        path=reg,
    )
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
                name="x-dead", provider="claude", cwd="/x", log_path="/tmp/d.log",
                short_id="deadd00d", claude_session_uuid="deadd00d-1111-2222-3333-444444444444",
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


def test_us2_registry_short_id_is_jobid_not_uuid_prefix(tmp_path, monkeypatch):
    """codex review: short_id must be the authoritative jobId,
    not the uuid's first 8 hex, when the two differ."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, write_registry

    reg = tmp_path / "registry.json"
    write_registry(
        [
            AgentEntry(
                name="x-foo", provider="claude", cwd="/x", log_path="/tmp/f.log",
                short_id="j0b1d001",  # jobId
                claude_session_uuid="aaaabbbb-1111-2222-3333-444444444444",  # uuid[:8]=aaaabbbb
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
    # ...and by the canonical handle derived from the uuid.
    by_canon, _ = discover.resolve_or_suggest(
        "claude-aaaabbbb", registry_path=reg, **_empty_seams(tmp_path)
    )
    assert by_canon is not None
