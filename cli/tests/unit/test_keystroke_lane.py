"""Keystroke-lane predicate (x-c24d Wave 2): the lane a registry row resolves to
and whether it is a prompt-line keystroke path. Mirrors _deliver_live's routing
order (mux first, then harness); a predicate that disagrees with the real router
is worse than none. The codex-daemon row asserting False is the load-bearing one.
"""
from __future__ import annotations

import pytest

from fno.agents.dispatch import keystroke_lane
from fno.agents.registry import AgentEntry


def _entry(**kw) -> AgentEntry:
    base: dict = {"name": "x", "harness": "claude", "cwd": "/w", "log_path": "", "status": "live"}
    base.update(kw)
    return AgentEntry(**base)


@pytest.mark.parametrize(
    "fields, expected_lane, expected_keystroke",
    [
        # 1. mux-hosted (any harness) -> a real text-then-CR pane paste.
        ({"harness": "claude", "mux": {"session": "s", "pane_id": 1}}, "mux-pane", True),
        # 2. mux wins over a non-claude harness (routing order: mux FIRST).
        ({"harness": "codex", "mux": {"session": "s", "pane_id": 1}}, "mux-pane", True),
        # 3. claude control.sock -> the one keystroke door (claude has no command RPC).
        ({"harness": "claude"}, "control.sock", True),
        # 4. codex daemon (turn/start RPC) -> NOT a keystroke lane.
        ({"harness": "codex"}, "codex-daemon", False),
        # 5. gemini daemon -> not a keystroke lane.
        ({"harness": "gemini"}, "gemini-daemon", False),
        # 6. opencode / unknown -> daemon lane, not a keystroke lane.
        ({"harness": "opencode"}, "opencode-daemon", False),
    ],
)
def test_keystroke_lane_table(fields, expected_lane, expected_keystroke):
    lane, is_keystroke = keystroke_lane(_entry(**fields))
    assert lane == expected_lane
    assert is_keystroke is expected_keystroke
