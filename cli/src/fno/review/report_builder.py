"""Assemble the review gate artifact from orchestrator output.

Phase 03. The report builder takes an :class:`OrchestratorResult`,
applies the session nonce binding, picks a verdict consistent with
the gate's schema, and writes a markdown artifact at
``.fno/artifacts/review-{session_id}.md``.

The frontmatter contract matches
``gate_reality_map.yaml :: quality_check_passed.artifact_schema``:

- ``phase: review``
- ``session_id: <nonce>``
- ``verdict: ready-to-merge | done-with-concerns | blocked``
- ``findings_critical: <int>``
- ``findings_high: <int>``

The body contains a per-agent summary and the raw findings list so a
human reviewer can skim without tooling.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

import yaml

from fno.review.orchestrator import Finding, OrchestratorResult


def _count_by(findings: list[Finding], severity: str) -> int:
    return sum(1 for f in findings if f.severity == severity)


def _provider_tag(outcome) -> str:
    """Inline provider/model attribution for a per-agent line (ab-6c8f4c61).

    Empty when no provider is set (all-claude OFF path) so output is unchanged.
    """
    provider = getattr(outcome, "provider", None)
    if not provider:
        return ""
    model = getattr(outcome, "model", None)
    return f" [{provider}/{model}]" if model else f" [{provider}]"


def choose_verdict(
    result: OrchestratorResult,
    *,
    escalate_suspicious: bool = True,
) -> str:
    """Pick a verdict given the orchestrator's findings.

    - critical > 0 -> ``blocked``
    - high > 0 -> ``done-with-concerns``
    - workers_failed > 0 -> ``done-with-concerns`` (partial coverage)
    - all-clean + ``escalate_suspicious`` -> ``done-with-concerns``
    - otherwise -> ``ready-to-merge``
    """
    if _count_by(result.findings, "critical") > 0:
        return "blocked"
    if _count_by(result.findings, "high") > 0:
        return "done-with-concerns"
    if result.workers_failed > 0:
        return "done-with-concerns"
    if escalate_suspicious and result.suspicious:
        return "done-with-concerns"
    return "ready-to-merge"


def render_artifact_markdown(
    session_id: str,
    result: OrchestratorResult,
    verdict: str,
    *,
    pr_number: int | None = None,
) -> str:
    """Render the full artifact markdown (frontmatter + body)."""
    frontmatter = {
        "phase": "review",
        "session_id": session_id,
        "verdict": verdict,
        "findings_critical": _count_by(result.findings, "critical"),
        "findings_high": _count_by(result.findings, "high"),
        "findings_medium": _count_by(result.findings, "medium"),
        "findings_low": _count_by(result.findings, "low"),
        "workers_completed": result.workers_completed,
        "workers_failed": result.workers_failed,
        "duration_seconds": round(result.duration_seconds, 3),
        "generated_at": datetime.now(timezone.utc).isoformat(
            timespec="seconds"
        ).replace("+00:00", "Z"),
    }
    if pr_number is not None:
        frontmatter["pr_number"] = pr_number

    lines = [
        "---",
        yaml.safe_dump(frontmatter, default_flow_style=False, sort_keys=False).rstrip(),
        "---",
        "",
        f"# Review report for session `{session_id}`",
        "",
        f"**Verdict:** {verdict}",
        "",
        "## Per-agent outcomes",
        "",
    ]
    for outcome in result.outcomes:
        if outcome.ok:
            status = "OK"
        elif outcome.error and "findings-parse-failed" in outcome.error:
            # AC3-ERR: a provider replied but not in the JSON contract.
            status = "agent errored (unparseable findings)"
        else:
            status = f"FAIL ({outcome.error})"
        # Degradation / fallback note (cross-model unavailable, fell back, etc.)
        # rides on a successful or failed outcome; empty on the all-claude path.
        note = getattr(outcome, "note", None)
        note_suffix = f" - {note}" if note else ""
        lines.append(
            f"- **{outcome.agent}**{_provider_tag(outcome)} - {status}, "
            f"{len(outcome.findings)} finding(s), {outcome.duration_seconds:.2f}s"
            f"{note_suffix}"
        )

    # Cross-model cost / coverage line (OQ2). Only when a run was attributed to
    # any provider (cross-model engaged); the all-claude OFF path adds nothing.
    used = sorted({p for o in result.outcomes if (p := getattr(o, "provider", None))})
    if used:
        non_claude = [p for p in used if p != "claude"]
        if non_claude:
            lines += [
                "",
                f"_Cross-model coverage: {', '.join(used)}. Billed a second "
                f"provider's quota this run: {', '.join(non_claude)}._",
            ]
        else:
            lines += [
                "",
                "_Cross-model coverage: claude only (no differing provider "
                "available; ran all-claude)._",
            ]

    if result.findings:
        lines += ["", "## Findings", ""]
        for f in result.findings:
            loc = ""
            if f.file:
                loc = f" ({f.file}{':' + str(f.line) if f.line else ''})"
            conf = f" [confidence {f.confidence}]" if f.confidence is not None else ""
            lines.append(f"- **{f.severity.upper()}** - {f.agent}{loc}{conf}: {f.message}")

    if result.suspicious:
        lines += [
            "",
            "## Suspicious-clean note",
            "",
            "All 6 workers returned zero findings. Treat this outcome with skepticism - "
            "reviewers have been known to surface zero findings when prompts are truncated "
            "or the diff is empty.",
        ]

    return "\n".join(lines) + "\n"


def write_artifact(
    session_id: str,
    result: OrchestratorResult,
    *,
    artifacts_dir: Path | None = None,
    pr_number: int | None = None,
    escalate_suspicious: bool = True,
) -> tuple[Path, str]:
    """Build a verdict, write the artifact to disk, return ``(path, verdict)``."""
    verdict = choose_verdict(result, escalate_suspicious=escalate_suspicious)
    resolved_dir = artifacts_dir or Path(".fno/artifacts")
    resolved_dir.mkdir(parents=True, exist_ok=True)
    path = resolved_dir / f"review-{session_id}.md"
    path.write_text(
        render_artifact_markdown(session_id, result, verdict, pr_number=pr_number),
        encoding="utf-8",
    )
    return path, verdict
