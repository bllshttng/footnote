"""The zai false green, the key-position label, and the confidence field (x-763a).

Run: cd cli && uv run pytest src/fno/adapters/providers/test_usage_false_green.py -v

Every payload here is a LIVE capture, not a synthesized shape. The sibling
``test_usage.py`` parametrizes single-row payloads only, and a single dropped
row leaves ``()``, which correctly reads unknown. A MIXED row list is the only
shape that produces the defect and it is the shape the live endpoint returns,
so every fixture in this module carries more than one row or reproduces a
measured production body.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.adapters.providers.runtime_state import HeadroomState, _headroom_from
from fno.adapters.providers.usage import (
    UsageSnapshot,
    UsageWindow,
    _label_for_minutes,
    _parse_codex_rate_limits,
    _parse_zai_windows,
)

FIXTURES = Path(__file__).resolve().parents[4] / "tests" / "fixtures"


def _fixture(name: str):
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


@pytest.fixture
def zai_live():
    """The two-row body the endpoint returned 2026-08-27, credentials stripped."""
    return _fixture("zai_quota_live_20260827.json")


@pytest.fixture
def codex_live():
    """The live codex rate_limits event: window_minutes 10080, secondary null."""
    return _fixture("codex_rate_limits_20260827.json")


def _snap(windows, **kw):
    return UsageSnapshot(
        provider_id="zai",
        windows=tuple(windows),
        probed_at=1000.0,
        source="quota-endpoint",
        **kw,
    )


class TestZaiFalseGreen:
    """AC1-HP / AC2-ERR: the binding window must survive, and green must not."""

    def test_live_payload_yields_both_windows_including_the_five_hour_cap(
        self, zai_live
    ) -> None:
        """AC1-HP. Today this returns ONE window (the 1m tool limit) and drops
        the 300-minute TOKENS_LIMIT for a missing nextResetTime."""
        windows = _parse_zai_windows(zai_live)
        assert len(windows) == 2, f"expected both rows retained, got {windows}"
        labels = {w.label for w in windows}
        assert "5h" in labels, f"the 300-minute TOKENS_LIMIT window is missing: {labels}"

    def test_the_retained_five_hour_window_has_no_invented_reset(
        self, zai_live
    ) -> None:
        """AC4-EDGE. The row carries no nextResetTime, so its reset is None -
        retained and binding, never fabricated."""
        five_h = [w for w in _parse_zai_windows(zai_live) if w.label == "5h"]
        assert five_h, "no 5h window parsed"
        assert five_h[0].resets_at is None

    def test_live_payload_does_not_fold_to_ok(self, zai_live) -> None:
        """AC2-ERR, the false-green criterion. Must FAIL against today's code,
        where one surviving 0.625% window with a future reset reads OK."""
        snap = _snap(_parse_zai_windows(zai_live))
        verdict = _headroom_from(snap, None, now=1787000000.0, threshold_pct=80.0)
        assert verdict.state is not HeadroomState.OK, (
            "a response whose binding window was never read must not read ok"
        )

    def test_a_rejected_row_marks_the_snapshot_partial(self) -> None:
        """AC2-ERR. A row of a KNOWN limit type that fails the usability guard
        poisons the response; it must not vanish from it."""
        payload = {
            "success": True,
            "code": 200,
            "data": {
                "limits": [
                    {
                        "type": "TIME_LIMIT",
                        "unit": 5,
                        "number": 1,
                        "percentage": 1,
                        "nextResetTime": 1788247801997,
                    },
                    # Unusable: no unit the parser recognizes, so it is rejected
                    # rather than retained with a None reset.
                    {"type": "TOKENS_LIMIT", "unit": 99, "number": 5, "percentage": 0},
                ]
            },
        }
        windows, partial = _parse_zai_windows(payload, with_partial=True)
        assert partial is True
        snap = _snap(windows, partial=True)
        verdict = _headroom_from(snap, None, now=1787000000.0, threshold_pct=80.0)
        assert verdict.state is not HeadroomState.OK

    def test_a_wholly_well_formed_response_is_not_partial_and_still_reads_ok(
        self,
    ) -> None:
        """AC3-EDGE. The floor must not make every reading pessimistic."""
        payload = {
            "success": True,
            "code": 200,
            "data": {
                "limits": [
                    {
                        "type": "TIME_LIMIT",
                        "unit": 5,
                        "number": 1,
                        "percentage": 1,
                        "nextResetTime": 1788247801997,
                    },
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "percentage": 2,
                        "nextResetTime": 1788247801997,
                    },
                ]
            },
        }
        windows, partial = _parse_zai_windows(payload, with_partial=True)
        assert len(windows) == 2
        assert partial is False
        verdict = _headroom_from(
            _snap(windows, partial=False), None, now=1787000000.0, threshold_pct=80.0
        )
        assert verdict.state is HeadroomState.OK

    def test_a_reset_less_window_at_full_reads_exhausted(self) -> None:
        """AC5-ERR. A missing reset must never downgrade a certain exhaustion
        to unknown: the window can never be 'already reset', so it always binds."""
        snap = _snap([UsageWindow(label="5h", used_pct=100.0, resets_at=None)])
        verdict = _headroom_from(snap, None, now=1787000000.0, threshold_pct=80.0)
        assert verdict.state is HeadroomState.EXHAUSTED


class TestIncoherentTimeLimitReset:
    """AC9-ERR: label and reset must describe the same span, or the reset is None."""

    def test_one_minute_window_does_not_publish_a_plan_period_reset(
        self, zai_live
    ) -> None:
        """Measured: label '1m', resets_at five days out, because the row's
        nextResetTime is the PLAN period and not that window's reset."""
        one_min = [w for w in _parse_zai_windows(zai_live) if w.label == "1m"]
        assert one_min, "no 1m window parsed"
        w = one_min[0]
        if w.resets_at is None:
            return  # dropped to None rather than published: the legal outcome
        # Otherwise the reset must lie within the window's own span (plus slack).
        assert w.resets_at - 1787818296.0 <= 60 * 2, (
            f"a 1m window resetting at {w.resets_at} describes a different span"
        )


