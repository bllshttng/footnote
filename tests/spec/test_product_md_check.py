#!/usr/bin/env python3
"""Tests for the PRODUCT.md prereq check used by /blueprint (Phase 02.1).

Acceptance criteria:
  AC1-HP: PRODUCT.md present (>=200 chars) -> no prerequisites block, no warning
  AC2-HP: No PRODUCT.md found -> prerequisites block written + warning on stderr
  AC3-ERR: PRODUCT.md exists with 50 chars of [TODO] -> treated as missing (stale)
  AC4-EDGE: /blueprint does NOT block plan creation when PRODUCT.md is missing

Run: python3 -m pytest tests/spec/test_product_md_check.py -v
"""
import subprocess
import tempfile
import textwrap
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CHECK_SCRIPT = REPO_ROOT / "scripts" / "check-product-md.sh"


def _run_check(plan_path: Path, repo_root: Path) -> subprocess.CompletedProcess:
    """Run check-product-md.sh and return the result."""
    return subprocess.run(
        ["bash", str(CHECK_SCRIPT), str(plan_path)],
        capture_output=True,
        text=True,
        env={
            "PATH": "/usr/bin:/bin:/usr/local/bin",
            "REPO_ROOT": str(repo_root),
        },
    )


def _make_plan_with_executor(plan_path: Path, executor: str = "impeccable") -> None:
    """Write a minimal single-doc plan with executor set."""
    plan_path.parent.mkdir(parents=True, exist_ok=True)
    plan_path.write_text(
        textwrap.dedent(f"""\
        ---
        title: Test Plan
        executor: {executor}
        created: 2026-05-06
        ---

        # Test Plan
        """)
    )


def test_ac1_hp_product_md_present_no_warning():
    """AC1-HP: PRODUCT.md >= 200 chars -> no prerequisites block, no stderr warning."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "impeccable")

        # Write a valid PRODUCT.md (>= 200 chars, no [TODO])
        product_md = repo_root / "PRODUCT.md"
        product_md.write_text(
            "A" * 200 + "\nThis is the product description that satisfies the 200-char minimum.\n"
        )

        result = _run_check(plan_path, repo_root)

        assert result.returncode == 0, f"Expected exit 0, got {result.returncode}.\nstdout: {result.stdout}\nstderr: {result.stderr}"
        assert "prerequisites:" not in result.stdout, "Should not add prerequisites block when PRODUCT.md is present"
        assert "warning:" not in result.stderr.lower(), f"Unexpected warning: {result.stderr}"


def test_ac2_hp_product_md_missing_writes_prerequisites():
    """AC2-HP: No PRODUCT.md -> prerequisites block in plan frontmatter + warning."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "impeccable")
        # No PRODUCT.md anywhere

        result = _run_check(plan_path, repo_root)

        # Exit 0 (plan still ships - this is heads-up only)
        assert result.returncode == 0, f"Expected exit 0 (plan ships), got {result.returncode}.\nstdout: {result.stdout}\nstderr: {result.stderr}"

        # The plan doc should now have a prerequisites block
        plan_content = plan_path.read_text()
        assert "prerequisites:" in plan_content, "Missing prerequisites: block in frontmatter"
        assert "kind: file" in plan_content, "Missing kind: file in prerequisites"
        assert "PRODUCT.md" in plan_content, "Missing PRODUCT.md path in prerequisites"

        # Warning should appear on stderr
        assert "warning:" in result.stderr.lower() or "warning:" in result.stdout.lower(), \
            f"Expected a warning. stderr={result.stderr!r} stdout={result.stdout!r}"


def test_ac3_err_stale_product_md_treated_as_missing():
    """AC3-ERR: PRODUCT.md with 50 chars of [TODO] content -> treated as missing."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "impeccable")

        # Stale PRODUCT.md: under 200 chars, placeholder content
        product_md = repo_root / "PRODUCT.md"
        product_md.write_text("[TODO] placeholder - fill this in later")

        result = _run_check(plan_path, repo_root)

        # Should behave like missing
        assert result.returncode == 0, f"Expected exit 0, got {result.returncode}.\nstderr: {result.stderr}"
        plan_content = plan_path.read_text()
        assert "prerequisites:" in plan_content, "Stale PRODUCT.md should be treated as missing"


def test_ac4_edge_no_executor_impeccable_no_check():
    """AC4-EDGE: Plan without executor: impeccable does not trigger the check."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "do")  # not impeccable
        # No PRODUCT.md

        result = _run_check(plan_path, repo_root)

        assert result.returncode == 0, f"Expected exit 0, got {result.returncode}"
        plan_content = plan_path.read_text()
        assert "prerequisites:" not in plan_content, \
            "Should not add prerequisites block for non-impeccable plan"


