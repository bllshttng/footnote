"""Keeper hygiene fixtures shared by BOTH pytest roots.

``cli/tests/`` and ``cli/src/fno/`` are separate pytest roots, and a fixture
that lives in one conftest protects nothing in the other. Bare ``pytest``
from ``cli/`` collects both trees in one invocation (no ``testpaths``), so
keeper hygiene that exists in one tree only leaks in the other - the trap
behind the leak of 2026-09-04, where the ``cli/src`` tree's keepers had no idle bound, no
per-test drain, and no session reaper and outlived their worker as live
orphans against graph paths pytest had deleted. Both conftests import the
fixtures defined here, so the trees cannot drift on this again.
"""
from __future__ import annotations

import os

import pytest


@pytest.fixture(autouse=True, scope="session")
def _reap_store_keepers():
    """Every spawned graph-store keeper dies with the test session.

    The store client spawns a detached ``fno-agents-worker --store-keeper``
    per fixture graph on demand, and the keeper is immortal by design. A
    session that touches many graphs therefore leaks one live worker per
    graph unless the spawner reaps them (measured 2026-09-03: 6,855 live
    keepers after one pytest pass, load 117, every fno call paying 4x
    startup). Three layers here:

    - ``FNO_STORE_KEEPER_IDLE_SECS`` bounds every keeper this session spawns
      to a short self-exit, so even a keeper the reaper never hears about
      cannot outlive the run by long.
    - The teardown SIGTERMs every keeper the client recorded and ASSERTS the
      alive count returns to zero. The assert is the point: a teardown that
      merely runs is decoration, and the positive signal is the count, not
      the pass.
    - ``sweep_orphaned_keepers()`` terminates every live keeper whose graph
      path no longer exists - the ledger-blind population (keepers spawned
      by CLI subprocesses never enter the ledger, and neither did the
      second tree's keepers before this module existed). The sweep's kill
      decision is the graph path's existence, never the command line: the
      canonical keepers and the leaked ones share a command line.

    Two measurement traps this assertion survived, recorded so the next
    counter does not re-learn them: a sandboxed shell sees a process jail,
    so its ``ps`` never lists the real pids (count under the same sandbox
    as the spawner, or the count is fiction); and macOS ``ps`` rejects the
    space form ``ps -o pid= args=`` with a silent empty output, so a
    zero-hit probe can be an arg-parse exit, not an absence (use the comma
    form ``-o pid=,args=`` and prove any filter with one live pid).
    """
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
    """Reap exited store keepers between tests, not only at session end.

    The session reaper above runs ONCE, at teardown, and an exited child stays
    in the process table as a zombie until someone collects its status - so
    every keeper that self-exits mid-run holds a table slot under its xdist
    worker pid until the whole session ends. Measured 2026-09-03: ~52 zombie
    keepers per minute under four workers, 549 zombies at 31% of the process
    table, with two suites running. Draining around every test bounds the
    corpse window to one test; the session reaper above stays as the SIGTERM
    backstop for keepers still LIVE at teardown, and its assert stays.
    """
    from fno.graph.store import drain_exited_keepers

    drain_exited_keepers()
    yield
    drain_exited_keepers()
