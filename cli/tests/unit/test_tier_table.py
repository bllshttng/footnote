"""The tier table: every id reachable, staleness detectable, omissions honest.

This table drifted a full model generation (gpt-5.5 / gpt-5.4 /
claude-opus-4-8) with nothing detecting it. The tests here are the tripwire:
AC5-HP walks the shipped tables asserting the unmapped-id list renders EMPTY
(listed, never an exit code trusted alone), and AC5-MARKER plants a dead id in
a copy and asserts the checker flags it BY NAME. AC5-ERR pins the recorded
omission: the two operator-named ids no configured provider serves are absent
from the table and named in the module that owns it.
"""
from __future__ import annotations

from pathlib import Path

import fno.adapters.providers.benchmarks as bm
from fno.route_resolve import _BAND_FLOOR, _GRID_CANDIDATES, _STATIC_FALLTHROUGH

REPO_ROOT = Path(__file__).resolve().parents[3]


def _all_tier_tables() -> dict:
    merged: dict = {k: list(v) for k, v in bm.STATIC_TIERS.items()}
    for band, names in _GRID_CANDIDATES.items():
        merged.setdefault(band, []).extend(names)
    return merged


def test_every_tier_id_is_reachable_by_name():
    """AC5-HP: the unmapped-id list renders empty; the list is the assertion."""
    dead = bm.unreachable_tier_ids(_all_tier_tables())
    assert dead == [], f"tier ids with no reachability row: {dead}"


def test_a_planted_dead_id_is_flagged_by_name():
    """AC5-MARKER: plant, observe the flag, unplant, observe green."""
    planted = {"high": ["claude-opus-5", "gpt-dead-9.9"]}
    flagged = bm.unreachable_tier_ids(planted)
    assert flagged == ["gpt-dead-9.9"]
    # Unplant: the shipped table is green.
    assert bm.unreachable_tier_ids(_all_tier_tables()) == []


def test_every_band_serves_every_harness_with_a_row():
    """No band resolves to nothing on a harness the table serves at all."""
    assert bm.empty_bands_for_harness() == {}


def test_a_band_a_harness_cannot_serve_is_reported():
    planted = {"max": ["gpt-5.6-sol"], "low": ["claude-haiku-4-5"]}
    holes = bm.empty_bands_for_harness(("claude", "codex"), tiers=planted)
    assert holes == {"claude": ["max"], "codex": ["low"]}


def test_the_max_band_exists_and_sits_above_high():
    assert "max" in bm.STATIC_TIERS and bm.STATIC_TIERS["max"]
    assert "max" in _BAND_FLOOR
    assert _BAND_FLOOR["max"] > _BAND_FLOOR["high"]
    assert _STATIC_FALLTHROUGH["max"][0] == "max"


def test_max_and_high_share_the_codex_model_and_differ_by_design():
    """The codex column of the operator table: sol serves both bands, so the
    LEVEL separation on that provider is the effort axis. The claude column
    keeps distinct models (fable vs opus), which is what makes a degraded max
    a real, detectable state rather than the universal case."""
    assert "gpt-5.6-sol" in bm.STATIC_TIERS["max"]
    assert "gpt-5.6-sol" in bm.STATIC_TIERS["high"]
    assert "claude-fable-5" in bm.STATIC_TIERS["max"]
    assert "claude-fable-5" not in bm.STATIC_TIERS["high"]


def test_the_generation_stale_generation_is_gone():
    for stale in ("gpt-5.5", "gpt-5.4", "claude-opus-4-8"):
        assert stale not in bm.REACHABILITY, stale
        for names in bm.STATIC_TIERS.values():
            assert stale not in names, stale


def test_the_unverifiable_ids_are_omitted_and_the_omission_recorded():
    """AC5-ERR: `gemini-3.7-flash` and opencode's 0x Alpha Free tier had no id
    any configured provider serves at verification time, so they are omitted -
    never guessed - and the omission is recorded in the module that owns the
    table. A marketing name shipped as a model id is the guess this refuses."""
    src = (REPO_ROOT / "cli/src/fno/adapters/providers/benchmarks.py").read_text(
        encoding="utf-8"
    )
    assert "gemini-3.7-flash" not in bm.REACHABILITY
    assert "0x" not in {n for names in bm.STATIC_TIERS.values() for n in names}
    assert "gemini-3.7-flash" in src  # named in the omission record
    assert "0x Alpha Free" in src  # named in the omission record


def test_the_verified_ids_carry_their_exact_provider_spellings():
    """Every shipped id was verified against a configured provider surface:
    the codex ids against the codex model surface, the GLM spellings against
    the z.ai lane's 1M-context suffix form."""
    expected = {
        "claude-fable-5": ("claude", "claude-fable-5"),
        "claude-opus-5": ("claude", "claude-opus-5"),
        "claude-sonnet-5": ("claude", "claude-sonnet-5"),
        "claude-haiku-4-5": ("claude", "claude-haiku-4-5"),
        "glm-5.3[1m]": ("claude", "glm-5.3[1m]"),
        "glm-4.7": ("claude", "glm-4.7"),
        "gpt-5.6-sol": ("codex", "gpt-5.6-sol"),
        "gpt-5.6-terra": ("codex", "gpt-5.6-terra"),
        "gpt-5.6-luna": ("codex", "gpt-5.6-luna"),
    }
    for name, row in expected.items():
        assert bm.REACHABILITY.get(name) == row, name
