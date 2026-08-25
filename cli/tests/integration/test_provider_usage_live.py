"""Opt-in live positive-marker gate for every metered provider account."""
from __future__ import annotations

import os
import time
from pathlib import Path

import pytest


def test_live_provider_usage_has_numeric_usage_and_future_resets(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC4/AC6/AC7: live refreshes must produce real reset-bearing windows."""
    if os.environ.get("FNO_LIVE_PROVIDER_USAGE") != "1":
        pytest.skip("set FNO_LIVE_PROVIDER_USAGE=1 to run live provider probes")

    user = os.environ.get("USER", "")
    real_home = next(
        (
            candidate
            for candidate in (
                Path("/Users") / user,
                Path("/home") / user,
                Path("/root"),
            )
            if candidate.is_dir()
        ),
        None,
    )
    assert real_home is not None, "live provider gate could not locate the real HOME"
    monkeypatch.setenv("HOME", str(real_home))
    monkeypatch.setenv("USERPROFILE", str(real_home))

    from fno.adapters.providers.loader import load_providers
    from fno.adapters.providers.runtime_state import refresh_usage_detailed
    import fno.adapters.providers.usage as usage_mod

    # Avoid the repository's project-local accounts block shadowing the
    # operator's global machine records used by this opt-in test.
    global_only_root = Path("/tmp")
    records = load_providers(repo_root=global_only_root).by_id
    required = ("zai", "readyrule", "makers", "codex-main")
    missing = [account_id for account_id in required if account_id not in records]
    assert not missing, f"live account records missing: {missing}"

    now = time.time()
    observations = {}
    for account_id in required:
        observation = refresh_usage_detailed(
            account_id,
            ttl_seconds=0,
            now=now,
            repo_root=global_only_root,
        )
        assert observation.known, (
            f"{account_id}: live probe did not produce a positive marker "
            f"({observation.reason})"
        )
        assert observation.snapshot is not None
        assert observation.snapshot.windows
        observations[account_id] = observation.snapshot.windows
        for window in observation.snapshot.windows:
            assert isinstance(window.used_pct, (int, float))
            assert not isinstance(window.used_pct, bool)
            assert 0 <= window.used_pct <= 100
            assert isinstance(window.resets_at, (int, float))
            assert not isinstance(window.resets_at, bool)
            assert window.resets_at > now

    assert any(window.label == "5h" for window in observations["zai"])

    # A failed advisory probe is still an explicit unknown and never a
    # successful empty result.
    monkeypatch.setitem(
        usage_mod._PROBES,
        "zai",
        lambda _record, _now: (None, "probe-failed"),
    )
    failed = refresh_usage_detailed(
        "zai",
        ttl_seconds=0,
        now=time.time(),
        repo_root=global_only_root,
    )
    assert failed.snapshot is None
    assert failed.reason == "probe-failed"
