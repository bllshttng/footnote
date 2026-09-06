"""x-8bfb wave 2, task 2.1: `_LIVE_STATUS_INPUT` gains the terminal `stopped`
state, and the unrecognized-status WARN dedupes to one line per distinct
value per call rather than one per row.

Measured on a real fleet: `stopped` fired 5 WARN lines from 5 rows, all
naming the same unrecognized value. The node's own VERIFY step counts WARN
SHAPES, not rows, so a fix that still emits one line per row does not
satisfy it even once the value is mapped.
"""
from __future__ import annotations

import json
from types import SimpleNamespace

from fno.agents.harnesses import claude as claude_mod


def _fake_completed(stdout: str = "", stderr: str = "", returncode: int = 0):
    return SimpleNamespace(stdout=stdout, stderr=stderr, returncode=returncode)


def test_stopped_maps_to_done_with_no_warning(monkeypatch):
    # AC5-HP + AC9-FR: `stopped` is now IN the input vocabulary. The resolved
    # value changes (raw 'stopped' -> 'Done', the one intended resolution
    # change wave 2 makes) and no unrecognized-status WARN fires for it.
    payload = [{"id": "aaaaaaa1", "state": "stopped"}]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {"aaaaaaa1": {"live_status": "Done"}}
    assert warnings == [], warnings


def test_failed_still_maps_to_done(monkeypatch):
    # Regression: `failed` already mapped to `Done` before this task; wave 2
    # must not disturb it while adding `stopped` alongside it.
    payload = [{"id": "aaaaaaa2", "state": "failed"}]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {"aaaaaaa2": {"live_status": "Done"}}
    assert warnings == [], warnings


def test_unrecognized_value_warns_once_regardless_of_row_count(monkeypatch):
    # AC7-EDGE: a genuinely unknown state on many rows still passes through
    # unchanged for every row, but the fleet's stderr carries exactly ONE
    # WARN naming it - not one per row.
    payload = [{"id": f"row{i:04d}", "state": "flibberty"} for i in range(5)]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert all(row["live_status"] == "flibberty" for row in result.values())
    assert len(result) == 5
    matches = [w for w in warnings if "unrecognized status=" in w and "flibberty" in w]
    assert len(matches) == 1, warnings


def test_two_distinct_unknown_values_get_two_warnings(monkeypatch):
    # The dedup key is the VALUE, not "any unknown value" - two distinct
    # unmapped values still get their own line each, each deduped internally.
    payload = [
        {"id": "aaaaaaa1", "state": "stuck"},
        {"id": "aaaaaaa2", "state": "stuck"},
        {"id": "aaaaaaa3", "state": "wedged"},
    ]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    _, warnings = claude_mod.claude_agents_json()

    stuck = [w for w in warnings if "'stuck'" in w]
    wedged = [w for w in warnings if "'wedged'" in w]
    assert len(stuck) == 1, warnings
    assert len(wedged) == 1, warnings
