"""US1 (x-86e4): capture a codex pane's session id at spawn.

Codex, like opencode, cannot be handed a pre-minted session id the way claude
can, so the id has to be discovered from the rollout store after the pane runs.
Without this capture every codex pane row carries ``harness_session_id=None``
forever, and ``_discover_from_registry`` drops a sid-less row before any name
matching runs -- so a live codex worker is unreachable by truth, peek, and the
mail name lane at once.

Coverage:
  - AC2-FR: a missed capture leaves the row live-only, never crashes the spawn.
  - The match is exact-directory + started-after-spawn, so a sibling worktree's
    session and a pre-existing session in the SAME cwd are both excluded.
  - Two same-cwd candidates stamp NEITHER row (the opencode ambiguity rule).

Every case drives an injected sessions dir: the suite must never read the
developer's real ``~/.codex``.
"""
from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path

import pytest

from fno.agents import mux_spawn
from fno.agents.discover import codex_session_ids_started_in
from fno.agents.mux_spawn import _backfill_codex_session_id

SID_A = "019cc081-de0d-7283-97cc-751c46742a07"
SID_B = "019cc082-1111-7283-97cc-751c46742a08"

T_SPAWN = "2026-07-24T18:00:00.000Z"
T_EARLIER = "2026-07-24T17:00:00.000Z"


def _ms(iso: str) -> int:
    return int(datetime.fromisoformat(iso).timestamp() * 1000)


def _rollout(root: Path, sid: str, cwd: str, started: str) -> Path:
    """Write a rollout jsonl whose first line is codex's real session_meta shape."""
    day = root / "2026" / "07" / "24"
    day.mkdir(parents=True, exist_ok=True)
    path = day / f"rollout-{started.replace(':', '-')}-{sid}.jsonl"
    path.write_text(
        json.dumps(
            {
                "timestamp": started,
                "type": "session_meta",
                "payload": {"id": sid, "timestamp": started, "cwd": cwd},
            }
        )
        + "\n",
        encoding="utf-8",
    )
    return path


def _no_sleep(_seconds: float) -> None:
    """Collapse the retry delay so tests do not pay it."""


# ---------------------------------------------------------------------------
# codex_session_ids_started_in: the store query
# ---------------------------------------------------------------------------


def test_unique_candidate_is_found(tmp_path: Path) -> None:
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert codex_session_ids_started_in(
        Path("/w/proj"), _ms(T_SPAWN) - 1, sessions_dir=tmp_path
    ) == [SID_A]


def test_sibling_worktree_in_another_cwd_is_excluded(tmp_path: Path) -> None:
    """Worktrees of one repo differ only by directory, so cwd must match exactly."""
    _rollout(tmp_path, SID_A, "/w/proj-other", T_SPAWN)
    assert (
        codex_session_ids_started_in(
            Path("/w/proj"), _ms(T_SPAWN) - 1, sessions_dir=tmp_path
        )
        == []
    )


def test_session_started_before_the_spawn_is_excluded(tmp_path: Path) -> None:
    """A codex session already open in this cwd is NOT the one we just spawned.

    Mtime cannot make this call: an older session that is still being typed into
    has a fresh mtime. Only the session_meta start timestamp separates them.
    """
    _rollout(tmp_path, SID_A, "/w/proj", T_EARLIER)
    assert (
        codex_session_ids_started_in(
            Path("/w/proj"), _ms(T_SPAWN), sessions_dir=tmp_path
        )
        == []
    )


def test_unreadable_and_malformed_rollouts_are_skipped(tmp_path: Path) -> None:
    day = tmp_path / "2026" / "07" / "24"
    day.mkdir(parents=True)
    (day / "rollout-garbage.jsonl").write_text("not json\n", encoding="utf-8")
    (day / "rollout-wrongtype.jsonl").write_text(
        json.dumps({"type": "response_item", "payload": {}}) + "\n", encoding="utf-8"
    )
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert codex_session_ids_started_in(
        Path("/w/proj"), 1, sessions_dir=tmp_path
    ) == [SID_A]


