"""Tests for the research eval scorer (US5, AC2): three mechanical assertions.

Green only if ALL pass:
  (a) zero uncited claims  - every claim cites a [Sn] that resolves to a real
      sources.jsonl row.
  (b) zero dead URLs       - every sidecar row is verified (or wayback-archived).
  (c) >=1 golden checklist item per section - the golden doc's headings are the
      checklist; each brief content section must cover at least one.

No model in the gate; the verify panel never changes the verdict.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.evals import research_grade as rg
from fno.research.core import Source


def _write(p: Path, text: str) -> Path:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")
    return p


def _sidecar(p: Path, rows: list[Source]) -> None:
    with p.open("w", encoding="utf-8") as fh:
        for r in rows:
            fh.write(r.to_json_line() + "\n")


GOLDEN = """# Discovery: example agency

## Financials
Revenue and budget lines.

## Inspection cadence
How often inspections happen.
"""

GREEN_BRIEF = """---
topic: "example agency"
slug: example-agency
stopped: declared
sources: example-agency.sources.jsonl
---

# example agency

## Financials
- The agency reports annual revenue figures. [S1]

## Inspection cadence
- Inspections happen on a fixed cadence. [S2]

## Sources

[S1]: https://a.example/financials
[S2]: https://b.example/cadence
"""


def _green_sidecar(p: Path) -> None:
    _sidecar(p, [
        Source(url="https://a.example/financials", fetched_at="t", hash="h1", extract="x", verified=True),
        Source(url="https://b.example/cadence", fetched_at="t", hash="h2", extract="y", verified=True),
    ])


def test_green_passes_all_three(tmp_path: Path) -> None:
    brief = _write(tmp_path / "example-agency.md", GREEN_BRIEF)
    _green_sidecar(tmp_path / "example-agency.sources.jsonl")
    golden = _write(tmp_path / "golden.md", GOLDEN)
    res = rg.grade(brief, golden)
    assert res.green, res.summary()
    assert res.uncited_claims == 0
    assert res.dead_urls == 0
    assert res.sections_uncovered == []


def test_uncited_claim_reds_a(tmp_path: Path) -> None:
    bad = GREEN_BRIEF.replace("- Inspections happen on a fixed cadence. [S2]",
                              "- Inspections happen on a fixed cadence.")  # no citation
    brief = _write(tmp_path / "example-agency.md", bad)
    _green_sidecar(tmp_path / "example-agency.sources.jsonl")
    golden = _write(tmp_path / "golden.md", GOLDEN)
    res = rg.grade(brief, golden)
    assert not res.green
    assert res.uncited_claims >= 1


def test_claim_citing_missing_row_reds_a(tmp_path: Path) -> None:
    """A [Sn] that resolves to a URL absent from the sidecar is an uncited claim
    (AC1: every claim links to a real sources.jsonl row)."""
    brief = _write(tmp_path / "example-agency.md", GREEN_BRIEF)
    # sidecar has only S1's url, not S2's
    _sidecar(tmp_path / "example-agency.sources.jsonl", [
        Source(url="https://a.example/financials", fetched_at="t", hash="h1", extract="x", verified=True),
    ])
    golden = _write(tmp_path / "golden.md", GOLDEN)
    res = rg.grade(brief, golden)
    assert not res.green
    assert res.uncited_claims >= 1


def test_dead_url_reds_b(tmp_path: Path) -> None:
    brief = _write(tmp_path / "example-agency.md", GREEN_BRIEF)
    _sidecar(tmp_path / "example-agency.sources.jsonl", [
        Source(url="https://a.example/financials", fetched_at="t", hash="h1", extract="x", verified=True),
        Source(url="https://b.example/cadence", fetched_at="t", hash="", extract="", verified=False, reason="http 404"),
    ])
    golden = _write(tmp_path / "golden.md", GOLDEN)
    res = rg.grade(brief, golden)
    assert not res.green
    assert res.dead_urls >= 1


def test_wayback_row_not_dead_b(tmp_path: Path) -> None:
    """A web.archive.org URL counts as resolvable even when verified=false."""
    brief = _write(tmp_path / "example-agency.md", GREEN_BRIEF.replace(
        "[S2]: https://b.example/cadence",
        "[S2]: https://web.archive.org/web/2026/https://b.example/cadence"))
    _sidecar(tmp_path / "example-agency.sources.jsonl", [
        Source(url="https://a.example/financials", fetched_at="t", hash="h1", extract="x", verified=True),
        Source(url="https://web.archive.org/web/2026/https://b.example/cadence",
               fetched_at="t", hash="", extract="", verified=False, reason="archived"),
    ])
    golden = _write(tmp_path / "golden.md", GOLDEN)
    res = rg.grade(brief, golden)
    assert res.dead_urls == 0


def test_uncovered_section_reds_c(tmp_path: Path) -> None:
    """A brief section covering no golden checklist item reds (c)."""
    brief_text = GREEN_BRIEF.replace("## Inspection cadence", "## Unrelated tangent")
    brief = _write(tmp_path / "example-agency.md", brief_text)
    _green_sidecar(tmp_path / "example-agency.sources.jsonl")
    golden = _write(tmp_path / "golden.md", GOLDEN)
    res = rg.grade(brief, golden)
    assert not res.green
    assert "Unrelated tangent" in res.sections_uncovered


def test_no_sources_brief_reds_c_not_error(tmp_path: Path) -> None:
    """AC3: a no-sources brief grades red on (c), never errors."""
    brief_text = """---
