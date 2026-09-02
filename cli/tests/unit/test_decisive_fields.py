from __future__ import annotations

import importlib
import sys
from collections.abc import Callable
from typing import Any

from fno.think_inspect import _node_summary


def _claims_decisive_field(field: str):
    def decorate(fn: Callable[..., Any]) -> Callable[..., Any]:
        setattr(fn, "decisive_field", field)
        return fn

    return decorate


@_claims_decisive_field("superseded_by")
def test_superseded_candidate_is_visible_to_the_consolidation_gate() -> None:
    assert _node_summary(
        {"id": "x-948e", "status": "ready", "superseded_by": "x-579e"}
    ) == {
        "id": "x-948e",
        "status": "ready",
        "superseded_by": "x-579e",
    }


def test_live_candidate_shape_stays_unchanged() -> None:
    assert _node_summary({"id": "x-live", "status": "ready"}) == {
        "id": "x-live",
        "status": "ready",
    }


def test_every_decisive_field_has_exactly_one_claimed_behavioral_test() -> None:
    try:
        decisive = importlib.import_module("fno.agents.decisive")
    except ModuleNotFoundError:
        decisive = None
    assert decisive is not None, "fno.agents.decisive must declare DECISIVE_FIELDS"
    declared = {entry.test: entry.field for entry in decisive.DECISIVE_FIELDS}
    claimed = {
        name: getattr(value, "decisive_field")
        for name, value in vars(sys.modules[__name__]).items()
        if callable(value) and hasattr(value, "decisive_field")
    }

    assert claimed == declared
