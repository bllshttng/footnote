"""Fixtures shared by the unit suite."""
from __future__ import annotations

import pytest

from fno import test_runner


@pytest.fixture
def no_native_owner(monkeypatch):
    # A dev machine has the deployed fno-agents on PATH; the native test-run
    # owner would wrap the argv these tests pin.
    monkeypatch.setattr(test_runner, "_native_owner_binary", lambda: None)

@pytest.fixture(autouse=True)
def _no_zero_job_reads(monkeypatch):
    """No unit test spawns the fno-agents binary: the zero-job read is a
    subprocess round-trip, so it stubs quiet unless a test restores the
    real wrapper (captured at import in test_pr_rest.py)."""
    from fno.pr import _rest

    monkeypatch.setattr(_rest, "_zero_job_rows", lambda *a, **k: ([], ""))
