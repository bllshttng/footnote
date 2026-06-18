"""`claude -p` subprocess-backed confidence scorer.

Shells out to the Claude Code CLI (``claude -p --output-format json``)
so the review pipeline reuses the CLI's OAuth credentials.

Two entry points:

- ``claude_scorer(finding)``: score a single finding. Returns 0 on any
  subprocess, parse, or timeout failure.
- ``claude_scorer_batch(findings)``: score N findings in a single
  subprocess call. Tagged with ``__batch__ = True`` so
  ``confidence_scorer.score_findings`` detects it and takes the
  one-shot path. Falls back to per-finding calls if the model returns
  the wrong array length.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from fno.review.orchestrator import Finding

_MODEL = "claude-haiku-4-5"

_SINGLE_SYSTEM_PROMPT = (
    "Rate the confidence of this code review finding from 0 to 100. "
    "Reply with only the integer, nothing else."
)


def _batch_system_prompt(n: int) -> str:
    return (
        f"Rate each of the {n} code review findings below from 0 (likely false positive) "
        f"to 100 (definitely a real issue). Reply with ONLY a JSON array of {n} integers "
        "in the same order as the input, no other text."
    )


def _format_finding(finding: "Finding") -> str:
    return (
        f"Agent: {finding.agent}\n"
        f"File: {finding.file}:{finding.line}\n"
        f"Severity: {finding.severity}\n"
        f"Finding: {finding.message}"
    )


def _parse_outer(stdout: str) -> str | None:
    """Pull the model's raw text out of ``claude -p --output-format json``.

    Returns None on parse failure. The envelope schema is:
        {"type": "result", "result": "<model output>", ...}
    """
    try:
        payload = json.loads(stdout)
    except (json.JSONDecodeError, TypeError):
        return None
    result = payload.get("result")
    if not isinstance(result, str):
        return None
    return result.strip()


def _strip_fences(text: str) -> str:
    """Models sometimes wrap output in ```json ... ```. Strip for parsing."""
    stripped = text.strip()
    if not stripped.startswith("```"):
        return stripped
    body = stripped.strip("`")
    # ```json\n...\n``` -> possible leading "json"
    if body.lower().startswith("json"):
        body = body[4:]
    return body.strip()


def _coerce_int(text: str) -> int | None:
    """Extract the first 0-100 integer from ``text``. Returns None on failure."""
    match = re.search(r"-?\d+", text)
    if not match:
        return None
    try:
        value = int(match.group(0))
    except ValueError:
        return None
    return max(0, min(100, value))


def claude_scorer(finding: "Finding", *, timeout: int = 60) -> int:
    """Score a single finding 0-100 via ``claude -p``.

    Returns 0 on any failure (subprocess error, missing binary, non-numeric
    output, timeout).
    """
    cmd = [
        "claude",
        "-p",
        "--model",
        _MODEL,
        "--output-format",
        "json",
        "--append-system-prompt",
        _SINGLE_SYSTEM_PROMPT,
        _format_finding(finding),
    ]
    ctx = f"{finding.agent}:{finding.file}:{finding.line}"

    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        print(f"claude_scorer: timeout for {ctx}", file=sys.stderr)
        return 0
    except FileNotFoundError:
        print(f"claude_scorer: `claude` binary not on PATH for {ctx}", file=sys.stderr)
        return 0
    except OSError as exc:
        print(f"claude_scorer: subprocess error for {ctx}: {exc}", file=sys.stderr)
        return 0

    if result.returncode != 0:
        print(
            f"claude_scorer: non-zero exit ({result.returncode}) for {ctx}: "
            f"{(result.stderr or '').strip()[:200]}",
            file=sys.stderr,
        )
        return 0

    raw = _parse_outer(result.stdout)
    if raw is None:
        print(
            f"claude_scorer: could not parse outer JSON for {ctx}: "
            f"{(result.stdout or '').strip()[:200]!r}",
            file=sys.stderr,
        )
        return 0

    score = _coerce_int(_strip_fences(raw))
    if score is None:
        print(
            f"claude_scorer: non-numeric response for {ctx}: {raw!r}",
            file=sys.stderr,
        )
        return 0
    return score


# After 3 consecutive zero-score fallback calls we assume a systemic
# failure (auth expired, quota exhausted, misconfiguration) rather than
# N legitimately low-confidence findings and short-circuit the remainder.
# A genuinely-zero confidence score is still dropped at the default
# threshold, so over-zeroing in a fallback path is a tolerable trade for
# bounded wall-clock cost.
_FALLBACK_ZERO_STREAK_CAP = 3


def _per_finding_fallback(findings: list["Finding"], timeout: int) -> list[int]:
    """Invoke :func:`claude_scorer` once per finding.

    Short-circuits in two cases to keep the fallback path bounded:

    1. :func:`shutil.which` stops finding ``claude`` (CLI removed from
       PATH mid-run): zero out the rest with one aggregate stderr line.
    2. The first three fallback calls all return ``0`` (systemic failure
       signal like auth expiry or quota exhaustion rather than N genuine
       low-confidence findings): zero out the rest with one aggregate log.
    """
    import shutil as _shutil

    scores: list[int] = []
    zero_streak = 0
    for i, f in enumerate(findings):
        score = claude_scorer(f, timeout=timeout)
        scores.append(score)
        zero_streak = zero_streak + 1 if score == 0 else 0

        remaining = len(findings) - len(scores)
        if remaining == 0:
            break

        # Case 1: CLI vanished mid-run.
        if _shutil.which("claude") is None:
            print(
                f"claude_scorer_batch: `claude` disappeared mid-fallback "
                f"(after finding {i}); zeroing remaining {remaining}",
                file=sys.stderr,
            )
            scores.extend([0] * remaining)
            break

        # Case 2: systemic failure (every fallback call exiting non-zero).
        if zero_streak >= _FALLBACK_ZERO_STREAK_CAP:
            print(
                f"claude_scorer_batch: {_FALLBACK_ZERO_STREAK_CAP} consecutive "
                f"zero-score fallback calls (likely systemic failure); "
                f"zeroing remaining {remaining}",
                file=sys.stderr,
            )
            scores.extend([0] * remaining)
            break

    return scores


def claude_scorer_batch(findings: list["Finding"], *, timeout: int = 120) -> list[int]:
    """Score N findings in a single ``claude -p`` call.

    Returns a list of scores in the same order. Any finding that fails to
    parse individually gets 0. If the model returns the wrong array length,
    falls back to per-finding :func:`claude_scorer` calls. If the batch
    subprocess itself errors (timeout, missing binary), returns zeros for
    every finding rather than launching N retries that would fail the same
    way.
    """
    if not findings:
        return []

    payload = json.dumps(
        [
            {
                "agent": f.agent,
                "severity": f.severity,
                "message": f.message,
                "file": f.file,
                "line": f.line,
            }
            for f in findings
        ]
    )
    cmd = [
        "claude",
        "-p",
        "--model",
        _MODEL,
        "--output-format",
        "json",
        "--append-system-prompt",
        _batch_system_prompt(len(findings)),
        payload,
    ]

    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        print(
            f"claude_scorer_batch: timeout after {timeout}s "
            f"(N={len(findings)}); returning zeros",
            file=sys.stderr,
        )
        return [0] * len(findings)
    except FileNotFoundError:
        print(
            "claude_scorer_batch: `claude` binary not on PATH; returning zeros",
            file=sys.stderr,
        )
        return [0] * len(findings)
    except OSError as exc:
        import errno

        if exc.errno == errno.E2BIG:
            # Payload exceeded ARG_MAX. Per-finding calls send one finding
            # each as a CLI arg, so they're far less likely to hit the cap.
            print(
                f"claude_scorer_batch: payload too large (ARG_MAX, N={len(findings)}); "
                f"falling back to per-finding scoring",
                file=sys.stderr,
            )
            return _per_finding_fallback(findings, timeout=timeout)
        print(f"claude_scorer_batch: subprocess error: {exc}", file=sys.stderr)
        return [0] * len(findings)

    if result.returncode != 0:
        print(
            f"claude_scorer_batch: non-zero exit ({result.returncode}) "
            f"(N={len(findings)}): {(result.stderr or '').strip()[:200]}; "
            "falling back to per-finding scoring",
            file=sys.stderr,
        )
        return _per_finding_fallback(findings, timeout=timeout)

    raw = _parse_outer(result.stdout)
    if raw is None:
        print(
            f"claude_scorer_batch: could not parse outer JSON "
            f"(N={len(findings)}); falling back to per-finding scoring",
            file=sys.stderr,
        )
        return _per_finding_fallback(findings, timeout=timeout)

    try:
        scores = json.loads(_strip_fences(raw))
    except (json.JSONDecodeError, TypeError):
        print(
            f"claude_scorer_batch: inner JSON parse failed; "
            f"falling back to per-finding scoring",
            file=sys.stderr,
        )
        return _per_finding_fallback(findings, timeout=timeout)

    if not isinstance(scores, list) or len(scores) != len(findings):
        got = len(scores) if isinstance(scores, list) else "not-a-list"
        print(
            f"claude_scorer_batch: length mismatch (got {got}, expected "
            f"{len(findings)}); falling back to per-finding scoring",
            file=sys.stderr,
        )
        return _per_finding_fallback(findings, timeout=timeout)

    coerced: list[int] = []
    malformed: list[tuple[int, object]] = []
    for i, raw_score in enumerate(scores):
        # bool is a subclass of int; reject before the numeric branch so
        # True/False don't silently score as 1/0.
        if isinstance(raw_score, bool):
            coerced.append(0)
            malformed.append((i, raw_score))
        elif isinstance(raw_score, (int, float)):
            coerced.append(max(0, min(100, int(raw_score))))
        else:
            coerced.append(0)
            malformed.append((i, raw_score))

    if malformed:
        print(
            f"claude_scorer_batch: coerced {len(malformed)}/{len(scores)} "
            f"non-numeric entries to 0: "
            f"{[(i, repr(v)) for i, v in malformed[:5]]}"
            + ("..." if len(malformed) > 5 else ""),
            file=sys.stderr,
        )
    return coerced


# Marker: ``score_findings`` detects this attribute to take the one-shot
# batch path instead of the per-finding loop.
claude_scorer_batch.__batch__ = True  # type: ignore[attr-defined]
