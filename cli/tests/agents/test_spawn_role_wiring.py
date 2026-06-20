"""dispatch_spawn / cmd_spawn thread the routing role to the create path (x-d2fe).

The provider boundary (bg_create) is covered by test_provider_role_routing.py;
these guards pin the wiring above it so a future refactor cannot silently drop
the ``role`` kwarg between the CLI flag and the claude create path.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Dict

import pytest

from fno.paths_testing import use_tmpdir


def _setup_tmp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    for k in ("FNO_AGENT_SELF", "FNO_AGENT_PROVIDER", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(k, raising=False)


def test_dispatch_spawn_threads_role_to_create_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn

    captured: Dict[str, Any] = {}

    def fake_create(**kw: Any) -> DispatchAskResult:
        captured.update(kw)
        return DispatchAskResult(kind="create", short_id="abc12345")

    monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)

    result = dispatch_spawn(
        name="dreamer",
        message="consolidate memory",
        provider="claude",
        cwd=tmp_path,
        role="consolidate",
    )
    assert result.kind == "created"
    assert captured["role"] == "consolidate"


def test_dispatch_spawn_defaults_role_to_none(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn

    captured: Dict[str, Any] = {}

    def fake_create(**kw: Any) -> DispatchAskResult:
        captured.update(kw)
        return DispatchAskResult(kind="create", short_id="abc12345")

    monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)

    dispatch_spawn(
        name="builder",
        message="build it",
        provider="claude",
        cwd=tmp_path,
    )
    # Regression guard: the default spawn passes role=None (today's behavior).
    assert captured["role"] is None