topic: "example agency"
slug: example-agency
stopped: declared
sources: example-agency.sources.jsonl
note: no sources found
---

# example agency

No sources found for this topic.
"""
    brief = _write(tmp_path / "example-agency.md", brief_text)
    (tmp_path / "example-agency.sources.jsonl").touch()  # empty
    golden = _write(tmp_path / "golden.md", GOLDEN)
    res = rg.grade(brief, golden)
    assert not res.green
    assert res.uncited_claims == 0  # no claims -> (a) vacuously clean
    assert res.dead_urls == 0       # empty store -> (b) clean
    assert res.sections_uncovered  # (c) reds


def test_missing_brief_errors(tmp_path: Path) -> None:
    golden = _write(tmp_path / "golden.md", GOLDEN)
    with pytest.raises(rg.GradeError):
        rg.grade(tmp_path / "nope.md", golden)


def test_empty_golden_checklist_errors(tmp_path: Path) -> None:
    """A golden doc with no headings is a setup error (exit 2), not a red brief."""
    brief = _write(tmp_path / "example-agency.md", GREEN_BRIEF)
    _green_sidecar(tmp_path / "example-agency.sources.jsonl")
    golden = _write(tmp_path / "golden.md", "just prose, no headings at all\n")
    with pytest.raises(rg.GradeError):
        rg.grade(brief, golden)


# --------------------------------------------------------------------------- #
# `fno evals grade` CLI exit codes
# --------------------------------------------------------------------------- #


def test_cli_grade_exit_codes(tmp_path: Path) -> None:
    from typer.testing import CliRunner

    from fno.evals.cli import evals_app

    golden = _write(tmp_path / "golden.md", GOLDEN)
    runner = CliRunner()

    # GREEN -> 0
    brief = _write(tmp_path / "example-agency.md", GREEN_BRIEF)
    _green_sidecar(tmp_path / "example-agency.sources.jsonl")
    r0 = runner.invoke(evals_app, ["grade", "--brief", str(brief), "--golden", str(golden)])
    assert r0.exit_code == 0, r0.output
    assert "GREEN" in r0.output

    # RED -> 1 (drop a citation)
    bad = _write(tmp_path / "bad.md", GREEN_BRIEF.replace(
        "sources: example-agency.sources.jsonl", "sources: example-agency.sources.jsonl").replace(
        "- The agency reports annual revenue figures. [S1]",
        "- The agency reports annual revenue figures."))
    r1 = runner.invoke(evals_app, [
        "grade", "--brief", str(bad), "--golden", str(golden),
        "--sidecar", str(tmp_path / "example-agency.sources.jsonl")])
    assert r1.exit_code == 1, r1.output
    assert "RED" in r1.output

    # ERROR -> 2 (missing brief)
    r2 = runner.invoke(evals_app, ["grade", "--brief", str(tmp_path / "nope.md"), "--golden", str(golden)])
    assert r2.exit_code == 2, r2.output
