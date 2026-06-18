#!/usr/bin/env python3
"""Unit tests for version-aware opus pricing in fno.cost.cost_tracker (the
former scripts/lib/cost_tracker.py).

Covers AC2-HP / AC2-ERR / AC2-EDGE / AC2-FR from the cost-accuracy plan
(internal/fno/plans/2026-06-04-cost-accuracy-dedup-pricing.md) plus the
Boundaries failure modes (suffixed IDs, versions beyond the table) and the
never-silently-reprice-history invariant.

Run: python3 tests/lib/test_cost_tracker_pricing.py
 OR: cd cli && uv run pytest ../tests/lib/test_cost_tracker_pricing.py -q
"""

import contextlib
import io
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
# cost_tracker.py moved into the fno package as fno.cost.cost_tracker.
sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))

from fno.cost import cost_tracker  # noqa: E402
from fno.cost.cost_tracker import (  # noqa: E402
    LATEST_MODERN_OPUS_TIER,
    PRICING,
    calculate_cost,
    model_tier,
)

# The estimate CLI is now run via `python3 -m fno.cost.cost_tracker`.
COST_TRACKER_CMD = [sys.executable, "-m", "fno.cost.cost_tracker"]

MODERN_PRICES = {"input": 5.00, "output": 25.00, "cache_read": 0.50, "cache_create": 6.25}
LEGACY_PRICES = {"input": 15.00, "output": 75.00, "cache_read": 1.50, "cache_create": 18.75}


def _reset_fallback_state():
    """Tests must not depend on warning state left by earlier tests."""
    cost_tracker.FALLBACK_MODELS_SEEN.clear()


# --- AC2-HP: opus-4-8 priced correctly --------------------------------------


def test_opus_48_tier_exists_with_modern_prices():
    _reset_fallback_state()
    assert "opus-4.8" in PRICING
    for key, expected in MODERN_PRICES.items():
        assert PRICING["opus-4.8"][key] == expected, (
            f"opus-4.8 {key} should be {expected}, got {PRICING['opus-4.8'][key]}"
        )


def test_opus_48_model_ids_resolve_to_48_tier():
    _reset_fallback_state()
    for model_id in ("claude-opus-4-8", "claude-opus-4.8", "CLAUDE-OPUS-4-8"):
        assert model_tier(model_id) == "opus-4.8", model_id


def test_opus_48_context_suffix_parses_version_not_suffix():
    # `[1m]` context-size suffix is live today; the version regex must take
    # the FIRST digit pair after "opus" (4, 8), never (1, m).
    _reset_fallback_state()
    assert model_tier("claude-opus-4-8[1m]") == "opus-4.8"


def test_calculate_cost_uses_modern_rates_for_opus_48():
    _reset_fallback_state()
    usage = {
        "input_tokens": 1_000_000,
        "output_tokens": 1_000_000,
        "cache_read_input_tokens": 1_000_000,
        "cache_creation_input_tokens": 1_000_000,
    }
    cost = calculate_cost(usage, "claude-opus-4-8")
    expected = 5.00 + 25.00 + 0.50 + 6.25
    assert abs(cost - expected) < 1e-9, f"expected {expected}, got {cost}"


# --- AC2-EDGE: legacy versions unaffected ------------------------------------


def test_known_modern_versions_keep_exact_tiers():
    _reset_fallback_state()
    assert model_tier("claude-opus-4-7") == "opus-4.7"
    assert model_tier("claude-opus-4-6") == "opus-4.6"
    assert model_tier("claude-opus-4-5") == "opus-4.5"


def test_opus_46_fast_speed_still_routes_to_fast_tier():
    _reset_fallback_state()
    assert model_tier("claude-opus-4-6", speed="fast") == "opus-4.6-fast"
    # fast on a non-4.6 opus has no dedicated tier; normal tier applies
    assert model_tier("claude-opus-4-8", speed="fast") == "opus-4.8"


def test_legacy_opus_versions_stay_at_15_75():
    _reset_fallback_state()
    assert model_tier("claude-opus-4-1") == "opus-4.1"
    assert model_tier("claude-opus-4-1-20250805") == "opus-4.1"
    for tier in ("opus-4.0", "opus-4.1"):
        for key, expected in LEGACY_PRICES.items():
            assert PRICING[tier][key] == expected