def test_product_md_fallback_agents_context():
    """AC2-HP variant: PRODUCT.md found in .agents/context/ fallback path -> no warning."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "impeccable")

        # Place PRODUCT.md in the fallback location
        fallback_dir = repo_root / ".agents" / "context"
        fallback_dir.mkdir(parents=True)
        (fallback_dir / "PRODUCT.md").write_text("B" * 250 + "\nFallback product context.\n")

        result = _run_check(plan_path, repo_root)

        assert result.returncode == 0
        plan_content = plan_path.read_text()
        assert "prerequisites:" not in plan_content, "Fallback PRODUCT.md should satisfy the check"


def test_product_md_fallback_docs():
    """PRODUCT.md in docs/ fallback -> no warning."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "impeccable")

        docs_dir = repo_root / "docs"
        docs_dir.mkdir(parents=True)
        (docs_dir / "PRODUCT.md").write_text("C" * 300 + "\nDocs product context.\n")

        result = _run_check(plan_path, repo_root)

        assert result.returncode == 0
        plan_content = plan_path.read_text()
        assert "prerequisites:" not in plan_content, "docs/ PRODUCT.md should satisfy the check"


def test_todo_dominance_treated_as_stale():
    """Gemini fix (PR #217 round 1): PRODUCT.md > 200 bytes but >25% [TODO] tokens
    must be treated as stale, matching orchestrator.py's is_product_md_stale.
    Without this, /blueprint silently passes a stub and /operator surprises the
    user by hard-blocking at dispatch time."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "impeccable")

        # 25 [TODO] tokens = 150 bytes of placeholder; total file = 250 bytes
        # 150/250 = 60% > 25% threshold -> stale
        product_md = repo_root / "PRODUCT.md"
        body = "[TODO]" * 25 + "x" * 100  # 150 + 100 = 250 bytes
        assert len(body) >= 200, "test fixture must be >= 200 bytes to isolate TODO dominance"
        product_md.write_text(body)

        result = _run_check(plan_path, repo_root)

        assert result.returncode == 0
        # Stale -> prerequisites block injected
        plan_content = plan_path.read_text()
        assert "prerequisites:" in plan_content, \
            "TODO-dominant PRODUCT.md must trigger the prerequisites block"
        assert "missing or stale" in result.stderr.lower() or "stale" in result.stderr.lower() \
            or "PRODUCT.md" in result.stderr, \
            f"expected stderr warning about stale PRODUCT.md, got: {result.stderr!r}"


def test_low_todo_count_still_passes():
    """Boundary: a few [TODO] markers in a substantial file should NOT trigger
    stale (only dominance does). 1 [TODO] = 6 bytes in a 1000-byte file = 0.6%
    is well under the 25% threshold."""
    with tempfile.TemporaryDirectory() as tmp:
        repo_root = Path(tmp) / "repo"
        plan_path = repo_root / "plans" / "my-plan.md"
        _make_plan_with_executor(plan_path, "impeccable")

        product_md = repo_root / "PRODUCT.md"
        # Substantial real content with one passing [TODO] reference
        body = (
            "Product: Acme. Helps small businesses stay organized.\n"
            "Customer: operations lead at a 50-200 person company.\n"
            "Key features: parsing, monitoring, reminders, drafts.\n"
            "[TODO] add competitor analysis section.\n"
            + "Detailed product context. " * 30
        )
        assert len(body) > 800
        product_md.write_text(body)

        result = _run_check(plan_path, repo_root)

        assert result.returncode == 0
        plan_content = plan_path.read_text()
        assert "prerequisites:" not in plan_content, \
            "PRODUCT.md with one [TODO] in a substantial file should pass"
