"""Regression tests for scripts/ci/check-parity-test-provenance.sh.

The gate reads the `parity-stage` / `parity-oracle` header of every
`crates/*/tests/*_parity.rs` and asserts the declaration against the
filesystem in BOTH directions: a `characterization` file must name an oracle
that is GONE, a `differential` file must name one that is PRESENT.

The two-sided assertion is the point. A one-sided check ("the oracle exists")
would pass a finished port that still advertises a live second implementation,
which is exactly the miscount this gate exists to prevent.

Output is captured via subprocess so the asserted returncode is the real one.
"""
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
GATE = ROOT / "scripts" / "ci" / "check-parity-test-provenance.sh"

DIFFERENTIAL = "//! parity-stage: differential\n//! parity-oracle: {oracle}\n//!\n//! prose.\n"
CHARACTERIZATION = (
    "//! parity-stage: characterization\n//! parity-oracle: {oracle}\n//!\n//! prose.\n"
)


def _run(root: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(GATE), "--root", str(root)],
        capture_output=True,
        text=True,
    )


def _tree(tmp_path: Path) -> Path:
    """A minimal fixture repo: crates/<crate>/tests/ plus a place for oracles."""
    (tmp_path / "crates" / "fixture" / "tests").mkdir(parents=True)
    (tmp_path / "scripts" / "lib").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "agents" / "harnesses").mkdir(parents=True)
    return tmp_path


def _parity(root: Path, name: str, body: str) -> Path:
    path = root / "crates" / "fixture" / "tests" / f"{name}_parity.rs"
    path.write_text(body, encoding="utf-8")
    return path


# --- the shipped tree -------------------------------------------------------


def test_shipped_tree_passes_and_reports_every_file() -> None:
    """AC4-HP: exit 0, and one report line per parity file. A silent pass is
    what let the original miscount stand, so the pass is not silent."""
    result = _run(ROOT)
    assert result.returncode == 0, result.stdout + result.stderr
    for name in (
        "claude_ask_parity.rs",
        "codex_ask_parity.rs",
        "kill_criteria_parity.rs",
        "verify_evidence_parity.rs",
    ):
        assert name in result.stdout, f"{name} not reported:\n{result.stdout}"
    assert "differential" in result.stdout
    assert "characterization" in result.stdout


# --- the two-sided refusal --------------------------------------------------


def test_characterization_with_a_live_oracle_fails(tmp_path: Path) -> None:
    """AC5-ERR. The leg is still there, so the golden is not the only witness
    and the file is lying about being a finished port."""
    root = _tree(tmp_path)
    (root / "scripts" / "lib" / "still-here.sh").write_text("#!/bin/sh\n")
    _parity(root, "ghost", CHARACTERIZATION.format(oracle="scripts/lib/still-here.sh"))

    result = _run(root)
    assert result.returncode != 0
    combined = result.stdout + result.stderr
    assert "ghost_parity.rs" in combined
    assert "characterization" in combined
    assert "scripts/lib/still-here.sh" in combined


def test_differential_with_a_dead_oracle_fails(tmp_path: Path) -> None:
    """AC6-ERR. The port finished and nobody converted the test, so CI still
    advertises a second implementation that no longer exists."""
    root = _tree(tmp_path)
    _parity(root, "stale", DIFFERENTIAL.format(oracle="scripts/lib/deleted.sh"))

    result = _run(root)
    assert result.returncode != 0
    combined = result.stdout + result.stderr
    assert "stale_parity.rs" in combined
    assert "characterization" in combined, "the refusal must name the fix"


# --- the header contract ----------------------------------------------------


def test_missing_header_fails_and_prints_the_required_form(tmp_path: Path) -> None:
    """AC7-ERR."""
    root = _tree(tmp_path)
    _parity(root, "bare", "//! prose only, no declaration.\n")

    result = _run(root)
    assert result.returncode != 0
    combined = result.stdout + result.stderr
    assert "bare_parity.rs" in combined
    assert "parity-stage" in combined
    assert "parity-oracle" in combined


def test_out_of_vocabulary_stage_fails(tmp_path: Path) -> None:
    """AC7-ERR. Exactly two values; anything else is a typo that would
    otherwise skip the assertion entirely."""
    root = _tree(tmp_path)
    _parity(root, "weird", "//! parity-stage: golden\n//! parity-oracle: x.sh\n")

    result = _run(root)
    assert result.returncode != 0
    combined = result.stdout + result.stderr
    assert "weird_parity.rs" in combined
    assert "golden" in combined


def test_missing_oracle_field_fails(tmp_path: Path) -> None:
    root = _tree(tmp_path)
    _parity(root, "half", "//! parity-stage: differential\n//! prose.\n")

    result = _run(root)
    assert result.returncode != 0
    assert "half_parity.rs" in result.stdout + result.stderr


# --- oracle resolution ------------------------------------------------------


def test_dotted_module_oracle_resolves_and_prints_the_resolution(
    tmp_path: Path,
) -> None:
    """AC8-EDGE. A Python oracle is named as a module; the gate resolves it to
    a path for the existence test and PRINTS that resolution, so a wrong
    mapping is visible rather than silently passing."""
    root = _tree(tmp_path)
    module = root / "cli" / "src" / "fno" / "agents" / "harnesses" / "claude.py"
    module.write_text("# real\n", encoding="utf-8")
    _parity(root, "dotted", DIFFERENTIAL.format(oracle="fno.agents.harnesses.claude"))

    result = _run(root)
    assert result.returncode == 0, result.stdout + result.stderr
    assert "cli/src/fno/agents/harnesses/claude.py" in result.stdout


def test_dotted_module_that_does_not_exist_fails_a_differential(
    tmp_path: Path,
) -> None:
    root = _tree(tmp_path)
    _parity(root, "missing", DIFFERENTIAL.format(oracle="fno.agents.harnesses.nope"))

    result = _run(root)
    assert result.returncode != 0
    assert "missing_parity.rs" in result.stdout + result.stderr


# --- degenerate trees -------------------------------------------------------


def test_a_tree_with_no_parity_files_fails_loud(tmp_path: Path) -> None:
    """A zero-file pass is an absence, and an absence has three explanations.
    The gate refuses rather than reporting a green it did not earn."""
    root = _tree(tmp_path)

    result = _run(root)
    assert result.returncode != 0
    assert "no *_parity.rs" in (result.stdout + result.stderr)