def test_opus_40_dated_id_is_not_repriced_to_modern():
    # claude-opus-4-20250514 was Opus 4.0's real model ID. The bare regex
    # would read (4, 20250514) >= (4, 5) and reprice history to modern;
    # the date-artifact guard must keep it on the legacy tier.
    _reset_fallback_state()
    assert model_tier("claude-opus-4-20250514") == "opus-4.0"


def test_claude_3_opus_routes_to_legacy_tier():
    # Contiguous date digits have no separator, so the pair regex fails by
    # construction; the claude-3 guard then routes to legacy ($15/$75).
    _reset_fallback_state()
    assert model_tier("claude-3-opus-20240229") == "opus-4.0"


# --- AC2-FR: hypothetical future releases ------------------------------------


def test_future_opus_versions_default_to_latest_modern_tier():
    _reset_fallback_state()
    assert model_tier("claude-opus-4-9") == LATEST_MODERN_OPUS_TIER
    assert model_tier("claude-opus-5-0") == LATEST_MODERN_OPUS_TIER


# --- AC2-ERR: unparseable opus version ----------------------------------------


def test_unparseable_opus_warns_once_and_uses_modern_tier():
    _reset_fallback_state()
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr):
        tier_first = model_tier("claude-opus-next")
        tier_second = model_tier("claude-opus-next")
    assert tier_first == LATEST_MODERN_OPUS_TIER
    assert tier_second == LATEST_MODERN_OPUS_TIER
    warnings = [line for line in stderr.getvalue().splitlines() if "claude-opus-next" in line]
    assert len(warnings) == 1, f"expected exactly one warning, got: {warnings}"
    assert "claude-opus-next" in cost_tracker.FALLBACK_MODELS_SEEN


def test_parseable_models_do_not_warn():
    _reset_fallback_state()
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr):
        model_tier("claude-opus-4-8")
        model_tier("claude-opus-4-1")
        model_tier("claude-3-opus-20240229")
        model_tier("claude-sonnet-4-5")
    assert stderr.getvalue() == ""
    assert not cost_tracker.FALLBACK_MODELS_SEEN


# --- Non-opus families unchanged ----------------------------------------------


def test_non_opus_families_unchanged():
    _reset_fallback_state()
    assert model_tier("claude-sonnet-4-5-20250929") == "sonnet"
    assert model_tier("claude-haiku-4-5-20251001") == "haiku-4.5"
    assert model_tier("claude-3-5-haiku-20241022") == "haiku-3.5"
    assert model_tier("some-unknown-model") == "sonnet"


# --- estimate CLI (delegation target for cost-tracker.sh) ---------------------


def _cli_env() -> dict:
    """Child env with cli/src on PYTHONPATH so `-m fno.cost.cost_tracker`
    resolves even when run outside an editable install."""
    import os

    env = os.environ.copy()
    src = str(REPO_ROOT / "cli" / "src")
    env["PYTHONPATH"] = src + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    return env


def _run_estimate(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [*COST_TRACKER_CMD, "estimate", *args],
        capture_output=True,
        text=True,
        env=_cli_env(),
    )


def test_estimate_cli_basic_tokens():
    result = _run_estimate("claude-opus-4-8", "1000000", "1000000")
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "30.0000"  # $5 input + $25 output


def test_estimate_cli_with_cache_args():
    result = _run_estimate("claude-opus-4-8", "0", "0", "1000000", "1000000")
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "6.7500"  # $0.50 cache_read + $6.25 cache_create


def test_estimate_cli_shell_model_aliases():
    # cost-tracker.sh callers pass bare family names (opus/sonnet/haiku);
    # those must keep resolving rather than erroring.
    result = _run_estimate("sonnet", "1000000", "1000000")
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "18.0000"  # $3 + $15


def test_estimate_cli_rejects_non_numeric_tokens():
    result = _run_estimate("claude-opus-4-8", "abc", "0")
    assert result.returncode != 0
    assert result.stdout.strip() == ""


def test_estimate_cli_requires_args():
    result = subprocess.run(
        [*COST_TRACKER_CMD, "estimate"],
        capture_output=True,
        text=True,
        env=_cli_env(),
    )
    assert result.returncode != 0


def _main() -> int:
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"  PASS {name}")
            except AssertionError as exc:
                failures += 1
                print(f"  FAIL {name}: {exc}")
    print(f"{'OK' if failures == 0 else 'FAILED'} ({failures} failures)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(_main())
