"""The evidence a ruling body must carry: claim detector, citations, reads.

The load-bearing test is AC7-EDGE: a false positive on ordinary prose is the
failure mode that gets this gate disabled, so the no-claim case is asserted
as positively as the claim cases.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.decide.evidence import (
    UnmeasuredClaimError,
    UnresolvableCitationError,
    check_citations,
    check_ruling_evidence,
    find_code_claims,
    run_reads,
)


def _repo(tmp_path: Path, files: dict[str, int]) -> Path:
    """A root where each named file exists with that many lines."""
    for name, lines in files.items():
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(f"line {i}" for i in range(1, lines + 1)) + "\n")
    return tmp_path


# ── citations ─────────────────────────────────────────────────────────────────


def test_clean_citation_returns_no_failures(tmp_path: Path):
    """AC1-HP: a citation the repo agrees with is positive, not absence."""
    root = _repo(tmp_path, {"cli/src/fno/law.py": 100})
    assert check_citations(
        "see cli/src/fno/law.py:57 for the classifier", root=root
    ) == []


def test_line_past_eof_fails_naming_the_real_length(tmp_path: Path):
    """AC2-ERR."""
    root = _repo(tmp_path, {"cli/src/fno/law.py": 100})
    failures = check_citations("cli/src/fno/law.py:99999", root=root)
    assert len(failures) == 1
    assert "100 lines" in failures[0]
    assert "99999" in failures[0]


def test_ambiguous_basename_fails_naming_the_count_and_remedy(tmp_path: Path):
    """AC3-EDGE: `cli.py` alone resolves to more than one tracked file."""
    root = _repo(tmp_path, {"a/cli.py": 50, "b/cli.py": 50})
    failures = check_citations("cli.py:41", root=root)
    assert len(failures) == 1
    assert "2 tracked files" in failures[0]
    assert "repo-relative" in failures[0]


def test_unique_basename_resolves(tmp_path: Path):
    """The bare-basename form is legitimate when the repo has exactly one."""
    root = _repo(tmp_path, {"cli/src/fno/backlog/advance.py": 200})
    assert check_citations("advance.py:167", root=root) == []


def test_untracked_path_fails(tmp_path: Path):
    """AC4-ERR."""
    root = _repo(tmp_path, {"real.py": 10})
    failures = check_citations("nosuchfile.py:1", root=root)
    assert len(failures) == 1
    assert "no tracked file" in failures[0]


# ── counted code facts ────────────────────────────────────────────────────────


def test_counted_claim_is_found():
    """AC5-HP."""
    assert find_code_claims("the drain loop is 167 lines") == ["167 lines"]


@pytest.mark.parametrize(
    ("text", "claim"),
    [("it has no callers", "no callers"), ("zero files touched", "zero files")],
)
def test_negative_claim_is_a_code_fact(text: str, claim: str):
    """AC6-EDGE: specimen 1 and two night specimens were negative claims."""
    assert find_code_claims(text) == [claim]


def test_ordinary_prose_is_not_a_claim(tmp_path: Path):
    """AC7-EDGE, the load-bearing one: prose naming a bare filename with no
    line number and no counted noun must trip NOTHING."""
    root = _repo(tmp_path, {"advance.py": 10})
    text = "advance.py is in backlog; read the plan before touching it"
    assert find_code_claims(text) == []
    assert check_citations(text, root=root) == []


# ── the read runner ───────────────────────────────────────────────────────────


def test_read_runs_and_carries_exit_and_output(tmp_path: Path):
    """AC8-HP."""
    rows = run_reads(["echo hi"], root=tmp_path)
    assert len(rows) == 1
    assert rows[0]["exit"] == 0
    assert rows[0]["out_head"] == "hi"
    assert rows[0]["cmd"] == "echo hi"


def test_single_zero_read_refuses_under_the_positive_marker_pitfall(tmp_path: Path):
    """AC9-EDGE, first half: an all-zero answer set is refused, naming the
    control requirement (AGENTS.md: assert a positive marker)."""
    with pytest.raises(UnmeasuredClaimError, match="control"):
        run_reads(["true"], root=tmp_path)  # exits 0, no output


def test_zero_beside_a_producing_read_is_a_measurement(tmp_path: Path):
    """AC9-EDGE, second half: the zero + its control records both rows."""
    rows = run_reads(["true", "echo hi"], root=tmp_path)
    assert [row["cmd"] for row in rows] == ["true", "echo hi"]


def test_grep_no_match_counts_as_a_zero(tmp_path: Path):
    """exit 1 with empty stdout is grep's no-match, the common zero shape."""

    def no_match(cmd, *, cwd, timeout):
        return subprocess.CompletedProcess(cmd, 1, "", "")

    with pytest.raises(UnmeasuredClaimError, match="control"):
        run_reads(["grep -c needle haystack.txt"], root=tmp_path, run=no_match)


def test_timeout_refuses_and_names_the_command(tmp_path: Path):
    """AC10-ERR: a read that cannot run stores no row."""

    def slow(cmd, *, cwd, timeout):
        raise subprocess.TimeoutExpired(cmd, timeout)

    with pytest.raises(UnmeasuredClaimError, match="sleep 999"):
        run_reads(["sleep 999"], root=tmp_path, timeout=20, run=slow)


def test_command_not_found_refuses_instead_of_storing_a_row(tmp_path: Path):
    """exit 127 is a broken read, not a measurement: it refuses like a timeout."""

    def not_found(cmd, *, cwd, timeout):
        return subprocess.CompletedProcess(cmd, 127, "", "")

    with pytest.raises(UnmeasuredClaimError, match="did not run"):
        run_reads(["nosuchcmd -x"], root=tmp_path, run=not_found)


def test_cap_on_read_count(tmp_path: Path):
    with pytest.raises(UnmeasuredClaimError, match="cap is 5"):
        run_reads(["echo 1"] * 6, root=tmp_path)


# ── the ruling-lane gate ──────────────────────────────────────────────────────


def test_gate_refuses_a_claim_with_no_read(tmp_path: Path):
    root = _repo(tmp_path, {"advance.py": 200})
    with pytest.raises(UnmeasuredClaimError, match="advance.py:167"):
        check_ruling_evidence(
            "advance.py:167 is the territory resolver",
            "port it to Rust",
            None,
            root=root,
        )


def test_gate_refuses_a_contradicted_citation_even_with_a_read(tmp_path: Path):
    """AC13 at the library tier: the read does not save a false citation."""
    root = _repo(tmp_path, {"advance.py": 200})
    with pytest.raises(UnresolvableCitationError, match="99999"):
        check_ruling_evidence(
            "advance.py:99999 is the territory resolver",
            None,
            ["echo hi"],
            root=root,
        )


def test_gate_stores_nothing_when_the_body_has_no_claim(tmp_path: Path):
    root = _repo(tmp_path, {"advance.py": 200})
    assert (
        check_ruling_evidence("Merges belong to the operator", "why", None, root=root)
        is None
    )


def test_gate_runs_attached_reads_and_returns_rows(tmp_path: Path):
    root = _repo(tmp_path, {"advance.py": 200})
    rows = check_ruling_evidence(
        "advance.py is 200 lines",
        None,
        ["head -3 advance.py"],
        root=root,
    )
    assert rows is not None
    assert rows[0]["exit"] == 0
