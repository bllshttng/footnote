"""The refusal deadline and the silent failover (x-763a, changes 11 and 12).

Run: cd cli && uv run pytest tests/test_quota_refusal_and_failover.py -v

Both fixtures are the machine's own recorded events, not synthesized payloads:
27 `worker_refused` records that every one carried `resets_at: null`, and 38
`worker_silent` records carrying a real 429. Handles and vendor request ids
are stripped; the message shapes are untouched.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.adapters.providers.error_taxonomy import (
    ErrorClass,
    normalize,
    reset_window_seconds_from,
)

FIXTURES = Path(__file__).resolve().parent / "fixtures"


def _records(name: str) -> list[dict]:
    lines = (FIXTURES / name).read_text(encoding="utf-8").splitlines()
    return [json.loads(line) for line in lines if line.strip()]


@pytest.fixture
def refused():
    return _records("worker_refused_quota_20260827.jsonl")


@pytest.fixture
def silent():
    return _records("quota_429_worker_silent.jsonl")


class TestRecordedCorpusStillShowsTheDefect:
    """The fixtures are only evidence if they still carry what was measured."""

    def test_every_recorded_refusal_carried_a_null_deadline(self, refused) -> None:
        assert refused, "the recorded refusal corpus is empty"
        assert all(r["data"]["resets_at"] is None for r in refused), (
            "the fixture no longer reproduces the defect it was captured for"
        )

    def test_most_of_them_carried_a_stamp_that_was_seen_and_refused(
        self, refused
    ) -> None:
        """A populated reset_stamp_unparsed beside a null resets_at proves the
        producer FOUND the reset and could not use it."""
        seen = [r for r in refused if r["data"].get("reset_stamp_unparsed")]
        assert len(seen) >= 20, f"only {len(seen)} of {len(refused)} carried a stamp"


class TestTheWindowNameIsTheTrustworthyHalf:
    """AC18-HP / AC19-ERR / AC20-ERR."""

    def test_a_named_window_yields_a_real_deadline(self, silent) -> None:
        """AC18-HP. Replayed over the real 429 corpus: every message naming a
        window produces an epoch, and it is the observation time plus that
        window."""
        assert silent, "the recorded 429 corpus is empty"
        checked = 0
        for record in silent:
            body = record["data"]["last_message"]
            if "for 5 hour" not in body:
                continue
            observed = 1_787_000_000.0
            err = normalize(429, None, body, now=observed)
            assert err.error_class is ErrorClass.PROVIDER_4XX_QUOTA
            assert err.resets_at == observed + 5 * 3600.0
            assert err.reset_is_derived is True
            checked += 1
        assert checked >= 20, f"only {checked} records named a five-hour window"

    def test_the_offset_free_stamp_is_never_the_epoch(self, silent) -> None:
        """AC19-ERR. Three offsets land inside the named window, so a test that
        asserts a parsed absolute value must fail whichever one it picks.

        Measured from the 10:00:26Z observation with a five-hour window: UTC+7,
        UTC+8 and UTC+9 sit at +4.16h, +3.16h and +2.16h. The window constraint
        disambiguates nothing.
        """
        from datetime import datetime, timedelta, timezone

        for record in silent:
            body = record["data"]["last_message"]
            stamp = body.split("will reset at ")[-1].rstrip("]")
            try:
                naive = datetime.fromisoformat(stamp)
            except ValueError:
                continue
            err = normalize(429, None, body, now=1_787_000_000.0)
            assert err.resets_at is not None
            for offset in range(-12, 15):
                aware = naive.replace(tzinfo=timezone(timedelta(hours=offset)))
                assert err.resets_at != aware.timestamp(), (
                    f"the epoch matches the stamp read at UTC{offset:+d}, "
                    "which is a timezone guess"
                )

    def test_an_unnamed_window_leaves_the_deadline_absent(self) -> None:
        """AC20-ERR. An invented deadline is worse than an absent one."""
        err = normalize(429, None, "429 Too Many Requests. Slow down.", now=1000.0)
        assert err.resets_at is None
        assert err.reset_is_derived is False

    def test_a_resolved_stamp_still_wins_over_the_derived_bound(self) -> None:
        """AC13-EDGE in miniature: an unambiguous epoch is not a bound, so it
        is never replaced by one."""
        body = "Usage limit reached for 5 hour. Resets at 2026-08-27T21:09:58+08:00"
        err = normalize(429, None, body, now=1_787_000_000.0)
        assert err.reset_is_derived is False
        assert err.resets_at != 1_787_000_000.0 + 5 * 3600.0

    @pytest.mark.parametrize(
        ("body", "seconds"),
        [
            ("limit reached for 5 hour", 5 * 3600.0),
            ("limit reached for 1 week", 604800.0),
            ("rate limit reached for hour", 3600.0),
            ("limit reached for 30 minutes", 1800.0),
            ("limit reached for 2 days", 172800.0),
            ("nothing named here", None),
            ("limit reached for 0 hours", None),
            ("limit reached for 99 weeks", None),  # past the sane horizon
        ],
    )
    def test_window_name_vocabulary(self, body, seconds) -> None:
        assert reset_window_seconds_from(body) == seconds


class TestFailoverNamesItsGiveUp:
    """AC21-ERR. Twenty recorded rotations, twenty abandonments, nineteen mute."""

    def test_an_abandonment_without_a_reason_is_refused_at_the_writer(self) -> None:
        from fno.events import ValidationError, _build

        with pytest.raises(ValidationError):
            _build(
                "failover_swapped",
                "daemon",
                {"short_id": "79ebf387", "redispatched": False},
            )

    def test_a_named_abandonment_is_accepted(self) -> None:
        from fno.events import _build

        event = _build(
            "failover_swapped",
            "daemon",
            {
                "short_id": "79ebf387",
                "redispatched": False,
                "reason": "no-session-uuid",
            },
        )
        assert event["data"]["reason"] == "no-session-uuid"

    def test_a_successful_redispatch_needs_no_reason(self) -> None:
        from fno.events import _build

        event = _build(
            "failover_swapped",
            "daemon",
            {"short_id": "79ebf387", "redispatched": True},
        )
        assert event["data"]["redispatched"] is True

    def test_the_outcome_carries_its_reason_and_still_compares_as_a_string(
        self,
    ) -> None:
        """The str subclass exists so no existing comparison had to change."""
        from fno import recovery

        outcome = recovery._gave_up("no-session-uuid")
        assert outcome == "rotated-no-worker"
        assert outcome in ("swapped", "rotated-no-worker", "notified")
        assert outcome.reason == "no-session-uuid"

    def test_a_failed_redispatch_is_falsy_and_named(self) -> None:
        """_redispatch callers test `is True` or truthiness, so the sentinel
        must be falsy, must not be True, and must still say why."""
        from fno import recovery

        failed = recovery._Failed("claim-held")
        assert not failed
        assert failed is not True
        assert failed == False  # noqa: E712 - the point is the equality
        assert failed.reason == "claim-held"


class TestSessionUuidFallsBackToTheRegistryRow:
    """AC22-HP. The supervisor's file is gone by the time recovery runs; the
    row that outlived it still carries the UUID."""

    def test_the_candidate_row_answers_when_the_session_file_is_gone(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno import recovery

        monkeypatch.setattr(
            "fno.agents.harnesses._claude_session_registry.resolve_session_uuid",
            lambda _s: None,
        )

        class _Row:
            short_id = "79ebf387"
            harness_session_id = "79ebf387-4c0b-43bd-b9e3-ccc4e74ed5ce"

        monkeypatch.setattr(
            "fno.agents.registry.load_registry", lambda *a, **k: [_Row()]
        )
        got = recovery._resolve_session_uuid("79ebf387")
        assert got == "79ebf387-4c0b-43bd-b9e3-ccc4e74ed5ce"

    def test_a_live_supervisor_still_wins(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """The registry value RECORDS what the resume key was; a live
        supervisor's sessionId IS it."""
        from fno import recovery

        monkeypatch.setattr(
            "fno.agents.harnesses._claude_session_registry.resolve_session_uuid",
            lambda _s: "79ebf387-live-from-the-supervisor-file",
        )

        class _Row:
            short_id = "79ebf387"
            harness_session_id = "79ebf387-4c0b-43bd-b9e3-ccc4e74ed5ce"

        monkeypatch.setattr(
            "fno.agents.registry.load_registry", lambda *a, **k: [_Row()]
        )
        assert recovery._resolve_session_uuid("79ebf387") == (
            "79ebf387-live-from-the-supervisor-file"
        )

    @pytest.mark.parametrize(
        ("value", "accepted"),
        [
            # The row's own short id, not the resume key: too short.
            ("79ebf387", False),
            ("", False),
            (None, False),
            (12345, False),
            # A real UUID, but another worker's: a resume onto it would drive
            # the wrong session.
            ("dbd7f862-9cad-4de6-b0ec-952d577e68d5", False),
            ("79ebf387-4c0b-43bd-b9e3-ccc4e74ed5ce", True),
        ],
    )
    def test_only_a_full_uuid_that_matches_the_short_id_is_accepted(
        self, monkeypatch: pytest.MonkeyPatch, value, accepted
    ) -> None:
        """A truncated or unrelated field must never reach --resume.

        Exercised against ``_session_uuid_from_registry`` itself, over a
        registry holding exactly the row under test. Going through
        ``_resolve_session_uuid`` with this function stubbed out would test the
        stub rather than the guard.
        """
        from fno import recovery

        class _Row:
            short_id = "79ebf387"
            harness_session_id = value

        monkeypatch.setattr(
            "fno.agents.registry.load_registry", lambda *a, **k: [_Row()]
        )
        got = recovery._session_uuid_from_registry("79ebf387")
        assert got == (value if accepted else None)
