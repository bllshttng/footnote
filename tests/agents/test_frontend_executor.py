"""
Parsed-document tests for agents/frontend-executor.md: each pins a help reason,
return-contract field or finding bucket the agent must keep documenting.
"""
from pathlib import Path

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
