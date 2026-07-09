"""Tests for `claude -p` subprocess-backed confidence scorer.

Replaces test_confidence_scorer_haiku.py. The scorer now shells out to
``claude -p --output-format json`` instead of calling the anthropic SDK,
so tests mock ``subprocess.run`` and ``shutil.which`` rather than an
anthropic client.

Covers the ACs in
internal/fno/plans/2026-04-22-cli-review-claude-scorer.md:
  - AC1-HP / AC1-ERR / AC1-EDGE (single + batch scorer primitives)
  - AC2-HP / AC2-UI (_resolve_default_scorer with/without claude on PATH)
"""

from __future__ import annotations

import json
import os
import subprocess
from unittest.mock import MagicMock, patch

import pytest

from fno.review.orchestrator import Finding


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_finding(msg: str = "test finding", *, agent: str = "code_reviewer") -> Finding:
    return Finding(agent=agent, severity="high", message=msg, file="src/foo.py", line=10)


def _cp(stdout: str, returncode: int = 0, stderr: str = "") -> subprocess.CompletedProcess:
    """Build a CompletedProcess stub that mimics `claude -p --output-format json`."""
    return subprocess.CompletedProcess(
        args=["claude", "-p"], returncode=returncode, stdout=stdout, stderr=stderr
    )


def _json_result(text: str) -> str:
    """Wrap a model response string in the JSON envelope `claude -p` returns."""
    return json.dumps({"type": "result", "subtype": "success", "is_error": False, "result": text})


# ---------------------------------------------------------------------------
# AC1-HP: scorer prompts carry the rubric and the abstain path
# ---------------------------------------------------------------------------

_RUBRIC_ANCHORS = ("0 =", "25 =", "50 =", "75 =", "100 =")


class TestScorerPromptRubric:
    """AC1-HP: both system prompts embed the 0/25/50/75/100 rubric and the
    'reply 25 or lower when unverifiable / never guess high' abstain path,
    while keeping the reply-format sentence last."""

    def test_single_prompt_carries_rubric_and_abstain(self) -> None:
        from fno.review.scorers.claude_scorer import _SINGLE_SYSTEM_PROMPT as p

        for anchor in _RUBRIC_ANCHORS:
            assert anchor in p, f"missing rubric anchor {anchor!r}"
        assert "25 or lower" in p.lower()
        assert "never guess high" in p.lower()
        # Format instruction stays last (trailing instructions dominate Haiku).
        assert p.rstrip().endswith("Reply with only the integer, nothing else.")

    def test_batch_prompt_carries_rubric_and_abstain(self) -> None:
        from fno.review.scorers.claude_scorer import _batch_system_prompt

        p = _batch_system_prompt(4)
        for anchor in _RUBRIC_ANCHORS:
            assert anchor in p, f"missing rubric anchor {anchor!r}"
        assert "25 or lower" in p.lower()
        assert "never guess high" in p.lower()
        # Output contract intact: array-of-N-integers instruction, last.
        assert "JSON array of 4 integers" in p
        assert p.rstrip().endswith("no other text.")


# ---------------------------------------------------------------------------
# AC1-HP / AC1-ERR: single-finding claude_scorer
# ---------------------------------------------------------------------------

