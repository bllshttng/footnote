"""The resolve-owned-identity verb resolves through claims.self_identity (x-0992).

History this file used to pin: x-a0cd measured a daemon-hosted codex thread
under seatbelt where only self's ppid is readable, so every walk inside init's
script chain returned None and the verb fell to collision-elimination, which
rejected the worker's OWN spawn-minted row. The launcher stamp
(FNO_SESSION_HARNESS/FNO_SESSION_PID, exported by the `fno do target init` CLI
while ITS ppid read is still permitted) fixed the walk half. x-0992 measured
the other half: a PANE-spawned codex worker carries no FNO_HARNESS_SESSION_ID
at all (the pane row is written after the child starts), so its stamp is
name_only and the verb's former private own_binding construction - gated on a
COMPLETE stamp - was always None. The verb now routes through
resolve_self_identity, the one owned-identity implementation, and these tests
pin the verb-level contract the hook parses."""

from typer.testing import CliRunner

from fno.cli import app

# The conftest's autouse _neutral_host_harness patches resolve_session_harness
# to None; the stamp tests below pin the REAL function, captured at collection
# time before any fixture runs (the same pattern the fno_py_cmd tests use).
from fno.claims import session_pid as _session_pid

_REAL_RESOLVE_HARNESS = _session_pid.resolve_session_harness

runner = CliRunner()


def _fields(result):
    return {
        line.split("=", 1)[0]: line.split("=", 1)[1]
        for line in result.stdout.splitlines()
        if "=" in line
    }


def _silent_walk_and_attester(monkeypatch, attested_id: str):
    """Pin the sandbox shape: no harness ancestor to walk, so the attester's
    witness stays env_only while it still names the env's own marker value -
    exactly what a bare runner (and a seatbelt sandbox) produces for a single
    codex family. A real runner's ancestry may or may not be readable, and the
    verb's answer must not depend on which."""
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: None
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_attester_identity",
        lambda env=None: (attested_id, "env_only"),
    )


def test_name_only_pane_stamp_resolves_without_row_or_proof(tmp_path, monkeypatch):
    """The x-0992 repro, pinned: a pane-spawned codex worker's environment
    (name_only stamp, both codex markers carrying the same uuid, no walk, no
    row) resolves to HARNESS=codex with a non-empty SESSION_ID and no
    COLLISION. The pre-fix verb answered ambiguous here, which stamped
    harness=unknown/provider=claude onto a codex worker's manifest."""
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    _silent_walk_and_attester(monkeypatch, mine)
    monkeypatch.setenv("FNO_HARNESS_NAME", "codex")
    monkeypatch.setenv("CODEX_THREAD_ID", mine)
    monkeypatch.setenv("CODEX_SESSION_ID", mine)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "codex"
    assert fields["SESSION_ID"] == mine
    assert fields["DISPOSITION"] == "single"
    assert fields["COLLISION"] == ""


def test_name_only_pane_stamp_own_row_is_not_contention(tmp_path, monkeypatch):
    """The measured x-77be shape: the spawn-minted registry row already holds
    the pane worker's id when the verb runs. The name_only stamp plus the
    same-family marker complete the (codex, id) pair, the row agrees on both
    halves, and the identity resolves instead of being refused as a competing
    holder."""
    from fno.agents.registry import register_existing_session
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    register_existing_session(harness="codex", session_id=mine, cwd="/x")
    _silent_walk_and_attester(monkeypatch, mine)
    monkeypatch.setenv("FNO_HARNESS_NAME", "codex")
    monkeypatch.setenv("CODEX_THREAD_ID", mine)
    monkeypatch.setenv("CODEX_SESSION_ID", mine)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "codex"
    assert fields["SESSION_ID"] == mine
    assert fields["COLLISION"] == ""


def test_session_harness_stamp_honored_while_pid_alive(monkeypatch):
    monkeypatch.setattr(_session_pid, "resolve_session_harness", _REAL_RESOLVE_HARNESS)
    monkeypatch.setattr(_session_pid.psutil, "pid_exists", lambda pid: True)
    monkeypatch.setenv("FNO_SESSION_HARNESS", "codex")
    monkeypatch.setenv("FNO_SESSION_PID", "1234")
    assert _session_pid.resolve_session_harness() == "codex"


def test_session_harness_stamp_ignored_when_pid_dead(monkeypatch):
    monkeypatch.setattr(_session_pid, "resolve_session_harness", _REAL_RESOLVE_HARNESS)
    monkeypatch.setattr(_session_pid.psutil, "pid_exists", lambda pid: False)
    monkeypatch.setattr(_session_pid, "_harness_name_of", lambda _proc: None)
    monkeypatch.setenv("FNO_SESSION_HARNESS", "codex")
    monkeypatch.setenv("FNO_SESSION_PID", "1234")
    assert _session_pid.resolve_session_harness(from_pid=-1) is None


def test_session_harness_stamp_ignored_for_unknown_harness(monkeypatch):
    monkeypatch.setattr(_session_pid, "resolve_session_harness", _REAL_RESOLVE_HARNESS)
    monkeypatch.setenv("FNO_SESSION_HARNESS", "not-a-harness")
    monkeypatch.setenv("FNO_SESSION_PID", "1234")
    assert _session_pid.resolve_session_harness() != "not-a-harness"
