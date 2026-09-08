"""The x-18c4 bounded placement verification split: each failure cause gets
its own message, a missing row is re-listed once, and the fail-closed
contract (reap, no registry row) holds. Companion to
fno/agents/placement_verify.py."""

import json
import subprocess
from pathlib import Path
from typing import Optional

import pytest

from fno.paths_testing import use_tmpdir

from tests.agents.test_spawn_pane import FakeRunner, _spawn


def test_bounded_verification_absent_row_names_the_listing_not_wrong_tab(
    tmp_path: Path, monkeypatch
) -> None:
    # x-18c4: the pane was created (it gets reaped), yet the listing cannot
    # see it. That must read as an ABSENCE from a non-empty listing, never as
    # "wrong tab" - the two causes get different fixes.
    from fno.agents.mux_spawn import DispatchAskError
    from fno.agents.registry import load_registry

    listing = [
        {"pane_id": 5, "squad_id": 1, "tab_id": 12, "cwd": "/w", "child_pid": 4005},
        {"pane_id": 6, "squad_id": 1, "tab_id": 12, "cwd": "/w", "child_pid": 4006},
    ]
    runner = FakeRunner(ls_stdout=json.dumps(listing))
    with pytest.raises(DispatchAskError, match="absent from a 2-pane listing"):
        _spawn(monkeypatch, tmp_path, tab_id="id:12", runner=runner)
    assert runner.kill_calls
    assert load_registry() == []


def test_bounded_verification_empty_listing_is_not_evidence_of_a_tab(
    tmp_path: Path, monkeypatch
) -> None:
    # x-18c4: `pane ls --json` prints `[]` exit 0 for a refused/absent socket
    # too, so an empty listing verifies nothing and says so.
    from fno.agents.mux_spawn import DispatchAskError
    from fno.agents.registry import load_registry

    runner = FakeRunner(ls_stdout="[]")
    with pytest.raises(DispatchAskError, match="pane listing was empty"):
        _spawn(monkeypatch, tmp_path, tab_id="id:12", runner=runner)
    assert runner.kill_calls
    assert load_registry() == []


def test_bounded_verification_relists_once_when_the_pane_is_missing(
    tmp_path: Path, monkeypatch
) -> None:
    # x-18c4 cause two: a read-after-write race. One absent listing re-lists;
    # the second read seeing the pane keeps the worker alive.
    monkeypatch.setattr("time.sleep", lambda _s: None)

    class LateListingRunner(FakeRunner):
        def __init__(self, **kwargs) -> None:
            super().__init__(**kwargs)
            self.ls_calls = 0

        def __call__(self, argv, **kwargs):
            if argv[1:4] == ["mux", "pane", "ls"]:
                self.ls_calls += 1
                if self.ls_calls == 2:
                    return subprocess.CompletedProcess(argv, 0, "[]", "")
            return super().__call__(argv, **kwargs)

    runner = LateListingRunner(
        ls_stdout=json.dumps(
            [{"pane_id": 7, "squad_id": 1, "tab_id": 12, "cwd": "/w", "child_pid": 4242}]
        )
    )
    _spawn(monkeypatch, tmp_path, tab_id="id:12", runner=runner)
    assert runner.ls_calls >= 3
    assert runner.kill_calls == []


def test_bounded_verification_receipt_disagreement_names_the_server_answer(
    tmp_path: Path, monkeypatch
) -> None:
    # x-18c4: when the server receipt reports a landing tab that differs from
    # the requested one (the TooSmall fallback redirect), the refusal says so
    # instead of leaving the redirect invisible.
    from fno.agents.mux_spawn import DispatchAskError

    listing = [
        {"pane_id": 7, "squad_id": 1, "tab_id": 55, "tab_name": None, "cwd": "/w",
         "child_pid": 4005},
    ]
    runner = FakeRunner(
        ls_stdout=json.dumps(listing),
        placement={"anchor": 0, "direction": "down", "fallback": "new_tab",
                   "squad": 1, "tab": 55},
    )
    with pytest.raises(DispatchAskError, match="server receipt reports it landed in tab 55"):
        _spawn(monkeypatch, tmp_path, tab_id="id:12", runner=runner)
