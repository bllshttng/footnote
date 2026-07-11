from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.agents.dispatch import DispatchAskError
from fno.agents.mux_spawn import (
    apply_opencode_variant,
    build_pane_argv,
    effort_tokens,
)

CWD = Path("/tmp")


@pytest.mark.parametrize(
    "provider,value,expected",
    [
        ("claude", "high", ["--effort", "high"]),
        ("codex", "medium", ["-c", "model_reasoning_effort=medium"]),
        ("opencode", "max", []),
    ],
)
def test_effort_mapping_accepts_native_values(provider, value, expected):
    assert effort_tokens(provider, value) == expected


@pytest.mark.parametrize(
    "provider,value",
    [("codex", "xhigh"), ("claude", "minimal"), ("gemini", "high"), ("agy", "low"), ("claude", "")],
)
def test_effort_mapping_fails_closed(provider, value):
    with pytest.raises(DispatchAskError) as exc:
        effort_tokens(provider, value)
    assert exc.value.exit_code == 2


def test_pane_argv_threads_effort_and_unset_is_noop():
    claude = build_pane_argv("claude", "hi", CWD, False, "uuid", effort="high")
    codex = build_pane_argv("codex", "hi", CWD, False, None, effort="medium")
    assert claude[-3:] == ["--effort", "high", "hi"]
    assert ["-c", "model_reasoning_effort=medium"] == codex[5:7]
    assert build_pane_argv("claude", "hi", CWD, False, "uuid", effort=None) == build_pane_argv(
        "claude", "hi", CWD, False, "uuid"
    )


def test_opencode_variant_write_and_corrupt_degrade(tmp_path, capsys):
    state = tmp_path / "model.json"
    state.write_text(json.dumps({"variant": {"other/model": "low"}}))
    apply_opencode_variant("opencode/glm-4.7-free", "high", state_path=state)
    assert json.loads(state.read_text())["variant"] == {
        "other/model": "low",
        "opencode/glm-4.7-free": "high",
    }
    state.write_text("{")
    before = state.read_bytes()
    apply_opencode_variant("opencode/glm-4.7-free", "high", state_path=state)
    assert state.read_bytes() == before
    assert "warning" in capsys.readouterr().err.lower()


def test_claude_python_build_argv_threads_effort():
    from fno.agents.providers.claude import _build_argv

    assert _build_argv("w", "hi", False, effort="high") == [
        "claude",
        "--bg",
        "--name",
        "w",
        "--effort",
        "high",
        "hi",
    ]
