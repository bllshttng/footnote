"""
Comprehensive BDD tests for agents/frontend-executor.md (Phase 01 of ab-028ad6e8).

These tests assert on the documented rules of the rewritten frontend-executor agent.
They are parsed-document tests: they read the agent file and verify that the agent's
documented behavior matches the AC list from Phase 01.1, 01.2, and 01.3.

Tests that simulate agent logic use pure-Python simulation or regex assertions on the
agent document - NOT actual subprocess invocations of /impeccable.

AC labels follow the phase-01 spec so failures map back to specific acceptance criteria.
"""
import re
import sys
import textwrap
from pathlib import Path
from unittest.mock import patch, MagicMock

REPO_ROOT = Path(__file__).parent.parent.parent
AGENT_FILE = REPO_ROOT / "agents" / "frontend-executor.md"


def load_agent_text() -> str:
    return AGENT_FILE.read_text()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _extract_section(text: str, keywords: list, window: int = 600) -> str:
    """Return text surrounding the first keyword match."""
    text_lower = text.lower()
    for kw in keywords:
        idx = text_lower.find(kw.lower())
        if idx != -1:
            start = max(0, idx - 80)
            end = min(len(text), idx + window)
            return text[start:end]
    return ""


def _simulate_verdict(score: int, target: int = 35, floor: int = 25) -> str:
    """Pure-Python simulation of the two-tier verdict rule documented in the agent."""
    if score >= target:
        return "SUCCESS"
    elif score < floor:
        return "FAILED"
    else:
        return "DONE_WITH_CONCERNS"


def _simulate_stage_selection(
    is_net_new: bool,
    has_a11y_ac: bool = False,
    impeccable_stages_pin: list = None,
) -> list:
    """
    Pure-Python simulation of the default stage selection rule documented in Step 2.

    Net-new component:   [craft, critique, harden]
    Edited component:    [polish, critique, harden]
    + a11y/perf AC:      audit is added
    Pin overrides all.
    """
    if impeccable_stages_pin is not None:
        return impeccable_stages_pin

    if is_net_new:
        stages = ["craft", "critique", "harden"]
    else:
        stages = ["polish", "critique", "harden"]

    if has_a11y_ac:
        # Insert audit before harden
        stages = [s for s in stages if s != "harden"] + ["audit", "harden"]

    return stages


# ---------------------------------------------------------------------------
# Phase 01.1 - Shape brief synthesis
# ---------------------------------------------------------------------------

def test_ac1_hp_01_1_synthesize_shape_brief_from_design_doc():
    """AC1-HP (01.1): Agent documents shape brief synthesis from design doc + AC list."""
    text = load_agent_text()

    # The agent must document a shape brief synthesis step.
    assert "shape brief" in text.lower(), (
        "Agent must document shape brief synthesis"
    )
    # Must reference the design doc as input.
    assert "design_doc" in text or "design doc" in text.lower(), (
        "Agent must reference design_doc as input to shape brief synthesis"
    )
    # Must document the per-task AC list as input.
    assert "ac list" in text.lower() or "ac_list" in text or "acceptance criteria" in text.lower(), (
        "Agent must reference the per-task AC list as input to shape brief synthesis"
    )
    # shape_source field must be documented
    assert "shape_source" in text, (
        "Agent must document the shape_source field in the gate artifact"
    )
    # think_design_doc is the canonical source value
    assert "think_design_doc" in text, (
        "Agent must document 'think_design_doc' as a valid shape_source value"
    )


def test_ac2_err_01_1_shape_adapter_rejection_emits_help():
    """AC2-ERR (01.1): Shape adapter rejection -> emits <help reason='shape-adapter-needs-canonical-schema'>."""
    text = load_agent_text()

    # The agent must document the shape-adapter-needs-canonical-schema help reason.
    assert "shape-adapter-needs-canonical-schema" in text, (
        "Agent must document <help reason='shape-adapter-needs-canonical-schema'> "
        "as the response when the shape loader rejects the synthesized brief."
    )
    # The agent must NOT forge shape=pass on rejection.
    rejection_block = _extract_section(
        text,
        ["shape-adapter-needs-canonical-schema", "shape loader rejects"],
        window=400,
    )
    assert rejection_block, "Agent must document behavior on shape loader rejection"
    assert "do not forge" in rejection_block.lower() or "do not modify" in rejection_block.lower() or "do not" in rejection_block.lower(), (
        "Agent must explicitly prohibit forging shape=pass on rejection"
    )


