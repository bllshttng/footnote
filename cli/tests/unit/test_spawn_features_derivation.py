"""The spawn roster derives from the capability table (x-a3e8).

One answer used to live in two places: a hardcoded tuple plus a prose
sentence duplicated in two refusal sites, none of which read a
capability. These tests pin the derivation, the derived pane-only list,
and the state-aware refusals at the spawn seam.
"""
from __future__ import annotations

import inspect
import tomllib
from importlib.resources import files

import pytest

from fno.agents.dispatch import DispatchAskError, _check_spawn_harness
from fno.harness_names import SPAWN_HARNESSES, pane_only_harnesses


def _packaged_table() -> dict:
    text = files("fno.agents").joinpath("harness_capabilities.toml").read_text(
        encoding="utf-8"
    )
    return tomllib.loads(text)["harness"]


def test_spawn_roster_equals_the_native_rows_in_the_table():
    derived = tuple(
        sorted(
            name
            for name, row in _packaged_table().items()
            if (row.get("features") or {}).get("spawn", {}).get("state") == "native"
        )
    )
    assert SPAWN_HARNESSES == derived


def test_spawn_roster_is_the_measured_six():
    # The measured answer, pinned so a silent drift in either direction
    # fails here rather than at a dispatch. pi/cursor-agent/grok carry
    # keeper journeys, opencode a wired launch seam; agy and gemini are
    # the pane-only pair.
    assert SPAWN_HARNESSES == (
        "claude", "codex", "cursor-agent", "grok", "opencode", "pi",
    )


def test_pane_only_roster_is_derived_not_hardcoded():
    assert pane_only_harnesses() == ("agy", "gemini")


def test_harness_names_stays_l0_with_no_fno_imports():
    # The derivation must not re-create the cross-layer edge the module's
    # docstring records as removed: the platform layer reads the packaged
    # TOML file, never fno.agents.
    import fno.harness_names as module

    source = inspect.getsource(module)
    assert "import fno" not in source
    assert "from fno" not in source


def test_spawn_seam_refuses_a_declared_pane_only_harness_naming_the_row():
    with pytest.raises(DispatchAskError) as excinfo:
        _check_spawn_harness("agy")
    message = str(excinfo.value)
    assert "features.spawn" in message
    assert "'absent'" in message
    assert "--substrate pane" in message
    assert excinfo.value.exit_code == 2


def test_spawn_seam_says_unwired_for_capable_not_unable(monkeypatch):
    # capable and absent are different remedies and must refuse in
    # different words: capable is fno's gap (no wired arm), absent is the
    # harness's. No shipped row reads capable yet, so this one is staged
    # through the accessor the seam actually calls.
    import fno.agents.harness_map as harness_map

    monkeypatch.setattr(
        harness_map, "feature_claim", lambda name, key: "capable"
    )
    with pytest.raises(DispatchAskError) as excinfo:
        _check_spawn_harness("agy")
    message = str(excinfo.value)
    assert "unwired" in message
    assert "'capable'" in message


def test_spawn_seam_names_the_probe_for_an_unmeasured_lane(monkeypatch):
    import fno.agents.harness_map as harness_map

    monkeypatch.setattr(
        harness_map, "feature_claim", lambda name, key: "unmeasured"
    )
    with pytest.raises(DispatchAskError) as excinfo:
        _check_spawn_harness("agy")
    message = str(excinfo.value)
    assert "'unmeasured'" in message
    assert "harness probe" in message


def test_spawn_seam_passes_a_native_lane_with_a_measured_stance():
    # claude is native on spawn and its thread stance is measured, so the
    # seam answers nothing: a working lane stays a working lane.
    _check_spawn_harness("claude")


def test_spawn_seam_still_refuses_an_unmeasured_headless_stance():
    # The spawn STATE gate and the state_root_grant stance gate are two
    # different gates; derivation of the first must not lower the second.
    # pi's headless lane has never run.
    with pytest.raises(DispatchAskError) as excinfo:
        _check_spawn_harness("pi", headless=True)
    assert "state_root_grant" in str(excinfo.value)


def test_spawn_seam_refuses_an_undeclared_harness_naming_the_pane():
    with pytest.raises(DispatchAskError) as excinfo:
        _check_spawn_harness("kimi")
    message = str(excinfo.value)
    assert "declares no capability row" in message
    assert "--substrate pane" in message
