"""x-3f84 W2: row -> session-process resolution, tree RSS, and its census/roster
joins. A bg row's recorded pid names the PTY HOST; the rv socket farm names the
process that IS the session."""
from __future__ import annotations

import json
import os
from types import SimpleNamespace

import pytest


@pytest.fixture(autouse=True)
def _empty_rv_farm(tmp_path, monkeypatch):
    """Point the socket-farm root at an empty tmp dir so no test on this machine
    joins the operator's REAL live bg sessions."""
    monkeypatch.setenv("FNO_CC_DAEMON_RV_ROOT", str(tmp_path / "rv-farm"))


_LSOF_OUT = (
    "p37355\n"
    "f13\n"
    "n/tmp/cc-daemon-501/608d3bdb/rv/119e3c52.sock\n"
    "f17\n"
    "n/tmp/cc-daemon-501/608d3bdb/rv/119e3c52.sock\n"
    "p58529\n"
    "f15\n"
    "n/tmp/cc-daemon-501/608d3bdb/rv/040f5e88.sock\n"
)


def test_pid_map_parses_stem_to_holder():
    from fno.agents.session_procs import _pid_map_from_lsof

    m = _pid_map_from_lsof(_LSOF_OUT)
    # Two fds on one socket collapse to one stem -> one pid (first wins).
    assert m == {"119e3c52": 37355, "040f5e88": 58529}


def test_pid_map_ignores_noise_lines():
    from fno.agents.session_procs import _pid_map_from_lsof

    m = _pid_map_from_lsof("pnotanumber\nf1\nn/x/rv/deadbeef.sock\n")
    assert m == {}


def _farm(tmp_path):
    root = tmp_path / "farm"
    (root / "608d3bdb" / "rv").mkdir(parents=True)
    (root / "608d3bdb" / "rv" / "119e3c52.sock").touch()
    return root


def test_bg_socket_pid_map_one_lsof_for_the_farm(tmp_path, monkeypatch):
    from fno.agents import session_procs

    calls: list[list[str]] = []

    def fake_run(argv, **kw):
        calls.append(argv)
        return SimpleNamespace(returncode=0, stdout=_LSOF_OUT)

    monkeypatch.setattr(session_procs.subprocess, "run", fake_run)
    m = session_procs.bg_socket_pid_map(_farm(tmp_path))
    assert m["119e3c52"] == 37355  # the one socket the farm holds
    assert len(calls) == 1  # one batched call, never one lsof per socket
    assert "--" in calls[0] and any("119e3c52.sock" in a for a in calls[0])


def test_bg_socket_pid_map_empty_farm_calls_nothing(tmp_path, monkeypatch):
    from fno.agents import session_procs

    def boom(*a, **kw):
        raise AssertionError("lsof must not run for an empty farm")

    monkeypatch.setattr(session_procs.subprocess, "run", boom)
    assert session_procs.bg_socket_pid_map(tmp_path / "nope") == {}


def test_bg_socket_pid_map_lsof_death_is_empty(tmp_path, monkeypatch):
    from fno.agents import session_procs

    def fake_run(argv, **kw):
        raise session_procs.subprocess.SubprocessError("no lsof here")

    monkeypatch.setattr(session_procs.subprocess, "run", fake_run)
    assert session_procs.bg_socket_pid_map(_farm(tmp_path)) == {}


def test_resolve_session_pid_prefers_socket_over_host_pid():
    from fno.agents.session_procs import resolve_session_pid

    got = resolve_session_pid(
        harness="claude", short_id="119e3c52", pid=98779,
        socket_map={"119e3c52": 37355},
    )
    assert got == 37355


def test_resolve_session_pid_falls_back_on_miss():
    from fno.agents.session_procs import resolve_session_pid

    assert (
        resolve_session_pid(
            harness="claude", short_id="ffffffff", pid=98779, socket_map={}
        )
        == 98779
    )
    # Non-claude harnesses have no host/session split: recorded pid verbatim.
    assert (
        resolve_session_pid(
            harness="codex", short_id="119e3c52", pid=42, socket_map={"119e3c52": 37355}
        )
        == 42
    )


