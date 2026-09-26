"""Fixtures shared by the unit suite."""
from __future__ import annotations

import pytest

from fno import test_runner


@pytest.fixture
def no_native_owner(monkeypatch):
    # A dev machine has the deployed fno-agents on PATH; the native test-run
    # owner would wrap the argv these tests pin.
    monkeypatch.setattr(test_runner, "_native_owner_binary", lambda: None)
