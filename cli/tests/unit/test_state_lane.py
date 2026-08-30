from __future__ import annotations

from pathlib import Path

import pytest

from fno.test_cmd import (
    STATE_LEAK_CANARY,
    _parse_smoke_args,
    _populate_state,
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