def test_missing_store_returns_empty(tmp_path: Path) -> None:
    assert (
        codex_session_ids_started_in(
            Path("/w/proj"), 1, sessions_dir=tmp_path / "absent"
        )
        == []
    )


# ---------------------------------------------------------------------------
# _backfill_codex_session_id: the spawn-side rule
# ---------------------------------------------------------------------------


def test_backfill_returns_the_unique_id(tmp_path: Path) -> None:
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), 1, sessions_dir=tmp_path, sleep=_no_sleep
        )
        == SID_A
    )


def test_backfill_refuses_an_ambiguous_pair(tmp_path: Path) -> None:
    """Two panes racing in one cwd: stamping either row could point resume at
    the other pane's session, so neither gets an id."""
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    _rollout(tmp_path, SID_B, "/w/proj", "2026-07-24T18:00:01.000Z")
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), 1, sessions_dir=tmp_path, sleep=_no_sleep
        )
        is None
    )


def test_backfill_skips_a_same_cwd_sibling_started_earlier(tmp_path: Path) -> None:
    """The race a too-early spawn timestamp loses.

    A sibling pane that started its codex session before this spawn's pane run
    must not be stamped onto this row. since_ms is sampled at the pane run, so
    the earlier sibling is excluded and this spawn's own session is the unique
    candidate (and the one stamped), not the sibling.
    """
    _rollout(tmp_path, SID_B, "/w/proj", T_EARLIER)
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), _ms(T_SPAWN), sessions_dir=tmp_path, sleep=_no_sleep
        )
        == SID_A
    )


def test_backfill_waits_out_a_transiently_unique_sibling(
    tmp_path: Path, monkeypatch
) -> None:
    """Codex P2 (#603): a sibling rollout surfacing before this pane's must not
    be stamped. The stability gate holds the first single sighting; when a second
    candidate appears on the next probe the backfill returns None rather than
    committing to the sibling."""
    from fno.agents import discover

    seq = iter([[SID_B], [SID_B, SID_A]])

    monkeypatch.setattr(
        discover, "codex_session_ids_started_in", lambda *_a, **_k: next(seq)
    )
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), _ms(T_SPAWN), sessions_dir=tmp_path, sleep=_no_sleep
        )
        is None
    )


def test_backfill_miss_returns_none_rather_than_raising(tmp_path: Path) -> None:
    """AC2-FR: the pane is already running; a missing id costs resume, not the spawn."""
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), 1, sessions_dir=tmp_path, sleep=_no_sleep
        )
        is None
    )


# ---------------------------------------------------------------------------
# Race-free id via the child pid's open rollout (Codex P1, #603)
# ---------------------------------------------------------------------------


class _FakeOpenFile:
    def __init__(self, path: str) -> None:
        self.path = path


class _FakeProc:
    def __init__(self, files=(), children=()) -> None:
        self._files = list(files)
        self._children = list(children)

    def open_files(self) -> list:
        return self._files

    def children(self, recursive: bool = False) -> list:
        out = list(self._children)
        if recursive:
            for c in self._children:
                out += c.children(recursive=True)
        return out


class _FakePsutil:
    class NoSuchProcess(Exception):
        pass

    def __init__(self, proc) -> None:
        self._proc = proc

    def Process(self, pid: int):  # noqa: N802 (mirror psutil API)
        return self._proc


def _write_rollout_with_id(root: Path, filename_id: str, payload_id: str) -> Path:
    """A rollout whose filename UUID may differ from session_meta.payload.id, to
    prove the id is read from metadata, not the filename (Codex P2 r5, #603)."""
    day = root / "2026" / "07" / "24"
    day.mkdir(parents=True, exist_ok=True)
    path = day / f"rollout-2026-07-24T18-00-00-{filename_id}.jsonl"
    path.write_text(
        json.dumps(
            {
                "timestamp": T_SPAWN,
                "type": "session_meta",
                "payload": {"id": payload_id, "timestamp": T_SPAWN, "cwd": "/w/proj"},
            }
        )
        + "\n",
        encoding="utf-8",
    )
    return path