class TestClaudeScorerSingle:
    """AC1-HP + AC1-ERR: claude_scorer returns integer on success, 0 on failure."""

    def test_happy_path_returns_integer(self) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", return_value=_cp(_json_result("92"))):
            score = claude_scorer(_make_finding())

        assert score == 92

    def test_score_zero_and_100_boundaries(self) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", return_value=_cp(_json_result("0"))):
            assert claude_scorer(_make_finding()) == 0
        with patch("subprocess.run", return_value=_cp(_json_result("100"))):
            assert claude_scorer(_make_finding()) == 100

    def test_whitespace_tolerated(self) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", return_value=_cp(_json_result("  85\n"))):
            assert claude_scorer(_make_finding()) == 85

    def test_non_numeric_returns_zero(self, capsys: pytest.CaptureFixture) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", return_value=_cp(_json_result("not-a-number"))):
            assert claude_scorer(_make_finding()) == 0

        captured = capsys.readouterr()
        # Error logged with file:line context for debuggability.
        assert "code_reviewer" in captured.err
        assert "src/foo.py" in captured.err

    def test_non_zero_exit_returns_zero(self, capsys: pytest.CaptureFixture) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", return_value=_cp("", returncode=1, stderr="auth failed")):
            assert claude_scorer(_make_finding()) == 0
        assert capsys.readouterr().err  # something logged

    def test_invalid_outer_json_returns_zero(self, capsys: pytest.CaptureFixture) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", return_value=_cp("not json at all")):
            assert claude_scorer(_make_finding()) == 0
        assert capsys.readouterr().err

    def test_timeout_returns_zero(self, capsys: pytest.CaptureFixture) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer

        def raise_timeout(*args, **kwargs):
            raise subprocess.TimeoutExpired(cmd=["claude", "-p"], timeout=60)

        with patch("subprocess.run", side_effect=raise_timeout):
            assert claude_scorer(_make_finding()) == 0
        assert capsys.readouterr().err

    def test_filenotfound_returns_zero(self, capsys: pytest.CaptureFixture) -> None:
        """If the `claude` binary is missing mid-call, score 0 instead of raising."""
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", side_effect=FileNotFoundError("claude")):
            assert claude_scorer(_make_finding()) == 0
        assert capsys.readouterr().err

    def test_invokes_claude_p_with_haiku_model(self) -> None:
        """Command must include `claude -p` and pin the Haiku model."""
        from fno.review.scorers.claude_scorer import claude_scorer

        with patch("subprocess.run", return_value=_cp(_json_result("77"))) as mock_run:
            claude_scorer(_make_finding())

        args = mock_run.call_args.args[0]
        assert args[0] == "claude"
        assert "-p" in args
        assert "--model" in args
        assert "claude-haiku-4-5" in args
        assert "--output-format" in args
        assert "json" in args


# ---------------------------------------------------------------------------
# AC1-EDGE: batched scorer
# ---------------------------------------------------------------------------

