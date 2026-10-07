"""
Tests for the PRODUCT.md dispatch-time gate in orchestrator.py.

When /operator resolves executor=impeccable for a task, it re-checks PRODUCT.md
at dispatch time using the same three-location search and stale-check that /spec uses.

AC1-HP: PRODUCT.md present (>=200 chars, no TODO dominance) -> dispatch proceeds.
AC2-ERR: PRODUCT.md missing entirely -> emits <help reason="missing-product-md">, no dispatch.
AC3-EDGE: PRODUCT.md stale ([TODO] < 200 chars) -> treated as missing, emits <help>.
AC4-EDGE: PRODUCT.md found via .agents/context/ fallback -> dispatch proceeds.
"""
import sys
from pathlib import Path


# The functions we are testing live in skills/execute/orchestrator.py.
# Import it relative to the repo root.
REPO_ROOT = Path(__file__).parent.parent.parent
sys.path.insert(0, str(REPO_ROOT / "skills" / "execute"))

from orchestrator import (  # noqa: E402
    check_product_md_for_dispatch,
    find_product_md,
    is_product_md_stale,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_product_md(root: Path, content: str, location: str = "root") -> Path:
    """Write a PRODUCT.md at the specified location within root."""
    if location == "root":
        path = root / "PRODUCT.md"
    elif location == ".agents/context":
        d = root / ".agents" / "context"
        d.mkdir(parents=True, exist_ok=True)
        path = d / "PRODUCT.md"
    elif location == "docs":
        d = root / "docs"
        d.mkdir(parents=True, exist_ok=True)
        path = d / "PRODUCT.md"
    else:
        raise ValueError(f"Unknown location: {location}")
    path.write_text(content)
    return path


GOOD_PRODUCT_CONTENT = "A" * 250  # 250 chars, no TODO -> valid


# ---------------------------------------------------------------------------
# AC1-HP: PRODUCT.md present (500 chars) -> dispatch proceeds
# ---------------------------------------------------------------------------

def test_ac1_hp_product_md_present_dispatch_proceeds(tmp_path, capsys):
    """AC1-HP: given PRODUCT.md with 500 chars at repo root, dispatch proceeds (no help emitted)."""
    _make_product_md(tmp_path, "X" * 500)
    result = check_product_md_for_dispatch(
        repo_root=tmp_path,
        plan_path="plan.md",
        stages=["craft", "critique", "harden"],
    )
    captured = capsys.readouterr()
    assert result is True, "Dispatch should proceed when PRODUCT.md is valid"
    assert "<help" not in captured.out, "No <help> should be emitted for valid PRODUCT.md"


# ---------------------------------------------------------------------------
# AC2-ERR: PRODUCT.md missing -> emits <help>, no dispatch
# ---------------------------------------------------------------------------

def test_ac2_err_product_md_missing_emits_help(tmp_path, capsys):
    """AC2-ERR: PRODUCT.md missing -> emits <help reason='missing-product-md'>, returns False."""
    result = check_product_md_for_dispatch(
        repo_root=tmp_path,
        plan_path="plan.md",
        stages=["craft", "critique"],
    )
    captured = capsys.readouterr()
    assert result is False, "Dispatch should be blocked when PRODUCT.md is missing"
    assert '<help reason="missing-product-md"' in captured.out, (
        "Must emit <help reason='missing-product-md'> when PRODUCT.md is absent"
    )


# ---------------------------------------------------------------------------
# AC3-EDGE: PRODUCT.md stale -> treated as missing
# ---------------------------------------------------------------------------

def test_ac3_edge_product_md_stale_treated_as_missing(tmp_path, capsys):
    """AC3-EDGE: PRODUCT.md with '[TODO]' and <200 chars -> treated as missing, emits <help>."""
    _make_product_md(tmp_path, "[TODO] fill this in later")
    result = check_product_md_for_dispatch(
        repo_root=tmp_path,
        plan_path="plan.md",
        stages=["craft"],
    )
    captured = capsys.readouterr()
    assert result is False, "Stale PRODUCT.md should block dispatch"
    assert '<help reason="missing-product-md"' in captured.out, (
        "Must emit <help reason='missing-product-md'> when PRODUCT.md is stale"
    )


# ---------------------------------------------------------------------------
# AC3-EDGE variant: stale-check details
# ---------------------------------------------------------------------------


def test_is_product_md_stale_todo_dominance():
    """Content with TODO dominance (>25% [TODO] markers) is stale."""
    # 10 [TODO] markers in 200 chars of text -> TODO dominance
    content = "[TODO] " * 15 + "A" * 100
    assert is_product_md_stale(content) is True


# ---------------------------------------------------------------------------
# AC4-EDGE: PRODUCT.md in .agents/context/ fallback -> dispatch proceeds
# ---------------------------------------------------------------------------

def test_ac4_edge_product_md_fallback_agents_context(tmp_path, capsys):
    """AC4-EDGE: PRODUCT.md at .agents/context/PRODUCT.md -> dispatch proceeds."""
    _make_product_md(tmp_path, "B" * 300, location=".agents/context")
    result = check_product_md_for_dispatch(
        repo_root=tmp_path,
        plan_path="plan.md",
        stages=["craft", "harden"],
    )
    captured = capsys.readouterr()
    assert result is True, "Dispatch should proceed when PRODUCT.md is in .agents/context/"
    assert "<help" not in captured.out


# ---------------------------------------------------------------------------
# Search order: root wins over fallbacks
# ---------------------------------------------------------------------------

def test_find_product_md_prefers_root(tmp_path):
    """Root PRODUCT.md wins over .agents/context/ fallback."""
    root_path = _make_product_md(tmp_path, GOOD_PRODUCT_CONTENT)
    _make_product_md(tmp_path, "other content", location=".agents/context")
    found = find_product_md(tmp_path)
    assert found == root_path


def test_find_product_md_docs_fallback(tmp_path):
    """docs/ is the last fallback location."""
    docs_path = _make_product_md(tmp_path, GOOD_PRODUCT_CONTENT, location="docs")
    found = find_product_md(tmp_path)
    assert found == docs_path


# ---------------------------------------------------------------------------
# Help message evidence field includes plan path and stages
# ---------------------------------------------------------------------------

def test_help_message_includes_plan_path_in_evidence(tmp_path, capsys):
    """The <help> evidence attribute must include the plan path and stages."""
    check_product_md_for_dispatch(
        repo_root=tmp_path,
        plan_path="myplan.md",
        stages=["craft", "harden"],
    )
    captured = capsys.readouterr()
    assert "myplan.md, stages: [craft, harden]" in captured.out


# ---------------------------------------------------------------------------
# AC3-EDGE: PRODUCT.md deleted between /spec and dispatch (Phase 04.2 requirement)
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# AC5-EDGE: unicode-heavy PRODUCT.md passes byte gate but not char gate
# ---------------------------------------------------------------------------

def test_ac5_edge_unicode_product_md_passes_byte_gate(tmp_path, capsys):
    """AC5-EDGE: PRODUCT.md with multibyte chars summing to >=200 bytes but <200 chars -> dispatch proceeds.

    /spec gate counts bytes (wc -c); /operator must also count bytes so both gates
    agree on the same PRODUCT.md. A content of 150 two-byte characters = 300 bytes
    should pass both gates.
    """
    # 150 x '£' (U+00A3) = 2 bytes each in UTF-8 = 300 bytes, but only 150 chars
    content = "£" * 150  # 150 chars, 300 bytes
    assert len(content) == 150, "sanity: 150 chars"
    assert len(content.encode("utf-8")) == 300, "sanity: 300 bytes"

    _make_product_md(tmp_path, content)

    result = check_product_md_for_dispatch(
        repo_root=tmp_path,
        plan_path="plan.md",
        stages=["craft"],
    )
    captured = capsys.readouterr()
    assert result is True, (
        "300-byte PRODUCT.md must pass the byte gate even though it is only 150 chars.\n"
        f"help output: {captured.out}"
    )
    assert "<help" not in captured.out


# ---------------------------------------------------------------------------
# AC6-ERR: PRODUCT.md read error -> emits <help>, returns False
# ---------------------------------------------------------------------------

def test_ac6_err_product_md_unreadable_emits_help(tmp_path, capsys):
    """AC6-ERR: PRODUCT.md exists but is unreadable (chmod 0) -> emits <help reason='missing-product-md'>, returns False."""
    product_path = _make_product_md(tmp_path, "X" * 300)
    # Make it unreadable
    product_path.chmod(0o000)
    try:
        result = check_product_md_for_dispatch(
            repo_root=tmp_path,
            plan_path="plan.md",
            stages=["craft"],
        )
        captured = capsys.readouterr()
        assert result is False, "Unreadable PRODUCT.md must block dispatch"
        assert '<help reason="missing-product-md"' in captured.out, (
            "Unreadable PRODUCT.md must emit <help reason='missing-product-md'>"
        )
        assert "read_error=" in captured.out, (
            "Evidence attribute must include read_error= hint"
        )
    finally:
        # Restore perms so tmp_path cleanup works
        product_path.chmod(0o644)