def test_codex_session_id_for_pid_reads_metadata_from_the_open_rollout(
    tmp_path: Path,
) -> None:
    """The id is session_meta.payload.id (authoritative), read race-free from the
    pane's own open rollout (Codex P1, #603)."""
    from fno.agents.mux_spawn import _codex_session_id_for_pid

    rollout = _write_rollout_with_id(tmp_path, SID_A, SID_A)
    psu = _FakePsutil(
        _FakeProc([_FakeOpenFile(str(rollout)), _FakeOpenFile("/unrelated.log")])
    )
    assert _codex_session_id_for_pid(4242, psutil_mod=psu) == SID_A


def test_codex_session_id_for_pid_uses_metadata_not_the_filename_uuid(
    tmp_path: Path,
) -> None:
    """Codex P2 r5 (#603): older turn-id layouts name the rollout by a UUID that
    is not session_meta.payload.id; the id must come from the metadata."""
    from fno.agents.mux_spawn import _codex_session_id_for_pid

    other = "11111111-2222-3333-4444-555566667777"  # filename UUID, not the session
    rollout = _write_rollout_with_id(tmp_path, other, SID_A)
    psu = _FakePsutil(_FakeProc([_FakeOpenFile(str(rollout))]))
    assert _codex_session_id_for_pid(4242, psutil_mod=psu) == SID_A


def test_codex_session_id_for_pid_walks_wrapper_descendants(tmp_path: Path) -> None:
    """Codex P1 r5 (#603): a wrapper launcher (@openai/codex Node shim) holds the
    pane pid while its native child opens the rollout; the descendant walk finds it."""
    from fno.agents.mux_spawn import _codex_session_id_for_pid

    rollout = _write_rollout_with_id(tmp_path, SID_A, SID_A)
    native = _FakeProc([_FakeOpenFile(str(rollout))])
    wrapper = _FakeProc(files=(), children=[native])  # pane pid is the wrapper
    psu = _FakePsutil(wrapper)
    assert _codex_session_id_for_pid(4242, psutil_mod=psu) == SID_A


def test_codex_session_id_for_pid_refuses_an_ambiguous_tree(tmp_path: Path) -> None:
    """AC5-CON: the same-cwd sibling-safety property, at the one place that owns it.

    Two distinct rollouts open in one process tree means the correlator cannot say
    which session this pane is, so it must return None rather than pick. Without
    this the "siblings can never be cross-stamped" claim lives only in a docstring.
    """
    from fno.agents.mux_spawn import _codex_session_id_for_pid

    roll_a = _write_rollout_with_id(tmp_path / "a", SID_A, SID_A)
    roll_b = _write_rollout_with_id(tmp_path / "b", SID_B, SID_B)
    native = _FakeProc([_FakeOpenFile(str(roll_b))])
    psu = _FakePsutil(_FakeProc([_FakeOpenFile(str(roll_a))], children=[native]))
    assert _codex_session_id_for_pid(4242, psutil_mod=psu) is None


def test_codex_session_id_for_pid_binds_when_the_tree_is_unambiguous(
    tmp_path: Path,
) -> None:
    """Positive control for the refusal above: the SAME shape with one rollout
    open twice still binds, so that test proves ambiguity and not merely that a
    descendant walk with two open files always fails."""
    from fno.agents.mux_spawn import _codex_session_id_for_pid

    rollout = _write_rollout_with_id(tmp_path / "a", SID_A, SID_A)
    native = _FakeProc([_FakeOpenFile(str(rollout))])
    psu = _FakePsutil(_FakeProc([_FakeOpenFile(str(rollout))], children=[native]))
    assert _codex_session_id_for_pid(4242, psutil_mod=psu) == SID_A


