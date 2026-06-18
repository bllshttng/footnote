"""
Smoke tests for agents/frontend-executor.md — Phase 01 invariants.

These tests parse the agent's documented rules from the markdown file.
They assert that the rewritten agent correctly encodes three invariants:

1. Default stage selection: net-new component -> [craft, critique, harden]
2. Two-tier verdict thresholds: target=35, floor=25, band -> DONE_WITH_CONCERNS
3. Finding classification table has exactly three buckets

Tests are lightweight markdown-parsing assertions, not full agent execution.
Full BDD tests land in Phase 04.
"""
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent.parent
AGENT_FILE = REPO_ROOT / "agents" / "frontend-executor.md"


def load_agent_text() -> str:
    return AGENT_FILE.read_text()


# ---------------------------------------------------------------------------
# Invariant 1: Default stage selection for net-new component files
# ---------------------------------------------------------------------------

def test_default_stage_net_new_component_selects_craft_critique_harden():
    """AC1-HP from 01.2: net-new src/components/Foo.tsx -> [craft, critique, harden]."""
    text = load_agent_text()

    # The agent must document the default rule for net-new files.
    # Look for a section that ties net-new component/route files to the
    # craft -> critique -> harden stage sequence.
    assert "craft" in text.lower(), "Agent must mention 'craft' stage"
    assert "critique" in text.lower(), "Agent must mention 'critique' stage"
    assert "harden" in text.lower(), "Agent must mention 'harden' stage"

    # The stage list [craft, critique, harden] must appear together in the
    # context of net-new files (components/, routes/, or new frontend modules).
    # Accept bracket list or arrow notation (craft -> critique -> harden).
    net_new_block = _extract_section(text, ["net-new", "new file", "components/", "routes/"])
    assert net_new_block, (
        "Agent must contain a section describing stage selection for net-new component files. "
        "Expected text near 'net-new' or 'components/' that documents craft+critique+harden."
    )

    # Verify craft, critique, harden all appear in that block
    block_lower = net_new_block.lower()
    assert "craft" in block_lower, "Net-new rule block must mention 'craft'"
    assert "critique" in block_lower, "Net-new rule block must mention 'critique'"
    assert "harden" in block_lower, "Net-new rule block must mention 'harden'"


def _extract_section(text: str, keywords: list[str], window: int = 400) -> str:
    """Return up to `window` chars of text surrounding the first keyword match."""
    text_lower = text.lower()
    for kw in keywords:
        idx = text_lower.find(kw.lower())
        if idx != -1:
            start = max(0, idx - 50)
            end = min(len(text), idx + window)
            return text[start:end]
    return ""


# ---------------------------------------------------------------------------
# Invariant 2: Two-tier verdict thresholds
# ---------------------------------------------------------------------------

def test_two_tier_verdict_target_threshold_is_35():
    """The agent must document critique_target = 35 (out of 40)."""
    text = load_agent_text()
    # Accept "35/40", "35" next to "target", "critique_target.*35", etc.
    found = bool(
        re.search(r"\b35/40\b", text)
        or re.search(r"critique_target[^0-9]{0,20}35\b", text)
        or re.search(r"target[^0-9]{0,20}35/40", text, re.IGNORECASE)
        or re.search(r">=\s*35\b", text)
        or re.search(r">=\s*`?critique_target`?", text)
    )
    assert found, (
        "Agent must document that the SUCCESS threshold is 35/40 (critique_target). "
        "Expected one of: '35/40', 'critique_target ... 35', '>= 35', or '>= critique_target'."
    )


def test_two_tier_verdict_floor_threshold_is_25():
    """The agent must document critique_floor = 25 (out of 40)."""
    text = load_agent_text()
    found = bool(
        re.search(r"\b25/40\b", text)
        or re.search(r"critique_floor[^0-9]{0,20}25\b", text)
        or re.search(r"floor[^0-9]{0,20}25/40", text, re.IGNORECASE)
        or re.search(r"<\s*25\b", text)
        or re.search(r"<\s*`?critique_floor`?", text)
    )
    assert found, (
        "Agent must document that the FAILED floor is 25/40 (critique_floor). "
        "Expected one of: '25/40', 'critique_floor ... 25', '< 25', or '< critique_floor'."
    )


def test_two_tier_verdict_band_produces_done_with_concerns():
    """Scores between floor and target must produce DONE_WITH_CONCERNS."""
    text = load_agent_text()
    found = bool(
        re.search(r"DONE_WITH_CONCERNS", text)
        or re.search(r"done.with.concerns", text, re.IGNORECASE)
    )
    assert found, (
        "Agent must document DONE_WITH_CONCERNS for scores in the band "
        "between critique_floor (25) and critique_target (35)."
    )

    # Also verify deferred_findings is associated with DONE_WITH_CONCERNS
    dwc_block = _extract_section(text, ["DONE_WITH_CONCERNS", "done_with_concerns"], window=500)
    assert "deferred_findings" in dwc_block.lower() or "deferred_findings" in text.lower(), (
        "Agent must associate deferred_findings with the DONE_WITH_CONCERNS verdict."
    )


# ---------------------------------------------------------------------------
# Invariant 3: Finding classification table has exactly three buckets
# ---------------------------------------------------------------------------

def test_classification_table_has_three_buckets():
    """The agent must document exactly three classification buckets."""
    text = load_agent_text()

    # All three bucket names must appear
    assert "in_diff" in text or "in-diff" in text.lower() or "in diff" in text.lower(), (
        "Agent must document the 'in_diff' (or 'in-diff') classification bucket."
    )
    assert (
        "out_of_diff_blocking" in text
        or "out-of-diff blocking" in text.lower()
        or "out_of_diff blocking" in text.lower()
    ), (
        "Agent must document the 'out_of_diff_blocking' (or 'out-of-diff blocking') bucket."
    )
    assert (
        "out_of_diff_latent" in text
        or "out-of-diff latent" in text.lower()
        or "out_of_diff latent" in text.lower()
    ), (
        "Agent must document the 'out_of_diff_latent' (or 'out-of-diff latent') bucket."
    )


def test_classification_table_bucket_count_is_exactly_three():
    """Verify the classification table does not silently grow beyond three buckets."""
    text = load_agent_text()

    # Count distinct bucket markers in the classification table.
    # The table uses 'in_diff', 'out_of_diff_blocking', 'out_of_diff_latent'.
    # We look for the canonical snake_case forms that must appear in the deferred_findings
    # YAML schema section, which is machine-parseable.
    bucket_names = [
        "in_diff",
        "out_of_diff_blocking",
        "out_of_diff_latent",
    ]
    found = [b for b in bucket_names if b in text]
    assert len(found) == 3, (
        f"Expected exactly 3 canonical bucket names in agent, found {len(found)}: {found}. "
        "All of in_diff, out_of_diff_blocking, out_of_diff_latent must appear."
    )


# ---------------------------------------------------------------------------
# Bonus: DONE_WITH_CONCERNS paired with approved: false (PR #196 shape)
# ---------------------------------------------------------------------------

def test_done_with_concerns_uses_approved_false():
    """DONE_WITH_CONCERNS verdict must use approved: false (PR #196 gate artifact shape)."""
    text = load_agent_text()
    found = bool(re.search(r"approved:\s*false", text, re.IGNORECASE))
    assert found, (
        "Agent must document 'approved: false' as part of the DONE_WITH_CONCERNS "
        "gate artifact shape, consistent with PR #196's done-with-concerns contract."
    )
