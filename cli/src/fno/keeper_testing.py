"""Keeper hygiene fixtures shared by BOTH pytest roots.

Bare ``pytest`` from ``cli/`` collects both trees (no ``testpaths``), and a
fixture in one conftest protects nothing in the other: the ``cli/src`` tree
had no idle bound, no drain, and no reaper, so its keepers outlived the
worker as live orphans against graph paths pytest had deleted. Both
conftests import from here so the trees cannot drift.
"""
import os

import pytest


@pytest.fixture(autouse=True, scope="session")
def _reap_store_keepers():
    """Every spawned keeper dies with the session: idle bound for keepers the
    reaper never hears about, ledger SIGTERM with a drain assert, sweep for
    the ledger-blind population by graph existence, never argv."""
    os.environ.setdefault("FNO_STORE_KEEPER_IDLE_SECS", "5")
    yield
    from fno.graph.store import reap_spawned_keepers, sweep_orphaned_keepers

    survivors = reap_spawned_keepers(timeout=15.0)
    sweep_orphaned_keepers(timeout=15.0)
    assert not survivors, (
        f"{len(survivors)} store keeper(s) outlived the test session "
        f"(pids {sorted(survivors)[:10]}); the spawn ledger must drain to zero"
    )


@pytest.fixture(autouse=True)
def _drain_exited_keepers():
    """poll() exited keepers around every test: an exited child is a zombie
    until collected, and the session reaper runs once."""
    from fno.graph.store import drain_exited_keepers

    drain_exited_keepers()
    yield
    drain_exited_keepers()
