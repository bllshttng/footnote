"""Integration: dispatch:<id> roots globally so cross-repo dispatchers dedup.

Regression for x-135d. The boot-window ``dispatch:<id>`` reservation used to
root PER-REPO in the Python dispatch surfaces (advance / reconcile_dispatch /
spawn-guard) while ``node:<id>`` rooted globally. Two repos dispatching the same
node with no ``FNO_CLAIMS_ROOT`` set reserved ``dispatch:C`` under different
roots, missed each other, and both spawned a boot-window worker.

After the consolidation, all four surfaces delegate to
:func:`fno.claims.io.claims_root_for`, which roots ``node:``/``dispatch:``/
``reconcile:`` at the global root regardless of env. These tests drive the real
advance + reconcile_dispatch liveness probes against a reservation written by a
"different repo", with ``FNO_CLAIMS_ROOT`` UNSET (the exact condition that bit).
"""
from __future__ import annotations

import os

import pytest

from fno.backlog import advance, reconcile_dispatch
from fno.claims.core import acquire_claim
from fno.claims.io import claims_root_for


@pytest.fixture
def shared_home(tmp_path, monkeypatch):
    """Simulate two repos sharing one $HOME with no FNO_CLAIMS_ROOT override.

    This is the bug's trigger condition: without the env var, the OLD code fell
    through to each repo's per-canonical-root claims dir. The fix routes
    dispatch: to global_claims_root() == $HOME instead.
    """
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    return home


def test_dispatch_routes_global_not_per_repo(shared_home):
    """AC2-HP precondition: dispatch:<id> resolves to the global ($HOME) root."""
    root = claims_root_for("dispatch:x-cccc")
    assert root == shared_home
    # Same id, same directory as the node: claim it bridges to.
    assert claims_root_for("node:x-cccc") == root


def test_cross_repo_dispatch_dedup(shared_home):
    """AC2-HP: a reservation by repo 'web' is seen live by repo 'etl'.

    'web' and 'etl' have distinct canonical roots, but with the fix both resolve
    dispatch:<id> to the shared global root, so etl's advance + reconcile_dispatch
    probes observe web's live reservation and do not double-spawn.
    """
    res_root = claims_root_for("dispatch:x-cccc")
    acquire_claim(
        "dispatch:x-cccc",
        "web-dispatcher:1",
        ttl_ms=180_000,
        pid=os.getpid(),
        root=res_root,
    )

    # etl evaluates node C with no FNO_CLAIMS_ROOT in its env.
    assert advance._claim_is_live("dispatch:x-cccc") is True
    assert reconcile_dispatch._claim_is_live("dispatch:x-cccc") is True


def test_recovery_via_redispatch_after_release(shared_home):
    """AC1-EDGE: once the reservation is gone, the probe reads free again.

    A dispatcher must be able to re-reserve after a prior reservation is
    released/expired (recovery-via-redispatch preserved by the consolidation).
    """
    from fno.claims.core import release_claim

    res_root = claims_root_for("dispatch:x-dddd")
    acquire_claim(
        "dispatch:x-dddd", "web-dispatcher:1", ttl_ms=180_000, pid=os.getpid(), root=res_root
    )
    assert advance._claim_is_live("dispatch:x-dddd") is True

    release_claim(key="dispatch:x-dddd", holder="web-dispatcher:1", root=res_root)
    assert advance._claim_is_live("dispatch:x-dddd") is False