def test_codex_session_id_for_pid_returns_none_with_no_rollout_open() -> None:
    from fno.agents.mux_spawn import _codex_session_id_for_pid

    psu = _FakePsutil(_FakeProc([_FakeOpenFile("/unrelated.log")]))
    assert _codex_session_id_for_pid(4242, psutil_mod=psu) is None


def test_backfill_with_child_pid_never_consults_the_cwd_store(
    tmp_path: Path, monkeypatch
) -> None:
    """Codex P1 (#603): with the child pid, the id comes race-free from the
    child's open rollout. The cwd/timestamp store is never read, so a sibling
    rollout present in the same cwd cannot be stamped onto this row."""
    from fno.agents import discover
    from fno.agents.mux_spawn import _backfill_codex_session_id

    rollout = _write_rollout_with_id(tmp_path, SID_A, SID_A)
    # A sibling the old discovery path would have picked up.
    _rollout(tmp_path, SID_B, "/w/proj", T_SPAWN)
    monkeypatch.setattr(
        discover,
        "codex_session_ids_started_in",
        lambda *_a, **_k: pytest.fail("cwd store was consulted on the pid path"),
    )
    psu = _FakePsutil(_FakeProc([_FakeOpenFile(str(rollout))]))
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"),
            _ms(T_SPAWN),
            sessions_dir=tmp_path,
            child_pid=4242,
            sleep=_no_sleep,
            psutil_mod=psu,
        )
        == SID_A
    )


# ---------------------------------------------------------------------------
# mtime-bounded rollout scan (Codex P2 perf, #603)
# ---------------------------------------------------------------------------


def test_rollout_scan_skips_files_older_than_since(tmp_path: Path) -> None:
    """A rollout whose last write precedes since_ms is pruned before parsing.
    codex appends over the session so mtime >= start always; a file older than
    since_ms therefore cannot be a session started at/after it (Codex P2, #603)."""
    import os as _os

    path = _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    old_mt = _ms(T_EARLIER) / 1000.0
    _os.utime(path, (old_mt, old_mt))
    assert (
        codex_session_ids_started_in(
            Path("/w/proj"), _ms(T_SPAWN), sessions_dir=tmp_path
        )
        == []
    )


# ---------------------------------------------------------------------------
# The app-server daemon oracle: codex 0.148 hands session ownership to a
# detached `codex app-server --remote-control` daemon, which holds the
# rollout fd the process-tree probe above expects to find in the pane's own
# tree - measured live at 0/20 binds. This oracle asks the daemon directly.
# ---------------------------------------------------------------------------


def test_daemon_bind_accepts_the_one_new_id_for_this_cwd(monkeypatch) -> None:
    monkeypatch.setattr(
        mux_spawn,
        "_codex_session_ids_loaded",
        lambda cwd: {SID_A, SID_B},
    )
    assert mux_spawn._codex_daemon_candidate(Path("/w/proj"), {SID_B}) == SID_A


def test_daemon_bind_refuses_two_new_ids_as_ambiguous(monkeypatch) -> None:
    monkeypatch.setattr(
        mux_spawn,
        "_codex_session_ids_loaded",
        lambda cwd: {SID_A, SID_B},
    )
    assert mux_spawn._codex_daemon_candidate(Path("/w/proj"), set()) is None


def test_daemon_bind_returns_none_when_daemon_is_unavailable(monkeypatch) -> None:
    # None distinct from an empty set: the daemon could not answer, so the fd
    # oracle alone must decide - a false "nothing new" would be worse than no
    # answer at all.
    monkeypatch.setattr(mux_spawn, "_codex_session_ids_loaded", lambda cwd: None)
    assert mux_spawn._codex_daemon_candidate(Path("/w/proj"), set()) is None


