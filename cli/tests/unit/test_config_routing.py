"""config.routing: the declared inventory block + the shipped sample.

Config stays a leaf module (x-7fdd): no validation against the harness map at
load time, an unknown objective degrades to the default, and the sample file
no code path reads must still parse and carry the fields the schema names.
"""
from __future__ import annotations

import tomllib
from pathlib import Path

from fno.config import RoutingBlock, RoutingModelBlock, SettingsModel

_REPO_ROOT = Path(__file__).resolve().parents[3]
_SAMPLE = _REPO_ROOT / "examples" / "routing.toml"


def _settings(payload: dict) -> SettingsModel:
    return SettingsModel.model_validate(payload)


def test_empty_routing_block_defaults():
    s = _settings({})
    assert s.routing.objective == "cheapest-that-clears"
    assert s.routing.prefer_harness == ""
    assert s.routing.models == []


def test_full_routing_block_parses():
    s = _settings({
        "routing": {
            "objective": "prefer-harness",
            "prefer_harness": "claude",
            "models": [
                {
                    "name": "glm-5.3", "harness": "claude", "model": "glm-5.3",
                    "band": "medium", "effort": "high",
                    "cost_per_mtok_in": 6.9, "context": 1000000,
                    "route": "zai/glm-5.3", "account": "makers",
                },
            ],
        },
    })
    row = s.routing.models[0]
    assert row.name == "glm-5.3"
    assert row.harness == "claude"
    assert row.cost_per_mtok_in == 6.9
    assert row.context == 1000000
    assert row.route == "zai/glm-5.3"
    assert row.account == "makers"
    assert s.routing.objective == "prefer-harness"


def test_unknown_objective_degrades_to_the_default():
    s = _settings({"routing": {"objective": "fastest"}})
    assert s.routing.objective == "cheapest-that-clears"


def test_rows_carry_no_validation_at_load_time():
    """Leaf module: a nonsense harness/band loads fine; the spawn seam refuses."""
    s = _settings({"routing": {"models": [
        {"name": "x", "harness": "not-a-harness", "model": "m", "band": "purple"},
    ]}})
    assert s.routing.models[0].band == "purple"


def test_shipped_sample_parses_and_declares_rows():
    """The labelled sample is documentation, but it must not drift from the
    schema: every [[routing.models]] row carries name, harness and model."""
    assert _SAMPLE.is_file()
    data = tomllib.loads(_SAMPLE.read_text(encoding="utf-8"))
    routing = data["routing"]
    assert routing["objective"] in ("cheapest-that-clears", "best-available", "prefer-harness")
    assert isinstance(routing["models"], list) and routing["models"]
    for row in routing["models"]:
        assert row.get("name") and row.get("harness") and row.get("model"), row
    # the sample labels itself as read by no code path
    assert "NO CODE PATH READS THIS FILE" in _SAMPLE.read_text(encoding="utf-8")


def test_routing_model_block_defaults():
    row = RoutingModelBlock()
    assert row.name == "" and row.harness == "" and row.model == ""
    assert row.band == "" and row.effort == ""
    assert row.cost_per_mtok_in is None and row.context is None


def test_routing_block_tolerates_extra_keys():
    block = RoutingBlock.model_validate({"objective": "best-available", "future_key": 1})
    assert block.objective == "best-available"