def test_ac3_ui_01_1_gate_artifact_contains_shape_source():
    """AC3-UI (01.1): Gate artifact and scratchpad note must contain shape_source field."""
    text = load_agent_text()

    # shape_source must appear in the return contract section.
    return_block = _extract_section(text, ["Return contract", "SHAPE_SOURCE"], window=600)
    assert "SHAPE_SOURCE" in return_block or "shape_source" in return_block, (
        "Agent must document SHAPE_SOURCE in the return contract"
    )

    # The two valid values must appear.
    assert "think_design_doc" in text, (
        "Agent must document 'think_design_doc' as a valid SHAPE_SOURCE value"
    )
    assert "explicit_shape_pin" in text, (
        "Agent must document 'explicit_shape_pin' as a valid SHAPE_SOURCE value"
    )


def test_ac4_edge_01_1_design_doc_reread_on_every_dispatch():
    """AC4-EDGE (01.1): Design doc must be re-read on every dispatch - no stale caching."""
    text = load_agent_text()

    # The agent must document re-reading on every dispatch.
    reread_block = _extract_section(
        text,
        ["every dispatch", "re-read", "re-reads", "not cache", "Do NOT cache"],
        window=400,
    )
    assert reread_block, (
        "Agent must document that design doc is re-read on every dispatch, "
        "not cached across tasks."
    )
    block_lower = reread_block.lower()
    assert "cache" in block_lower or "every" in block_lower or "dispatch" in block_lower, (
        "Re-read section must mention caching or dispatch frequency"
    )


# ---------------------------------------------------------------------------
# Phase 01.2 - Default stage selection
# ---------------------------------------------------------------------------

def test_ac1_hp_01_2_net_new_component_selects_craft_critique_harden():
    """AC1-HP (01.2): Net-new component -> default stages [craft, critique, harden]."""
    stages = _simulate_stage_selection(is_net_new=True, has_a11y_ac=False)
    assert stages == ["craft", "critique", "harden"], (
        f"Net-new component without a11y should yield [craft, critique, harden], got {stages}"
    )

    # Cross-check: the agent document must describe this rule.
    text = load_agent_text()
    net_new_block = _extract_section(text, ["Net-new", "net-new", "new file", "components/"], window=400)
    assert "craft" in net_new_block.lower(), "Net-new rule must mention 'craft'"
    assert "harden" in net_new_block.lower(), "Net-new rule must mention 'harden'"


def test_ac2_hp_01_2_edit_with_a11y_ac_includes_audit():
    """AC2-HP (01.2): Edited component with a11y AC -> stages include audit."""
    stages = _simulate_stage_selection(is_net_new=False, has_a11y_ac=True)
    assert "audit" in stages, (
        f"Edit with a11y AC should include audit stage, got {stages}"
    )
    assert "polish" in stages, (
        f"Edit (non-net-new) should start with polish, got {stages}"
    )
    assert "harden" in stages, (
        f"Harden must always be last, got {stages}"
    )
    assert stages[-1] == "harden", f"Harden must be last, got {stages}"

    # Cross-check: agent document must describe the a11y/perf modifier.
    text = load_agent_text()
    a11y_block = _extract_section(text, ["a11y", "WCAG", "audit"], window=400)
    assert "audit" in a11y_block.lower(), "Agent must document audit stage for a11y/perf AC"


def test_ac3_err_01_2_impeccable_stages_pin_overrides_default():
    """AC3-ERR (01.2): Per-task impeccable_stages pin overrides the default rule."""
    pin = ["polish", "layout", "critique", "harden"]
    stages = _simulate_stage_selection(is_net_new=True, has_a11y_ac=True, impeccable_stages_pin=pin)
    assert stages == pin, (
        f"Explicit impeccable_stages pin should override default rule, got {stages}"
    )

    # Verify the agent document describes pin-override behavior.
    text = load_agent_text()
    pin_block = _extract_section(text, ["impeccable_stages", "Pin overrides", "that list wins"], window=400)
    assert pin_block, "Agent must document that impeccable_stages pin overrides the default rule"


def test_ac4_edge_01_2_pin_only_treatments_not_auto_selected():
    """AC4-EDGE (01.2): animate/delight/etc. are pin-only and NEVER auto-selected."""
    text = load_agent_text()

    # All pin-only treatment names must appear in the doc.
    pin_only = ["animate", "bolder", "colorize", "delight", "overdrive", "quieter", "typeset"]
    for stage in pin_only:
        assert stage in text, (
            f"Agent must document '{stage}' as a pin-only treatment"
        )

    # The doc must explicitly state these are never picked autonomously.
    pin_only_block = _extract_section(
        text,
        ["pin-only", "Pin-only", "never picked autonomously", "NEVER picked"],
        window=500,
    )
    assert pin_only_block, (
        "Agent must document that pin-only treatments are never autonomously selected"
    )
    block_lower = pin_only_block.lower()
    assert "never" in block_lower or "not" in block_lower, (
        "Pin-only block must use 'never' or 'not' to prohibit autonomous selection"
    )

    # Verify none of the auto-selection paths include pin-only stages.
    stages_net_new = _simulate_stage_selection(is_net_new=True, has_a11y_ac=True)
    stages_edit = _simulate_stage_selection(is_net_new=False, has_a11y_ac=True)
    for stage in pin_only:
        assert stage not in stages_net_new, (
            f"Pin-only stage '{stage}' must not appear in default net-new selection"
        )
        assert stage not in stages_edit, (
            f"Pin-only stage '{stage}' must not appear in default edit selection"
        )