class TestClaudeScorerBatch:
    """AC1-EDGE: claude_scorer_batch returns ordered list; falls back on mismatch."""

    def test_empty_list_short_circuits(self) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        with patch("subprocess.run") as mock_run:
            assert claude_scorer_batch([]) == []
        mock_run.assert_not_called()

    def test_batch_happy_path_preserves_order(self) -> None:
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(4)]
        with patch("subprocess.run", return_value=_cp(_json_result("[90, 85, 82, 80]"))) as mock_run:
            scores = claude_scorer_batch(findings)

        assert scores == [90, 85, 82, 80]
        mock_run.assert_called_once()  # batched: one subprocess call for N findings

    def test_batch_tolerates_markdown_fences(self) -> None:
        """Models sometimes wrap JSON in ```json ... ``` fences. Strip before parsing."""
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(2)]
        fenced = "```json\n[70, 88]\n```"
        with patch("subprocess.run", return_value=_cp(_json_result(fenced))):
            assert claude_scorer_batch(findings) == [70, 88]

    def test_batch_length_mismatch_falls_back_to_per_finding(self) -> None:
        """Model returned wrong array size -> call claude_scorer once per finding."""
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(3)]

        # First call (batch) returns 2 scores for 3 findings -> triggers fallback.
        # Next 3 calls return per-finding scores.
        batch_reply = _cp(_json_result("[50, 60]"))
        per_finding_replies = [_cp(_json_result("91")), _cp(_json_result("92")), _cp(_json_result("93"))]

        # Pin shutil.which so the fallback's mid-run "claude disappeared"
        # short-circuit doesn't fire on CI runners that lack the binary.
        with (
            patch("subprocess.run", side_effect=[batch_reply, *per_finding_replies]),
            patch("shutil.which", return_value="/usr/local/bin/claude"),
        ):
            scores = claude_scorer_batch(findings)

        assert scores == [91, 92, 93]

    def test_batch_non_list_result_falls_back(self) -> None:
        """Model returned an object instead of a list -> per-finding fallback."""
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(2)]
        batch_reply = _cp(_json_result('{"error": "bad"}'))
        per_finding_replies = [_cp(_json_result("55")), _cp(_json_result("66"))]

        with (
            patch("subprocess.run", side_effect=[batch_reply, *per_finding_replies]),
            patch("shutil.which", return_value="/usr/local/bin/claude"),
        ):
            scores = claude_scorer_batch(findings)

        assert scores == [55, 66]

    def test_batch_subprocess_error_returns_zeros(self, capsys: pytest.CaptureFixture) -> None:
        """Timeout/FileNotFound on the batch call returns zeros (no per-finding retry storm)."""
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(3)]
        with patch(
            "subprocess.run",
            side_effect=subprocess.TimeoutExpired(cmd=["claude", "-p"], timeout=120),
        ):
            scores = claude_scorer_batch(findings)

        assert scores == [0, 0, 0]
        assert capsys.readouterr().err

    def test_batch_has_marker_attribute(self) -> None:
        """__batch__ marker tells score_findings to use one-shot path."""
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        assert getattr(claude_scorer_batch, "__batch__", False) is True

    def test_fallback_short_circuits_when_claude_disappears(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """If `claude` goes missing mid-fallback, remaining findings get zeros
        via one aggregate stderr log instead of N identical FileNotFoundErrors.
        """
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(5)]

        # Batch call returns wrong length -> fallback path triggers.
        batch_reply = _cp(_json_result("[50]"))
        # First fallback call succeeds, then `claude` vanishes (shutil.which
        # returns None after the first scorer call, short-circuiting).
        first_success = _cp(_json_result("77"))

        # shutil.which is only consulted inside _per_finding_fallback (after
        # each per-finding score). Returning None on every call means the
        # first scorer run sees one None check and short-circuits.
        with (
            patch("subprocess.run", side_effect=[batch_reply, first_success]),
            patch("shutil.which", return_value=None),
        ):
            scores = claude_scorer_batch(findings)

        # First fallback call succeeded with 77; remaining 4 zeroed after the
        # post-call which-check returned None.
        assert scores == [77, 0, 0, 0, 0]
        err = capsys.readouterr().err
        assert "disappeared mid-fallback" in err

    def test_fallback_short_circuits_on_consecutive_zeros(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """If the first N fallback calls all return 0, assume systemic
        failure (auth/quota) and zero out the remaining findings.
        """
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(10)]

        # Batch call: wrong length -> fallback.
        batch_reply = _cp(_json_result("[50]"))
        # Per-finding calls: all exit non-zero (systemic auth failure) -> all 0.
        systemic_fail = _cp("", returncode=1, stderr="auth expired")

        # Supply enough replies for the cap + a couple extras in case the
        # implementation ever reorders. shutil.which must NOT return None so
        # the "claude disappeared" branch doesn't trigger first.
        with (
            patch("subprocess.run", side_effect=[batch_reply] + [systemic_fail] * 10),
            patch("shutil.which", return_value="/usr/local/bin/claude"),
        ):
            scores = claude_scorer_batch(findings)

        assert scores == [0] * 10
        err = capsys.readouterr().err
        assert "systemic failure" in err or "consecutive" in err

    def test_batch_e2big_falls_back_to_per_finding(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """An OSError with errno.E2BIG (payload too large for ARG_MAX) falls
        back to per-finding calls rather than zeroing everything.
        """
        import errno

        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(3)]

        e2big = OSError(errno.E2BIG, "Argument list too long")

        # First call (batch): raises E2BIG. Next 3 calls (per-finding): succeed.
        replies = [e2big, _cp(_json_result("80")), _cp(_json_result("90")), _cp(_json_result("85"))]

        with (
            patch("subprocess.run", side_effect=replies),
            patch("shutil.which", return_value="/usr/local/bin/claude"),
        ):
            scores = claude_scorer_batch(findings)

        assert scores == [80, 90, 85]
        err = capsys.readouterr().err
        assert "ARG_MAX" in err or "too large" in err

    def test_batch_logs_malformed_entries(self, capsys: pytest.CaptureFixture) -> None:
        """Non-numeric / bool entries in the batch array log once with indices."""
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        findings = [_make_finding(f"f{i}") for i in range(4)]
        # Mix of int, bool, str, null -> three coerced to 0.
        stdout = _json_result("[90, true, \"oops\", null]")
        with patch("subprocess.run", return_value=_cp(stdout)):
            scores = claude_scorer_batch(findings)

        assert scores == [90, 0, 0, 0]
        err = capsys.readouterr().err
        assert "coerced 3/4 non-numeric entries" in err