class TestLabelComesFromTheField:
    """AC14-HP / AC15-EDGE: assert the span the payload states, never the span
    its key position implies."""

    def test_live_codex_payload_labels_weekly_not_five_hour(self, codex_live) -> None:
        """AC14-HP. primary.window_minutes is 10080 and secondary is null, so
        fno mislabels the ONLY window codex reports."""
        windows = _parse_codex_rate_limits(codex_live)
        assert len(windows) == 1
        assert windows[0].label == "weekly", (
            f"window_minutes 10080 must read weekly, got {windows[0].label!r}"
        )
        assert windows[0].used_pct == 71.0

    def test_a_payload_without_window_minutes_labels_unknown(self) -> None:
        """AC15-EDGE. Never inherit a key position's assumption."""
        payload = {
            "rate_limits": {
                "primary": {"used_percent": 12.0, "resets_at": 1788272008},
                "secondary": None,
            }
        }
        windows = _parse_codex_rate_limits(payload)
        assert len(windows) == 1
        assert windows[0].label == "unknown"

    @pytest.mark.parametrize(
        ("minutes", "label"),
        [
            (300, "5h"),
            (10080, "weekly"),
            (1440, "1d"),
            (2880, "2d"),
            (60, "1h"),
            (1, "1m"),
            (None, "unknown"),
        ],
    )
    def test_label_helper_is_one_shared_vocabulary(self, minutes, label) -> None:
        """One helper for both lanes, so 'weekly' cannot come to mean two spans."""
        assert _label_for_minutes(minutes) == label


class TestConfidenceTravelsInBand:
    """AC16-EDGE: a coarse reading must not be indistinguishable from a precise one."""

    def test_snapshot_carries_confidence(self) -> None:
        snap = _snap([UsageWindow("5h", 1.0, 9e18)], confidence="exact")
        assert snap.confidence == "exact"

    def test_confidence_defaults_to_unknown(self) -> None:
        """A legacy row already on disk reads unknown, never exact: an old row's
        precision is exactly what is not known."""
        assert _snap([UsageWindow("5h", 1.0, 9e18)]).confidence == "unknown"

    def test_zai_live_payload_reads_percent_only_where_only_percentage_is_present(
        self, zai_live
    ) -> None:
        """The TOKENS_LIMIT row carries percentage alone, so the reading for
        this response is not exact."""
        from fno.adapters.providers.usage import _zai_confidence

        assert _zai_confidence(zai_live) == "percent_only"