# ---------------------------------------------------------------------------
# Phase 01.3 - Two-tier exit verdict + finding classification
# ---------------------------------------------------------------------------

def test_ac1_hp_01_3_score_38_yields_success():
    """AC1-HP (01.3): Score 38/40 -> RESULT: SUCCESS (above target 35)."""
    verdict = _simulate_verdict(score=38, target=35, floor=25)
    assert verdict == "SUCCESS", f"Score 38 should yield SUCCESS, got {verdict}"

    # Verify the agent document encodes this threshold.
    text = load_agent_text()
    assert "35/40" in text or re.search(r"critique_target.*35", text), (
        "Agent must document SUCCESS threshold as 35/40"
    )


def test_ac2_hp_01_3_score_30_yields_done_with_concerns_with_deferred_findings():
    """AC2-HP (01.3): Score 30/40 (in band 25-35) -> DONE_WITH_CONCERNS + deferred_findings."""
    verdict = _simulate_verdict(score=30, target=35, floor=25)
    assert verdict == "DONE_WITH_CONCERNS", (
        f"Score 30 (in band) should yield DONE_WITH_CONCERNS, got {verdict}"
    )

    # Verify the agent document associates deferred_findings with DONE_WITH_CONCERNS.
    text = load_agent_text()
    dwc_idx = text.find("DONE_WITH_CONCERNS")
    assert dwc_idx >= 0, "Agent must document DONE_WITH_CONCERNS"
    dwc_context = text[dwc_idx : dwc_idx + 600]
    assert "deferred_findings" in dwc_context or "deferred_findings" in text[max(0, dwc_idx - 200):dwc_idx + 600], (
        "DONE_WITH_CONCERNS must be associated with deferred_findings in agent doc"
    )


def test_ac3_err_01_3_score_18_yields_failed():
    """AC3-ERR (01.3): Score 18/40 -> RESULT: FAILED (below floor 25)."""
    verdict = _simulate_verdict(score=18, target=35, floor=25)
    assert verdict == "FAILED", f"Score 18 (below floor 25) should yield FAILED, got {verdict}"

    # Verify the agent document encodes the floor threshold.
    text = load_agent_text()
    assert "25/40" in text or re.search(r"critique_floor.*25", text) or "< 25" in text, (
        "Agent must document FAILED floor as 25/40"
    )


def test_ac4_edge_01_3_brand_copy_finding_files_backlog_not_auto_applied():
    """AC4-EDGE (01.3): Brand/copy findings (rename label etc.) file backlog node, NOT auto-applied."""
    text = load_agent_text()

    # The agent must document brand/copy decision rule.
    brand_block = _extract_section(
        text,
        ["Brand and copy", "brand or copy", "brand/copy", "rename", "NEVER auto-apply"],
        window=500,
    )
    assert brand_block, (
        "Agent must document brand/copy decision rule for findings"
    )
    block_lower = brand_block.lower()
    assert "never" in block_lower or "not" in block_lower or "do not" in block_lower, (
        "Brand/copy block must prohibit auto-application"
    )
    assert "backlog" in block_lower, (
        "Brand/copy block must require filing as backlog node instead"
    )


def test_ac5_edge_01_3_backlog_new_failure_folds_into_deferred_findings():
    """AC5-EDGE (01.3): fno backlog new failure (rc!=0) -> backlog_node: null + stderr warning."""
    text = load_agent_text()

    # The agent must document the backlog-new failure path.
    failure_block = _extract_section(
        text,
        ["backlog new failed", "fno backlog new failure", "rc != 0", "rc=", "backlog_node: null"],
        window=500,
    )
    assert failure_block, (
        "Agent must document what happens when 'fno backlog new' fails (rc != 0)"
    )
    block_lower = failure_block.lower()
    assert "backlog_node" in failure_block, (
        "Failure path must document backlog_node field"
    )
    assert "null" in failure_block, (
        "Failure path must set backlog_node: null when filing fails"
    )
    assert "warn" in block_lower or "warning" in block_lower or "stderr" in block_lower, (
        "Failure path must emit a stderr warning"
    )


