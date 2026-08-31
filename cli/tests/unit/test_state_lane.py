from __future__ import annotations

from pathlib import Path

import pytest

from fno.test_cmd import (
    STATE_LEAK_CANARY,
    _parse_smoke_args,
    _populate_state,
    _state_both_exit,
    _state_verdict_diff,
)


def test_state_mode_parser_accepts_the_three_state_shapes():
    assert _parse_smoke_args(["--state", "clean"])["state"] == "clean"
    assert _parse_smoke_args(["--state=populated"])["state"] == "populated"
    assert _parse_smoke_args(["--state", "both"])["state"] == "both"


def test_state_mode_parser_rejects_unknown_shapes():
    with pytest.raises(ValueError, match="--state 'invalid'.*clean/populated/both"):
        _parse_smoke_args(["--state", "invalid"])


def test_populated_state_profile_has_positive_canary_and_state_channels(tmp_path: Path):
    sandbox = tmp_path / "sandbox"

    profile = _populate_state(sandbox)

    assert (profile / "README.md").is_file()
    assert (sandbox / "home" / ".fno" / "graph.json").is_file()
    assert (sandbox / "home" / ".fno" / "agents" / "registry.json").is_file()
    assert (sandbox / "home" / ".fno" / "claims" / "state-canary.lock").is_file()
    assert STATE_LEAK_CANARY in (
        sandbox / "home" / ".fno" / "graph.json"
    ).read_text(encoding="utf-8")


def test_state_verdict_diff_names_changed_test_nodeids(tmp_path: Path):
    clean = tmp_path / "clean.xml"
    populated = tmp_path / "populated.xml"
    clean.write_text(
        "<testsuite><testcase classname='tests.unit.test_state' name='stable'/></testsuite>",
        encoding="utf-8",
    )
    populated.write_text(
        "<testsuite><testcase classname='tests.unit.test_state' name='stable'/>"
        "<testcase classname='tests.unit.test_state_canary' name='canary'>"
        "<failure/></testcase></testsuite>",
        encoding="utf-8",
    )

    diff = _state_verdict_diff(clean, populated)

    assert diff == ["tests.unit.test_state_canary::canary"]


def test_state_both_exit_green_only_when_control_fires_and_diff_is_canary_only():
    canary = "tests.unit.test_state_canary::test_state_canary_detects_populated_state"
    other = "tests.unit.test_state::reads_live_graph"

    # Healthy run: control fired, only the canary changed. The populated lane
    # itself exited red on the canary, so the raw populated rc must NOT decide.
    assert _state_both_exit(0, [canary], "passed", "failed") == 0
    # A real leak: some testcase other than the canary changed verdict.
    assert _state_both_exit(0, [canary, other], "passed", "failed") == 1
    # Control disarmed or never ran: never green.
    assert _state_both_exit(0, [canary], "passed", "passed") == 1
    assert _state_both_exit(0, [canary], None, "failed") == 1
    # Empty verdict set is unmeasurable, never green.
    assert _state_both_exit(0, [], "passed", "failed") == 1
    # A red clean lane is red, whatever the diff says.
    assert _state_both_exit(3, [canary], "passed", "failed") == 3