# ---------------------------------------------------------------------------
# AC2-HP / AC2-UI: _resolve_default_scorer
# ---------------------------------------------------------------------------

class TestResolveDefaultScorer:
    """AC2-HP + AC2-UI: resolver picks batch scorer when claude is on PATH."""

    def test_claude_on_path_returns_batch_scorer(self) -> None:
        from fno.review import confidence_scorer as cs_mod
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        with patch("shutil.which", return_value="/usr/local/bin/claude"):
            cs_mod._no_claude_warned = False
            resolved = cs_mod._resolve_default_scorer()

        assert resolved is claude_scorer_batch
        assert getattr(resolved, "__batch__", False) is True

    def test_claude_missing_returns_pass_through(self) -> None:
        from fno.review import confidence_scorer as cs_mod

        with patch("shutil.which", return_value=None):
            cs_mod._no_claude_warned = False
            resolved = cs_mod._resolve_default_scorer()

        assert resolved is cs_mod.pass_through_scorer

    def test_missing_claude_warns_once(self, capsys: pytest.CaptureFixture) -> None:
        from fno.review import confidence_scorer as cs_mod

        with patch("shutil.which", return_value=None):
            cs_mod._no_claude_warned = False
            cs_mod._resolve_default_scorer()
            cs_mod._resolve_default_scorer()
            cs_mod._resolve_default_scorer()

        err = capsys.readouterr().err
        # Exactly one warning per process.
        assert err.count("claude") >= 1
        assert err.lower().count("pass-through") == 1


# ---------------------------------------------------------------------------
# AC2-FILTER: threshold filtering via score_findings
# ---------------------------------------------------------------------------