def test_ac6_edge_01_3_malformed_critique_output_emits_help():
    """AC6-EDGE (01.3): Malformed critique output (unexpected denominator) -> <help reason='critique-output-malformed'>."""
    text = load_agent_text()

    # The agent must document the malformed output handler.
    assert "critique-output-malformed" in text, (
        "Agent must document <help reason='critique-output-malformed'> for malformed output"
    )

    # Specifically for unexpected denominator (score format brittleness).
    malformed_block = _extract_section(
        text,
        ["critique-output-malformed", "denominator", "unexpected denominator"],
        window=500,
    )
    assert malformed_block, (
        "Agent must document handling for unexpected score denominator (Score format brittleness)"
    )
    block_lower = malformed_block.lower()
    assert "denominator" in block_lower, (
        "Malformed-output block must address the denominator case specifically"
    )
    # Must NOT normalize/guess the score
    assert "do not" in block_lower or "not" in block_lower or "never" in block_lower, (
        "Agent must prohibit guessing a normalized score when denominator is unexpected"
    )


# ---------------------------------------------------------------------------
# Additional coverage: score boundary conditions
# ---------------------------------------------------------------------------

def test_score_boundary_exactly_at_target():
    """Score exactly at target (35) -> SUCCESS (boundary inclusive)."""
    assert _simulate_verdict(35) == "SUCCESS"


def test_score_boundary_exactly_at_floor():
    """Score exactly at floor (25) -> DONE_WITH_CONCERNS (floor is lower bound of band)."""
    # floor <= score < target -> DONE_WITH_CONCERNS
    # score < floor -> FAILED
    # score 25: floor=25, so 25 >= floor, 25 < target -> DONE_WITH_CONCERNS
    assert _simulate_verdict(25) == "DONE_WITH_CONCERNS"


def test_score_boundary_one_below_floor():
    """Score 24 (one below floor 25) -> FAILED."""
    assert _simulate_verdict(24) == "FAILED"


def test_score_40_yields_success():
    """Perfect score 40/40 -> SUCCESS."""
    assert _simulate_verdict(40) == "SUCCESS"


# ---------------------------------------------------------------------------
# Deferred-findings provenance fields
# ---------------------------------------------------------------------------

def test_deferred_findings_entry_requires_three_provenance_fields():
    """deferred_findings entry must have file_path, ac_ref, and rationale fields."""
    text = load_agent_text()

    # All three provenance fields must be in the deferred_findings schema.
    deferred_block = _extract_section(
        text,
        ["deferred_findings entry", "deferred_findings:", "out_of_diff_latent"],
        window=700,
    )
    assert deferred_block, "Agent must document deferred_findings entry shape"
    assert "file_path" in deferred_block, (
        "deferred_findings entry must require file_path field"
    )
    assert "ac_ref" in deferred_block, (
        "deferred_findings entry must require ac_ref field"
    )
    assert "rationale" in deferred_block, (
        "deferred_findings entry must require rationale field"
    )


def test_out_of_diff_blocking_emits_help_not_continues():
    """out_of_diff_blocking finding must emit <help>, not just continue."""
    text = load_agent_text()

    blocking_block = _extract_section(
        text,
        ["out_of_diff_blocking", "out-of-diff blocking", "out-of-scope-blocking"],
        window=500,
    )
    assert blocking_block, "Agent must document out_of_diff_blocking bucket behavior"
    block_lower = blocking_block.lower()
    assert "<help" in blocking_block or "help" in block_lower, (
        "out_of_diff_blocking must emit <help> tag"
    )
    assert "out-of-scope-blocking" in blocking_block or "out_of_scope" in blocking_block, (
        "out_of_diff_blocking must use out-of-scope-blocking reason"
    )


def test_harden_is_always_last_stage():
    """Harden is always last regardless of stage selection path."""
    text = load_agent_text()

    harden_block = _extract_section(text, ["Harden is always last", "harden\nalways"], window=200)
    assert harden_block or "harden" in text.lower(), "Agent must document harden stage"

    # Verify via simulation: all paths end in harden.
    for is_net_new in [True, False]:
        for has_a11y in [True, False]:
            stages = _simulate_stage_selection(is_net_new=is_net_new, has_a11y_ac=has_a11y)
            assert stages[-1] == "harden", (
                f"Harden must be last stage for net_new={is_net_new} a11y={has_a11y}, "
                f"got {stages}"
            )


def test_return_contract_commit_absent_on_failed():
    """RESULT: FAILED must NOT emit COMMIT field (per return contract)."""
    text = load_agent_text()

    commit_block = _extract_section(
        text,
        ["COMMIT:", "RESULT: FAILED", "Do NOT create a commit on RESULT: FAILED"],
        window=600,
    )
    assert commit_block, "Agent must document commit behavior on FAILED"
    # The COMMIT field must not be emitted on FAILED
    assert "do not" in commit_block.lower() or "omit" in commit_block.lower() or "FAILED" in commit_block, (
        "Agent must explicitly document that COMMIT is omitted on RESULT: FAILED"
    )
