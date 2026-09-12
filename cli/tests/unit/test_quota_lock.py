"""The one lock-decision site (x-bbc0 change 1).

``record_quota_lock`` attributes the cooldown to the account the caller
names and refuses to guess: None and "default" write nothing, and the
active account is never consulted. The window boundary comes from the
refusal body when it names one.

Run: cd cli && uv run pytest tests/unit/test_quota_lock.py -v
"""
from __future__ import annotations

import time
from datetime import datetime, timedelta, timezone

import pytest

from fno.adapters.providers.error_taxonomy import reset_epoch_from
from fno.adapters.providers.runtime_state import is_in_cooldown, read_state


@pytest.fixture
def state_path(tmp_path, monkeypatch: pytest.MonkeyPatch):
    """Pin the machine-wide runtime-state file inside the test's tmp dir."""
    p = tmp_path / "provider-runtime-state.json"
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(p))
    return p


def _quota_body_with_reset(hours_ahead: int = 3) -> str:
    """A quota refusal whose body names an offset-bearing reset stamp."""
    reset = datetime.now(timezone.utc) + timedelta(hours=hours_ahead)
    return (
        "API Error: 429 usage limit reached. Quota exceeded; "
        f"resets at {reset.strftime('%Y-%m-%dT%H:%M:%S+00:00')}."
    )


class TestRecordQuotaLock:
    def test_a_quota_body_locks_the_named_account_until_the_reset(
        self, state_path
    ) -> None:
        from fno.agents.quota_lock import record_quota_lock

        body = _quota_body_with_reset()
        wrote = record_quota_lock("readyrule", body)

        assert wrote == "readyrule"
        assert is_in_cooldown("readyrule")
        health = read_state().provider_health["readyrule"]
        assert health.rate_limited_until is not None
        # The lock holds to the harvested reset, not to a backoff step.
        assert health.rate_limited_until == pytest.approx(
            reset_epoch_from(body, None)
        )
        assert health.rate_limited_until > time.time()

    def test_an_explicit_resets_at_wins_over_the_body(self, state_path) -> None:
        from fno.agents.quota_lock import record_quota_lock

        explicit = time.time() + 90.0
        record_quota_lock("readyrule", _quota_body_with_reset(hours_ahead=5),
                          resets_at=explicit)

        health = read_state().provider_health["readyrule"]
        assert health.rate_limited_until == pytest.approx(explicit)

    def test_none_writes_nothing_and_never_resolves_the_active_account(
        self, state_path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.adapters.providers import loader
        from fno.agents.quota_lock import record_quota_lock

        def _boom(*a, **k):
            raise AssertionError("active account consulted for an unattributed refusal")

        monkeypatch.setattr(loader, "effective_active", _boom)

        assert record_quota_lock(None, _quota_body_with_reset()) is None
        assert read_state().provider_health == {}

    def test_default_writes_nothing(self, state_path) -> None:
        from fno.agents.quota_lock import record_quota_lock

        # "default" is the registry's value for "the spawn pinned nothing";
        # only a registered account id is an account.
        assert record_quota_lock("default", _quota_body_with_reset()) is None
        assert read_state().provider_health == {}

    def test_unclassifiable_text_writes_nothing(self, state_path) -> None:
        from fno.agents.quota_lock import record_quota_lock

        assert record_quota_lock("readyrule", "Error: file not found") is None
        assert read_state().provider_health == {}

    def test_empty_text_writes_nothing(self, state_path) -> None:
        from fno.agents.quota_lock import record_quota_lock

        assert record_quota_lock("readyrule", "") is None
        assert record_quota_lock("readyrule", None) is None
        assert read_state().provider_health == {}
