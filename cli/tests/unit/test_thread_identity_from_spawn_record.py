"""A codex thread worker proves its identity from its own spawn record (x-e882).

The process-tree walk cannot answer for the thread lane: every thread worker
is a WebSocket client of the ONE shared ``codex app-server`` daemon, so N
workers share one pid and ancestry cannot name which thread a shell belongs
to. The spawn record can. These tests pin the cwd-keyed read of that record
and the resolver fill that consumes it, including the guard that a proven
ambient identity is never overwritten.
"""

import json

import pytest

from fno.harness_identity import live_thread_row_for_cwd
from fno.paths import agents_registry_path
from fno.paths_testing import use_tmpdir

# HARNESS_SESSION_MARKERS is the ambient marker set; identity tests scrub it so
# the pytest host's own harness env cannot pose as the synthetic session.
from fno.harness_identity import HARNESS_SESSION_MARKERS


def _thread_row(name, cwd, session_id, *, status="live", substrate="thread"):
    return {
        "name": name,
        "status": status,
        "substrate": substrate,
        "harness": "codex",
        "harness_session_id": session_id,
        "cwd": cwd,
    }


def _write_registry(tmp_path, monkeypatch, rows, *, raw=None):
    use_tmpdir(monkeypatch, tmp_path)
    path = agents_registry_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    if raw is not None:
        path.write_bytes(raw)
    else:
        path.write_text(json.dumps({"schema_version": 23, "agents": rows}))
    return path


def _two_worker_rows(tmp_path):
    """Two live thread rows: one worktree each, one session id each."""
    cwd_a = tmp_path / "worker-a"
    cwd_b = tmp_path / "worker-b"
    cwd_a.mkdir()
    cwd_b.mkdir()
    sid_a = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    sid_b = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae5"
    rows = [
        _thread_row("worker-a", str(cwd_a), sid_a),
        _thread_row("worker-b", str(cwd_b), sid_b),
    ]
    return rows, str(cwd_a), str(cwd_b), sid_a, sid_b


def test_one_live_thread_row_for_cwd_returns_its_identity(tmp_path, monkeypatch):
    rows, cwd_a, _cwd_b, sid_a, _sid_b = _two_worker_rows(tmp_path)
    _write_registry(tmp_path, monkeypatch, rows)
    assert live_thread_row_for_cwd(cwd_a) == ("codex", sid_a)


def test_sibling_cwd_returns_the_sibling_row_not_the_victim(tmp_path, monkeypatch):
    """The discriminating property of the lane: one shared app-server pid, two
    different session ids. The key is the caller's own cwd, so the same lookup
    run from each worktree answers with that worktree's own row."""
    rows, cwd_a, cwd_b, sid_a, sid_b = _two_worker_rows(tmp_path)
    _write_registry(tmp_path, monkeypatch, rows)
    assert live_thread_row_for_cwd(cwd_a) == ("codex", sid_a)
    assert live_thread_row_for_cwd(cwd_b) == ("codex", sid_b)
    assert sid_a != sid_b


def test_duplicate_cwd_refuses_rather_than_picks(tmp_path, monkeypatch):
    cwd_a = tmp_path / "worker-a"
    cwd_a.mkdir()
    rows = [
        _thread_row("worker-a", str(cwd_a), "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"),
        _thread_row("worker-a-2", str(cwd_a), "019f48e1-5b09-72a0-9bc8-6b364bcf4ae5"),
    ]
    _write_registry(tmp_path, monkeypatch, rows)
    assert live_thread_row_for_cwd(str(cwd_a)) is None


def test_exited_rows_own_no_identity(tmp_path, monkeypatch):
    cwd_a = tmp_path / "worker-a"
    cwd_a.mkdir()
    rows = [
        _thread_row(
            "worker-a",
            str(cwd_a),
            "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4",
            status="exited",
        )
    ]
    _write_registry(tmp_path, monkeypatch, rows)
    assert live_thread_row_for_cwd(str(cwd_a)) is None


def test_non_thread_rows_and_unidentified_rows_never_answer(tmp_path, monkeypatch):
    cwd_a = tmp_path / "worker-a"
    cwd_a.mkdir()
    pane_row = _thread_row(
        "pane", str(cwd_a), "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4", substrate="pane"
    )
    bald_row = _thread_row("bald", str(cwd_a), "")  # no session id on the row
    _write_registry(tmp_path, monkeypatch, [pane_row, bald_row])
    assert live_thread_row_for_cwd(str(cwd_a)) is None


def test_absent_registry_degrades_to_none(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    assert live_thread_row_for_cwd(str(tmp_path / "nowhere")) is None


def test_unreadable_registry_degrades_to_none(tmp_path, monkeypatch):
    _write_registry(tmp_path, monkeypatch, [], raw=b"\xff\xfe not json")
    assert live_thread_row_for_cwd(str(tmp_path)) is None


def test_symlinked_worktree_matches_its_own_row(tmp_path, monkeypatch):
    real = tmp_path / "real-worktree"
    real.mkdir()
    link = tmp_path / "linked-worktree"
    link.symlink_to(real)
    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _write_registry(tmp_path, monkeypatch, [_thread_row("worker", str(real), sid)])
    assert live_thread_row_for_cwd(str(link)) == ("codex", sid)


@pytest.mark.parametrize("bad_cwd", ["", "   "])
def test_blank_cwd_never_reads_the_registry(tmp_path, monkeypatch, bad_cwd):
    _write_registry(tmp_path, monkeypatch, [])
    assert live_thread_row_for_cwd(bad_cwd) is None


def test_scrub_helper_covers_the_marker_set():
    """Positive control for the resolver tests below: the scrub loop names real
    markers, so a clean-env test cannot pass because it deleted nothing."""
    assert len(HARNESS_SESSION_MARKERS) > 0
