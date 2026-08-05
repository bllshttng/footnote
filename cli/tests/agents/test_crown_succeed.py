"""Tests for `fno agents crown --succeed` (x-7685, US6/AC11/AC12/AC13).

`--succeed` is a TRANSFER, not a grant: it moves the caller's live crown over
--scope onto a successor in one atomic write. The grant path's two refusals
(one-live-crown, not-a-superset) stay correct for grants, so AC11 pins that a
same-scope grant WITHOUT --succeed still exits 2 even when the caller holds the
crown. AC12 asserts the transfer by reading the registry before and after, not
by trusting the exit code. AC13 asserts the post-state after a refusal: a
refusal that already vacated the crown is the orphaning this feature prevents.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.cli import agents_app
from fno.agents.registry import AgentEntry, load_registry, write_registry
from fno.paths_testing import use_tmpdir

_SCOPE = "x-test-epic"
_CALLER = "king-caller"
_SUCC = "king-successor"


def _entry(name, *, short_id, status="live", crown_level=None, crown_scope=None,
           crown_grantor=None):
    return AgentEntry(
        name=name,
        harness="claude",
        cwd="/tmp",
        log_path=f"/tmp/{name}.log",
        short_id=short_id,
        status=status,
        crown_level=crown_level,
        crown_scope=crown_scope,
        crown_grantor=crown_grantor,
    )


@pytest.fixture
def reg(tmp_path: Path, monkeypatch):
    """Isolated registry with a crowned caller and a live successor."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([
        _entry(_CALLER, short_id="caller-short",
               crown_level=1, crown_scope=_SCOPE, crown_grantor="human"),
        _entry(_SUCC, short_id="succ-short", status="live"),
    ])
    monkeypatch.setenv("FNO_AGENT_SELF", _CALLER)
    return tmp_path


def _crown(*args):
    return CliRunner().invoke(agents_app, ["crown", *args])


def _row(name):
    return next(e for e in load_registry() if e.name == name)


# --- AC11: the refuted claim, made executable --------------------------------


def test_grant_without_succeed_still_refused_for_live_holder(reg):
    # x-f605 claimed succession works via a bare grant. It does not: a same-scope
    # grant by the live holder is refused (one-live-crown matches the holder's
    # own row). Pin it so the next reader does not re-derive the refutation.
    result = _crown(_SUCC, "--scope", _SCOPE)  # NO --succeed
    assert result.exit_code == 2
    # The caller's crown is untouched by the refusal.
    assert _row(_CALLER).crown_scope == _SCOPE
    assert _row(_SUCC).crown_level is None


# --- AC12: the transfer ------------------------------------------------------


def test_succeed_transfers_crown_atomically(reg):
    before_caller = _row(_CALLER)
    result = _crown(_SUCC, "--scope", _SCOPE, "--succeed")
    assert result.exit_code == 0, result.stdout + result.stderr
    caller = _row(_CALLER)
    succ = _row(_SUCC)
    # Caller vacated.
    assert caller.crown_level is None
    assert caller.crown_scope is None
    assert caller.crown_grantor is None
    # Successor inherits the caller's former altitude, scope unchanged, grantor
    # names the outgoing king (its session_id, which for claude is short_id).
    assert succ.crown_level == before_caller.crown_level == 1
    assert succ.crown_scope == _SCOPE
    assert succ.crown_grantor == "caller-short"
    # Exactly one live crown over the scope after the transfer.
    holders = [e for e in load_registry() if e.crown_scope == _SCOPE]
    assert len(holders) == 1 and holders[0].name == _SUCC


def test_succeed_level_defaults_to_caller_altitude(tmp_path, monkeypatch):
    # A VP (level 0) transfers as level 0, not +1: a successor inherits the rung.
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([
        _entry(_CALLER, short_id="c", crown_level=0, crown_scope=_SCOPE),
        _entry(_SUCC, short_id="s"),
    ])
    monkeypatch.setenv("FNO_AGENT_SELF", _CALLER)
    result = _crown(_SUCC, "--scope", _SCOPE, "--succeed")
    assert result.exit_code == 0, result.stdout + result.stderr
    assert _row(_SUCC).crown_level == 0
    assert _row(_CALLER).crown_level is None


def test_succeed_level_override_wins(reg):
    result = _crown(_SUCC, "--scope", _SCOPE, "--level", "2", "--succeed")
    assert result.exit_code == 0, result.stdout + result.stderr
    assert _row(_SUCC).crown_level == 2


# --- AC13: refusals leave the caller intact ----------------------------------


def test_succeed_refuses_when_caller_holds_no_crown(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_entry(_CALLER, short_id="c"), _entry(_SUCC, short_id="s")])
    monkeypatch.setenv("FNO_AGENT_SELF", _CALLER)
    result = _crown(_SUCC, "--scope", _SCOPE, "--succeed")
    assert result.exit_code == 2
    # Nothing was transferred.
    assert _row(_SUCC).crown_level is None


def test_succeed_refuses_dead_successor_and_leaves_caller_intact(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([
        _entry(_CALLER, short_id="c", crown_level=1, crown_scope=_SCOPE),
        _entry(_SUCC, short_id="s", status="exited"),
    ])
    monkeypatch.setenv("FNO_AGENT_SELF", _CALLER)
    result = _crown(_SUCC, "--scope", _SCOPE, "--succeed")
    assert result.exit_code == 2
    assert "not live" in result.stderr
    # The post-state is the point: a refusal must not have vacated the caller.
    caller = _row(_CALLER)
    assert caller.crown_level == 1
    assert caller.crown_scope == _SCOPE
    assert _row(_SUCC).crown_level is None


def test_succeed_refuses_nonexistent_handle(reg):
    result = _crown("ghost-handle", "--scope", _SCOPE, "--succeed")
    assert result.exit_code == 2
    assert _row(_CALLER).crown_scope == _SCOPE  # caller intact


def test_succeed_refuses_self(reg):
    result = _crown(_CALLER, "--scope", _SCOPE, "--succeed")
    assert result.exit_code == 2
    assert _row(_CALLER).crown_scope == _SCOPE  # cannot succeed itself


def test_succeed_refuses_wrong_scope(reg):
    # Caller holds the crown over _SCOPE, not over "some-other-scope".
    result = _crown(_SUCC, "--scope", "some-other-scope", "--succeed")
    assert result.exit_code == 2
    assert _row(_CALLER).crown_scope == _SCOPE
    assert _row(_SUCC).crown_level is None