class _FakeProc:
    def __init__(self, rss, children=()):
        self._rss = rss
        self._children = children

    def memory_info(self):
        return SimpleNamespace(rss=self._rss)

    def children(self, recursive=True):
        assert recursive, "cost is the TREE; a False here re-opens the undercount"
        return self._children


class _FakePsutil:
    class Error(Exception):
        pass

    @staticmethod
    def Process(pid):  # noqa: N802 - mirrors the real API surface
        # The session process plus the per-session MCP servers it forks.
        return _FakeProc(300 * 1024 * 1024, [_FakeProc(80 * 1024 * 1024)])

    @staticmethod
    def ZombieProcessError(*a, **kw):  # an alias callers may catch
        return _FakePsutil.Error()


def test_tree_rss_sums_children_recursive():
    from fno.agents.session_procs import tree_rss_mb

    assert tree_rss_mb(37355, _psutil=_FakePsutil) == 380


def test_tree_rss_none_for_dead_pid():
    from fno.agents.session_procs import tree_rss_mb

    class _DeadPsutil:
        @staticmethod
        def Process(pid):
            raise _FakePsutil.Error("gone")

    assert tree_rss_mb(999, _psutil=_DeadPsutil) is None
    assert tree_rss_mb(None, _psutil=_FakePsutil) is None


# ---------------------------------------------------------------------------
# the joins: census rows and roster discovery carry the resolved pid
# ---------------------------------------------------------------------------


def test_census_bg_row_resolves_to_session_pid(tmp_path, monkeypatch):
    """The specimen: a live registry row whose pid is the PTY HOST gets the
    socket-holder pid, while `pid` keeps the recorded host pid."""
    import fno.agents.session_procs as sp
    from fno.agents.registry import AgentEntry
    from fno.agents.spawn_gate import census

    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setattr(sp, "bg_socket_pid_map", lambda root=None: {"55f9847a": 37355})
    rows = [
        AgentEntry(
            name="t-xb57a-glm",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/l",
            status="live",
            # The host pid must itself be alive or the row reads dead; a live
            # host pid here stands in for the live PTY HOST of the specimen.
            pid=os.getpid(),
            short_id="55f9847a",
        )
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    w = next(w for w in census().workers if w.name == "t-xb57a-glm")
    assert w.pid == os.getpid()  # recorded, kept
    assert w.session_pid == 37355  # the process that IS the session


def test_top_prices_tree_rss_off_the_resolved_pid(tmp_path, monkeypatch):
    import fno.agents.session_procs as sp
    from fno.agents.registry import AgentEntry

    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setattr(sp, "bg_socket_pid_map", lambda root=None: {"55f9847a": 37355})
    monkeypatch.setattr(
        "fno.agents.top.tree_rss_mb", lambda pid, _psutil=None: 380 if pid == 37355 else None
    )
    rows = [
        AgentEntry(
            name="t-xb57a-glm",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/l",
            status="live",
            pid=os.getpid(),  # live host pid; the socket map resolves the session
            short_id="55f9847a",
        )
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(agents_app, ["top", "--json"])
    assert result.exit_code == 0, result.output
    row = next(w for w in json.loads(result.output)["workers"] if w["name"] == "t-xb57a-glm")
    assert row["pid"] == 37355  # the column shows the session, not its host
    assert row["rss_mb"] == 380  # and prices its tree


def test_roster_sessions_overlays_session_pid(tmp_path, monkeypatch):
    import fno.agents.session_procs as sp
    from fno.agents.harnesses._claude_session_registry import roster_sessions

    daemon = tmp_path / "daemon"
    daemon.mkdir()
    roster = {
        "workers": {
            "119e3c52": {"sessionId": "119e3c52-62bf-43b4-b3c4-3c7ce659f802", "pid": 5},
        }
    }
    (daemon / "roster.json").write_text(json.dumps(roster))
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setattr(sp, "bg_socket_pid_map", lambda root=None: {"119e3c52": 37355})
    rows = roster_sessions()
    assert rows and rows[0]["pid"] == 37355
    # A join miss keeps the recorded pid (an interactive session has no socket).
    monkeypatch.setattr(sp, "bg_socket_pid_map", lambda root=None: {})
    assert roster_sessions()[0]["pid"] == 5
