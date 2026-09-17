"""`resolve_truth_status` binds the claims root late.

The resolver itself is tested against real claim + events files in
tests/test_truth_status.py.
"""

from __future__ import annotations


def test_truth_status_root_binding_is_late(tmp_path, monkeypatch):
    """`resolve_truth_status` must resolve the claims root at CALL time.

    A module-level `from fno.claims.io import claims_root_for` binds whatever
    that name points at during this module's first import, permanently, because
    sys.modules caches the module afterward. Several tests redirect
    `fno.claims.io.claims_root_for` at their own tmp dir, so any one of them
    that happened to trigger the first import left the stale lambda bound for
    every later caller in the process, and monkeypatch's teardown could not see
    it to restore.

    Measured before the fix: `mail drain-self` read a live node claim as `free`
    because this function was resolving the claims root of an unrelated test's
    tmp dir, which made a CI job red on a test the change under review never
    touched.

    Asserting the patch REACHES the function is the positive marker. An
    assertion that some claim reads correctly would pass either way whenever the
    two roots happen to coincide.
    """
    import fno.agents.truth_status as truth_status

    seen: list[str] = []

    def _fake_root(key: str):
        seen.append(key)
        return tmp_path

    monkeypatch.setattr("fno.claims.io.claims_root_for", _fake_root)
    truth_status.resolve_truth_status("some-node")

    assert seen == ["node:some-node"], (
        "the patched resolver was never consulted, so the binding is early again"
    )
