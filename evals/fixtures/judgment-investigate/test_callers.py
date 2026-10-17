"""Run with: python3 test_callers.py"""
import cache
import heal
import reconcile
import status

cache.write("pr:1", "merged", "e1")
cache.write("pr:2", "conflicting", "e2")
assert status.pr_state(1, "e1") == "merged"
assert status.pr_state(1, "old") is None
assert heal.needs_heal(2, "e2") and not heal.needs_heal(2, "old")
assert reconcile.merged([1], "e1") == [1] and reconcile.merged([1], "old") == []
print("ok")
