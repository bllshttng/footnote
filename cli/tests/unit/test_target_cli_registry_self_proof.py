"""The resolve-owned-identity proof policy and the launcher stamp (x-a0cd).

Measured live on a daemon-hosted codex thread under seatbelt: only self's
ppid is readable, so every walk inside init's script chain returns None and
the verb fell to collision-elimination, which rejected the worker's OWN
spawn-minted row. The fix is the launcher stamp - the `fno do target init`
CLI resolves the harness while ITS ppid read is still permitted and exports
FNO_SESSION_HARNESS/FNO_SESSION_PID for the deeper verb - not registry rows
as self-proof: a row proves only that some live session owns the id
(round-1 P1)."""

from fno.target_cli import _registry_self_proof

# The conftest's autouse _neutral_host_harness patches resolve_session_harness
# to None; the stamp tests below pin the REAL function, captured at collection
# time before any fixture runs (the same pattern the fno_py_cmd tests use).
from fno.claims import session_pid as _session_pid

_REAL_RESOLVE_HARNESS = _session_pid.resolve_session_harness


def _prove(**kwargs):
    kwargs.setdefault("true_harness", None)
    kwargs.setdefault("own_binding", None)
    kwargs.setdefault("owning_row_harness", lambda sid: None)
    return _registry_self_proof("codex", "thread-1", **kwargs)


def test_tree_proof_wins_when_harness_matches():
    assert _prove(true_harness="codex") is True


def test_tree_proof_contradicts_other_harness():
    assert _prove(true_harness="claude") is False


def test_own_binding_stamp_backed_by_same_harness_row_proves_self():
    assert _prove(
        own_binding=("codex", "thread-1"),
        owning_row_harness=lambda sid: "codex",
    ) is True


def test_own_binding_row_harness_disagreement_stays_unproven():
    assert _prove(
        own_binding=("codex", "thread-1"),
        owning_row_harness=lambda sid: "claude",
    ) is None


def test_row_agreement_without_stamp_or_tree_never_proves():
    """The round-1 P1 shape: a marker copied from another live same-harness
    session meets that session's row, and the row must stay collision
    evidence, never self-proof."""
    assert _prove(owning_row_harness=lambda sid: "codex") is None


def test_session_harness_stamp_honored_while_pid_alive(monkeypatch):
    monkeypatch.setattr(_session_pid, "resolve_session_harness", _REAL_RESOLVE_HARNESS)
    monkeypatch.setattr(_session_pid.psutil, "pid_exists", lambda pid: True)
    monkeypatch.setenv("FNO_SESSION_HARNESS", "codex")
    monkeypatch.setenv("FNO_SESSION_PID", "1234")
    assert _session_pid.resolve_session_harness() == "codex"


def test_session_harness_stamp_ignored_when_pid_dead(monkeypatch):
    monkeypatch.setattr(_session_pid, "resolve_session_harness", _REAL_RESOLVE_HARNESS)
    monkeypatch.setattr(_session_pid.psutil, "pid_exists", lambda pid: False)
    monkeypatch.setenv("FNO_SESSION_HARNESS", "codex")
    monkeypatch.setenv("FNO_SESSION_PID", "1234")
    assert _session_pid.resolve_session_harness(from_pid=-1) is None


def test_session_harness_stamp_ignored_for_unknown_harness(monkeypatch):
    monkeypatch.setattr(_session_pid, "resolve_session_harness", _REAL_RESOLVE_HARNESS)
    monkeypatch.setenv("FNO_SESSION_HARNESS", "not-a-harness")
    monkeypatch.setenv("FNO_SESSION_PID", "1234")
    assert _session_pid.resolve_session_harness() != "not-a-harness"
