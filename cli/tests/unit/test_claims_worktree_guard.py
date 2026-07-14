"""Unit tests for fno.claims.worktree_guard (x-193d Wave 5).

Maps to the plan's Verification points:
  1. claude holds W -> codex entering W is refused, message names the claude owner.
  2. Same-harness re-entry into W: no refusal.
  3. Concurrent double-entry race: exactly one winner.
  4. Dispatched worker: claim exists from boot; killing it releases per liveness.

Filesystem isolation via the ``root`` claims-dir argument (tmp_path), the same
pattern the other claim tests use.
"""
from __future__ import annotations

from pathlib import Path

from fno.claims.core import acquire_claim, claim_status
from fno.claims.worktree_guard import (
    VERDICT_ACQUIRED,
    VERDICT_FOREIGN,
    VERDICT_NO_WORKTREE,
    VERDICT_OK,
    VERDICT_OVERRIDE,
    guard_worktree,
    worktree_claim_key,
)

WT = Path("/work/repo/.claude/worktrees/x-abcd")


def _guard(root, harness, session, wt=WT, **kw):
    return guard_worktree(
        wt,
        my_harness=harness,
        my_holder=f"{harness}-worktree:{session}",
        root=root,
        **kw,
    )


class TestAcquire:
    def test_free_worktree_is_acquired_and_tagged(self, tmp_path):
        r = _guard(tmp_path, "claude", "s1")
        assert r.verdict == VERDICT_ACQUIRED
        st = claim_status(worktree_claim_key(WT), root=tmp_path)
        assert st["state"] == "live"
        assert st["harness"] == "claude"
        assert st["holder"] == "claude-worktree:s1"

    def test_no_worktree_enforces_nothing(self, tmp_path):
        r = guard_worktree(None, my_harness="claude", my_holder="h", root=tmp_path)
        assert r.verdict == VERDICT_NO_WORKTREE

    def test_no_harness_enforces_nothing(self, tmp_path):
        r = guard_worktree(WT, my_harness=None, my_holder="h", root=tmp_path)
        assert r.verdict == VERDICT_NO_WORKTREE
        assert not claim_status(worktree_claim_key(WT), root=tmp_path).get("holder")


class TestForeignRefusal:
    def test_codex_refused_from_claude_owned_worktree(self, tmp_path):
        """Verification #1."""
        _guard(tmp_path, "claude", "s1")  # claude owns W
        r = _guard(tmp_path, "codex", "s2")
        assert r.blocked
        assert r.verdict == VERDICT_FOREIGN
        assert r.owner_harness == "claude"
        assert r.owner_holder == "claude-worktree:s1"  # message can name the owner

    def test_override_downgrades_foreign(self, tmp_path):
        _guard(tmp_path, "claude", "s1")
        r = _guard(tmp_path, "codex", "s2", override=True)
        assert r.verdict == VERDICT_OVERRIDE
        assert not r.blocked
        assert r.owner_harness == "claude"


class TestSameHarnessReentry:
    def test_same_holder_reentry_ok(self, tmp_path):
        """Verification #2: my own session re-entering never refuses."""
        _guard(tmp_path, "claude", "s1")
        r = _guard(tmp_path, "claude", "s1")
        assert r.verdict == VERDICT_OK
        assert not r.blocked

    def test_sibling_same_harness_session_ok(self, tmp_path):
        """Two claude sessions in one worktree: same harness, no refusal."""
        _guard(tmp_path, "claude", "s1")
        r = _guard(tmp_path, "claude", "s2")
        assert r.verdict == VERDICT_OK
        assert not r.blocked
        # The original owner's claim is left intact (not stolen by the sibling).
        st = claim_status(worktree_claim_key(WT), root=tmp_path)
        assert st["holder"] == "claude-worktree:s1"


class TestReadOnly:
    def test_read_only_does_not_acquire(self, tmp_path):
        r = _guard(tmp_path, "claude", "s1", acquire=False)
        assert r.verdict == VERDICT_NO_WORKTREE
        assert not claim_status(worktree_claim_key(WT), root=tmp_path).get("holder")

    def test_read_only_reports_foreign_owner(self, tmp_path):
        _guard(tmp_path, "claude", "s1")  # claude owns W
        r = _guard(tmp_path, "codex", "s2", acquire=False)
        assert r.verdict == VERDICT_FOREIGN
        assert r.owner_harness == "claude"


class TestLostRace:
    def test_lost_race_winner_vanishes_retries_and_acquires(self, tmp_path, monkeypatch):
        """A lost acquire whose winner then vanishes must NOT false-positive as
        foreign: the retry loop re-acquires once the claim is free again."""
        import fno.claims.worktree_guard as wg

        calls = {"n": 0}
        real_acquire = wg._try_acquire

        def flaky(*a, **kw):
            calls["n"] += 1
            if calls["n"] == 1:
                return None  # simulate a lost race on the first attempt
            return real_acquire(*a, **kw)

        monkeypatch.setattr(wg, "_try_acquire", flaky)
        # No claim on disk -> the re-read after the None sees state=free, so the
        # loop retries instead of returning foreign.
        r = _guard(tmp_path, "claude", "s1")
        assert r.verdict == VERDICT_ACQUIRED
        assert calls["n"] == 2

    def test_lost_race_to_live_foreign_refuses(self, tmp_path, monkeypatch):
        """A lost race whose winner is a still-LIVE foreign owner is refused:
        the re-read inside the retry loop sees the live codex claim."""
        import fno.claims.worktree_guard as wg

        def lose_then_plant_foreign(*a, **kw):
            # Simulate the winning codex worker landing its claim between our
            # (failed) acquire and the re-read.
            acquire_claim(
                worktree_claim_key(WT), "codex-worktree:c1", ttl_ms=600_000,
                harness="codex", pid=1, root=tmp_path,
            )
            return None

        monkeypatch.setattr(wg, "_try_acquire", lose_then_plant_foreign)
        r = _guard(tmp_path, "claude", "s1")  # starts with no claim (state=free)
        assert r.verdict == VERDICT_FOREIGN
        assert r.owner_harness == "codex"


class TestStaleRecovery:
    def test_foreign_but_stale_claim_is_reclaimed(self, tmp_path):
        """A dead foreign owner (TTL expired) does not block - it is stale and
        reclaimable, mirroring node-claim liveness (Verification #4 release)."""
        # A codex claim that is already expired (ttl in the past via tiny ttl).
        acquire_claim(
            worktree_claim_key(WT), "codex-worktree:old", ttl_ms=60_000,
            harness="codex", pid=1, root=tmp_path,
        )
        # pid=1 is init (alive) which would keep it live; use a definitely-dead pid.
        # Re-acquire with a dead pid + already-past effective liveness:
        # simplest deterministic path is force the file expired.
        key = worktree_claim_key(WT)
        st = claim_status(key, root=tmp_path)
        # Rewrite expires_at into the past and pid to an unused one.
        from fno.claims.io import claim_path
        import yaml
        p = claim_path(key, root=tmp_path)
        doc = yaml.safe_load(p.read_text())
        doc["expires_at"] = 1  # epoch ms in 1970 -> expired
        doc["pid"] = 2_147_483_000  # implausible pid -> dead
        p.write_text(yaml.safe_dump(doc))
        r = _guard(tmp_path, "claude", "s1")
        assert r.verdict == VERDICT_ACQUIRED
        assert claim_status(key, root=tmp_path)["harness"] == "claude"