class TestScoreFindings:
    """AC2-FILTER: 10 findings with varied scores, threshold=80 keeps 4."""

    def _make_scorer_returning(self, scores: list[int]):
        score_iter = iter(scores)

        def scorer(f: Finding) -> int:
            return next(score_iter)

        return scorer

    def test_threshold_keeps_exactly_4_of_10(self) -> None:
        from fno.review.confidence_scorer import score_findings

        scores = [90, 85, 82, 80, 79, 70, 60, 50, 10, 0]
        findings = [_make_finding(f"finding {i}") for i in range(10)]
        scorer = self._make_scorer_returning(scores)

        kept = score_findings(findings, scorer=scorer, threshold=80)
        assert len(kept) == 4

    def test_kept_findings_have_confidence_populated(self) -> None:
        from fno.review.confidence_scorer import score_findings

        scores = [90, 85, 82, 80, 79, 70, 60, 50, 10, 0]
        findings = [_make_finding(f"finding {i}") for i in range(10)]
        scorer = self._make_scorer_returning(scores)

        kept = score_findings(findings, scorer=scorer, threshold=80)
        assert [f.confidence for f in kept] == [90, 85, 82, 80]

    def test_empty_findings_returns_empty(self) -> None:
        from fno.review.confidence_scorer import score_findings

        kept = score_findings([], scorer=lambda f: 100, threshold=80)
        assert kept == []

    def test_all_above_threshold_keeps_all(self) -> None:
        from fno.review.confidence_scorer import score_findings

        findings = [_make_finding("ok") for _ in range(3)]
        scorer = self._make_scorer_returning([90, 95, 85])
        kept = score_findings(findings, scorer=scorer, threshold=80)
        assert len(kept) == 3

    def test_none_above_threshold_keeps_none(self) -> None:
        from fno.review.confidence_scorer import score_findings

        findings = [_make_finding("drop") for _ in range(3)]
        scorer = self._make_scorer_returning([79, 50, 0])
        kept = score_findings(findings, scorer=scorer, threshold=80)
        assert len(kept) == 0

    def test_batch_scorer_used_one_shot(self) -> None:
        """When resolver returns a __batch__-tagged callable, one call covers all findings."""
        from fno.review.confidence_scorer import score_findings

        findings = [_make_finding(f"f{i}") for i in range(4)]
        call_counter = {"n": 0}

        def batch_scorer(fs: list[Finding]) -> list[int]:
            call_counter["n"] += 1
            return [95, 85, 50, 10]

        batch_scorer.__batch__ = True  # type: ignore[attr-defined]

        kept = score_findings(findings, scorer=batch_scorer, threshold=80)
        assert [f.confidence for f in kept] == [95, 85]
        assert call_counter["n"] == 1  # single batched invocation, not per-finding

    def test_batch_scorer_wrong_length_zeros_all(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """A user-supplied batch scorer that returns the wrong-length list
        must NOT have its scores zip'd into findings (silent misalignment).
        """
        from fno.review.confidence_scorer import score_findings

        findings = [_make_finding(f"f{i}") for i in range(4)]

        def bad_batch_scorer(fs: list[Finding]) -> list[int]:
            # Returns only 2 scores for 4 findings -> should be rejected.
            return [90, 85]

        bad_batch_scorer.__batch__ = True  # type: ignore[attr-defined]

        kept = score_findings(findings, scorer=bad_batch_scorer, threshold=80)

        # Every finding zeroed out, all below threshold -> empty kept list.
        assert kept == []
        err = capsys.readouterr().err
        assert "batch scorer returned 2 for 4" in err

    def test_batch_scorer_non_list_zeros_all(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """A batch scorer that returns a non-list (buggy user scorer) also
        triggers the guard rather than crashing on zip().
        """
        from fno.review.confidence_scorer import score_findings

        findings = [_make_finding(f"f{i}") for i in range(3)]

        def broken_batch_scorer(fs: list[Finding]) -> int:  # type: ignore[return]
            return 42  # not a list at all

        broken_batch_scorer.__batch__ = True  # type: ignore[attr-defined]

        kept = score_findings(findings, scorer=broken_batch_scorer, threshold=80)
        assert kept == []
        err = capsys.readouterr().err
        assert "batch scorer returned int for 3" in err

    def test_no_claude_uses_pass_through(self) -> None:
        """When claude binary is absent, default resolver picks pass-through."""
        from fno.review.confidence_scorer import score_findings
        import fno.review.confidence_scorer as cs_mod

        findings = [_make_finding("a"), _make_finding("b")]
        with patch("shutil.which", return_value=None):
            cs_mod._no_claude_warned = False
            kept = score_findings(findings)

        # pass-through gives every finding 100 which is above threshold 80.
        assert len(kept) == 2
        assert all(f.confidence == 100 for f in kept)


# ---------------------------------------------------------------------------
# AC2-IMMUTABLE: input findings unchanged
# ---------------------------------------------------------------------------

class TestImmutability:
    def test_input_findings_unchanged(self) -> None:
        from fno.review.confidence_scorer import score_findings

        original = Finding(
            agent="code_reviewer",
            severity="high",
            message="original",
            file="src/a.py",
            line=5,
            confidence=None,
            raw="",
        )
        original_id = id(original)

        kept = score_findings([original], scorer=lambda f: 95, threshold=80)

        assert original.confidence is None  # unchanged
        assert id(kept[0]) != original_id
        assert kept[0].confidence == 95

    def test_returned_findings_are_new_instances(self) -> None:
        from fno.review.confidence_scorer import score_findings

        findings = [
            Finding(agent="a", severity="high", message="m1"),
            Finding(agent="b", severity="low", message="m2"),
        ]
        kept = score_findings(findings, scorer=lambda f: 90, threshold=80)
        for original, returned in zip(findings, kept):
            assert original is not returned
            assert returned.confidence == 90


# ---------------------------------------------------------------------------
# AC3-NO-DEP: tests themselves do not import anthropic
# ---------------------------------------------------------------------------

class TestNoAnthropicImport:
    """AC3-NO-DEP: the new test module must not import anthropic anywhere."""

    def test_module_source_has_no_anthropic_import(self) -> None:
        import ast

        source = open(__file__).read()
        tree = ast.parse(source)
        imported_modules: set[str] = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                for alias in node.names:
                    imported_modules.add(alias.name.split(".")[0])
            elif isinstance(node, ast.ImportFrom):
                if node.module:
                    imported_modules.add(node.module.split(".")[0])

        assert "anthropic" not in imported_modules, (
            f"test module still imports anthropic (found in: {imported_modules})"
        )
