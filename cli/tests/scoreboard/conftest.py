"""Shared fixtures for the scoreboard view tests.

Delivery classification is answered by the Rust keeper in production. These
tests judge the folds and the render, so the classifier seam is patched to
the test-side reference (see tests/_delivery_reference.py) - the same shape
as the _apply_graph_defaults hermetic patch, and for the same reason: CI's
changed-smoke and pip-only boxes have no worker binary.
"""

import pytest

from fno.scoreboard import fold
from tests._delivery_reference import reference_deliveries


@pytest.fixture(autouse=True)
def hermetic_deliveries(monkeypatch):
    monkeypatch.setattr(fold, "classify_deliveries", reference_deliveries)
