"""Unit tests for fno doctor's single blocker authority, `_blockers()`.

One pure function reading the assembled `result` dict that `fno doctor` and
the setup wizard both call (x-75dc): see `_emit_blockers` in doctor.py and
`report_machine_blockers` in setup_cli.py.
"""
from __future__ import annotations

import pytest

from fno.doctor import _blockers


def _clean_result(**overrides):
    base = {
        "status": "fresh",
        "launch_agents": {"applicable": True, "dead": []},
        "archive_id_collisions": {"count": 0, "ids": []},
        "fd_limit": {"verdict": "ok"},
        "plugin_hooks": {"failed": 0},
        "plugin_cache": {"status": "fresh"},
    }
    base.update(overrides)
    return base


def test_clean_machine_returns_no_blockers():
    assert _blockers(_clean_result()) == []


def test_missing_report_keys_do_not_raise_and_produce_no_lines():
    # A report that never ran (e.g. the non-darwin launch_agents probe) is
    # absent evidence, not a blocker - the never-cry-wolf rule that governs
    # _verdict governs this too.
    assert _blockers({}) == []
    assert _blockers({"launch_agents": {}}) == []


@pytest.mark.parametrize(
    "overrides, expect_substring",
    [
        ({"status": "stale"}, "behind source"),
        (
            {
                "launch_agents": {
                    "applicable": True,
                    "dead": [{"label": "sh.fno.groom", "exit": 1}],
                }
            },
            "sh.fno.groom",
        ),
        ({"archive_id_collisions": {"count": 29, "ids": []}}, "29 node id"),
        (
            {"archive_id_collisions": {"count": 0, "ids": [], "unreadable": True}},
            "unreadable",
        ),
        ({"fd_limit": {"verdict": "low", "launchd_soft": 256}}, "256"),
        ({"plugin_hooks": {"failed": 2}}, "2 plugin hook"),
        ({"plugin_cache": {"status": "stale"}}, "plugin cache is stale"),
    ],
)
def test_each_signal_produces_a_blocker_line(overrides, expect_substring):
    result = _clean_result(**overrides)
    blockers = _blockers(result)
    assert len(blockers) == 1
    assert expect_substring in blockers[0]


def test_fd_limit_low_alone_is_exactly_one_line_naming_the_reading():
    """AC from the plan: fd_limit.verdict == 'low' with nothing else set
    returns exactly one line, and that line contains the launchd soft
    reading."""
    result = {"fd_limit": {"verdict": "low", "launchd_soft": 256}}
    blockers = _blockers(result)
    assert len(blockers) == 1
    assert "256" in blockers[0]


def test_multiple_blockers_are_all_named_in_order():
    result = _clean_result(
        status="stale",
        plugin_cache={"status": "stale"},
    )
    blockers = _blockers(result)
    assert len(blockers) == 2
    assert "behind source" in blockers[0]
    assert "plugin cache is stale" in blockers[1]