def test_session_ids_loaded_filters_by_cwd_and_distinguishes_none_from_empty(
    monkeypatch,
) -> None:
    from fno.agents import discover as _discover

    monkeypatch.setattr(_discover, "_codex_daemon_threads_raw", lambda: None)
    assert mux_spawn._codex_session_ids_loaded(Path("/w/proj")) is None

    monkeypatch.setattr(
        _discover,
        "_codex_daemon_threads_raw",
        lambda: [
            {"session_id": SID_A, "cwd": "/w/proj"},
            {"session_id": SID_B, "cwd": "/w/other"},
        ],
    )
    assert mux_spawn._codex_session_ids_loaded(Path("/w/proj")) == {SID_A}
    assert mux_spawn._codex_session_ids_loaded(Path("/w/other")) == {SID_B}
    assert mux_spawn._codex_session_ids_loaded(Path("/w/nothing-here")) == set()


def test_session_ids_loaded_selects_the_requested_codex_home(monkeypatch) -> None:
    from fno.agents import discover as _discover

    seen: dict[str, str] = {}

    def threads(*, env):
        seen["codex_home"] = env["CODEX_HOME"]
        return []

    monkeypatch.setattr(_discover, "_codex_daemon_threads_raw", threads)

    assert mux_spawn._codex_session_ids_loaded(
        Path("/w/proj"), codex_home=Path("/tmp/canary-home")
    ) == set()
    assert seen["codex_home"] == "/tmp/canary-home"


# ---------------------------------------------------------------------------
# _make_codex_bind_probe: the daemon oracle is stability-gated (the same
# candidate must repeat across two consecutive probes) and liveness-checked
# (a daemon-sourced id is not itself proof the pane is up) before it binds.
# ---------------------------------------------------------------------------


def _probe_kwargs(monkeypatch, **overrides):
    monkeypatch.setattr(mux_spawn, "_CODEX_DAEMON_PROBE_INTERVAL_S", 0.0)
    monkeypatch.setattr(
        mux_spawn, "_backfill_codex_session_id", lambda *a, **k: None
    )
    kwargs = dict(
        cwd=Path("/w/proj"),
        spawn_started_ms=0,
        child_pid=4242,
        codex_sessions_dir=None,
        daemon_baseline_ids=set(),
        mux={"session": "main", "pane_id": 7},
        runner=lambda *a, **k: None,
    )
    kwargs.update(overrides)
    return kwargs


def test_daemon_candidate_is_not_trusted_on_first_observation(monkeypatch) -> None:
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: SID_A)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *a, **k: True)
    probe = mux_spawn._make_codex_bind_probe(**_probe_kwargs(monkeypatch))
    assert probe() is None


def test_daemon_candidate_binds_once_it_repeats_and_the_pane_is_alive(monkeypatch) -> None:
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: SID_A)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *a, **k: True)
    oracle_used: list = []
    probe = mux_spawn._make_codex_bind_probe(
        **_probe_kwargs(monkeypatch, oracle_used=oracle_used)
    )
    assert probe() is None
    assert probe() == SID_A
    assert oracle_used == ["daemon"]


def test_daemon_candidate_changing_between_probes_never_binds(monkeypatch) -> None:
    seen = iter([SID_A, SID_B, SID_B])
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: next(seen))
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *a, **k: True)
    probe = mux_spawn._make_codex_bind_probe(**_probe_kwargs(monkeypatch))
    assert probe() is None  # SID_A observed, nothing to compare yet
    assert probe() is None  # SID_B != SID_A, restarts the stability count
    assert probe() == SID_B  # SID_B repeats, pane alive -> trusted


def test_a_repeated_daemon_candidate_refuses_to_bind_a_dead_pane(monkeypatch) -> None:
    monkeypatch.setattr(mux_spawn, "_codex_daemon_candidate", lambda *a, **k: SID_A)
    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda *a, **k: False)
    probe = mux_spawn._make_codex_bind_probe(**_probe_kwargs(monkeypatch))
    assert probe() is None
    assert probe() is None
