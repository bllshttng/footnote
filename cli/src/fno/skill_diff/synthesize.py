"""Diff-synthesis seam: build the prompt, dispatch a fable agent, parse the result.

The agent call is the one non-deterministic step; it is isolated here behind a
structured contract so the rest of the proposer stays testable. The parser
(:func:`parse_proposal`) is unit-tested against fixture JSON; the dispatch
(:func:`synthesize`) is the integration point exercised when the loop graduates
to ``assisted``.

The agent returns a JSON object (house return contract, fenced or bare):

    {"verdict": "propose_pr" | "no_diff_helps",
     "hunks": [{"file","old_text","new_text","cited_finding_ids","rationale"}],
     "justification": "<if additive-only>" | null,
     "no_diff_reason": "<if no_diff_helps>" | null}
"""
from __future__ import annotations

import json
import logging
import re
import subprocess
from dataclasses import dataclass, field
from typing import Optional

_LOG = logging.getLogger(__name__)

# fable = judgment-about-judgment tier.
SYNTH_MODEL = "claude-fable-5"

_DRIVER_SKILLS = {"fno:think", "fno:target", "fno:megawalk", "fno:do", "fno:blueprint"}


@dataclass
class Proposal:
    verdict: str  # propose_pr | no_diff_helps
    hunks: list[dict] = field(default_factory=list)
    justification: Optional[str] = None
    no_diff_reason: Optional[str] = None


class ProposalParseError(ValueError):
    """The synthesis output could not be parsed into a Proposal."""


def parse_proposal(text: str) -> Proposal:
    """Parse the agent's structured output, fenced ```json or bare, into a Proposal.

    Fail closed: an unparseable or out-of-enum verdict raises rather than
    silently opening an empty PR. A ``no_diff_helps`` verdict is honored even
    with hunks present (the verdict wins).
    """
    obj = _extract_json(text)
    verdict = obj.get("verdict")
    if verdict not in ("propose_pr", "no_diff_helps"):
        raise ProposalParseError(f"unknown verdict: {verdict!r}")
    hunks = obj.get("hunks") or []
    if not isinstance(hunks, list):
        raise ProposalParseError("hunks must be a list")
    norm: list[dict] = []
    for h in hunks:
        if not isinstance(h, dict) or "file" not in h:
            raise ProposalParseError(f"malformed hunk: {h!r}")
        # Fail closed on wrong field types: a non-str file crashes the git apply,
        # and a str cited_finding_ids would silently become a per-character list
        # (turning "s1" into ['s','1']). The output is untrusted LLM JSON.
        for key in ("file", "old_text", "new_text", "rationale"):
            if key in h and h[key] is not None and not isinstance(h[key], str):
                raise ProposalParseError(f"hunk.{key} must be a string, got {type(h[key]).__name__}")
        cites = h.get("cited_finding_ids") or []
        if not isinstance(cites, list) or not all(isinstance(c, str) for c in cites):
            raise ProposalParseError("hunk.cited_finding_ids must be a list of strings")
        norm.append(
            {
                "file": h["file"],
                "old_text": h.get("old_text", "") or "",
                "new_text": h.get("new_text", "") or "",
                "cited_finding_ids": list(cites),
                "rationale": h.get("rationale", "") or "",
            }
        )
    return Proposal(
        verdict=verdict,
        hunks=norm,
        justification=obj.get("justification"),
        no_diff_reason=obj.get("no_diff_reason"),
    )


def _extract_json(text: str) -> dict:
    # Prefer a fenced block's inner text, else the whole text; then take the
    # outermost { .. } by first-brace / last-brace. A lazy `\{.*?\}` regex would
    # truncate at the first nested closing brace (the hunks list is nested), so
    # brace-span extraction is used instead of a single regex.
    fence = re.search(r"```(?:json)?\s*(.*?)\s*```", text, re.DOTALL)
    region = fence.group(1) if fence else text
    start, end = region.find("{"), region.rfind("}")
    if start == -1 or end <= start:
        raise ProposalParseError("no JSON object found in synthesis output")
    candidate = region[start : end + 1]
    try:
        obj = json.loads(candidate)
    except json.JSONDecodeError as exc:
        raise ProposalParseError(f"invalid JSON: {exc}") from exc
    if not isinstance(obj, dict):
        raise ProposalParseError("synthesis output is not a JSON object")
    return obj


def build_prompt(
    *,
    skill_id: str,
    skill_files: dict[str, str],
    findings: list[dict],
    ranking: list[dict],
    history: list[dict],
    additive_threshold: int,
) -> str:
    """Compose the synthesis prompt. Bounded evidence only (each finding's
    <=500-char evidence, never a full transcript) - the earned-specificity
    discipline applied to skill edits, and the anti-bloat posture starts here."""
    driver_note = (
        "\nThis is a DRIVER skill: your diff must not introduce a self-containment "
        "CI violation - no ${REPO_ROOT}/scripts/ refs, no ../../ path escapes.\n"
        if skill_id in _DRIVER_SKILLS
        else ""
    )
    ev = "\n".join(
        f"- [{f.get('dimension')}/{f.get('verdict')}] finding_id={f.get('corpus_item_id')}: "
        f"{(f.get('evidence') or '')[:500]}"
        for f in findings
    )
    rank = ", ".join(f"{r['dimension']}={r['fail_count']}" for r in ranking) or "(none)"
    files = "\n\n".join(f"### FILE: {path}\n{body}" for path, body in skill_files.items())
    prior = (
        "\n".join(
            f"- run {d.get('run_id')}: +{d.get('added_lines')}/-{d.get('removed_lines')} "
            f"cited {d.get('cited_finding_ids')}"
            for d in history
        )
        or "(no prior proposals)"
    )
    return f"""You improve a Claude skill by editing its files in response to OBSERVED failures.

Skill: {skill_id}
Failure ranking (dimension=fail_count): {rank}
{driver_note}
Cited failure evidence (bounded, one line each):
{ev}

Prior proposals for this skill (do not re-propose what already did not help):
{prior}

Current skill file contents:
{files}

RULES:
- Prefer EDITS and DELETIONS over pure additions. A diff that only adds text is
  suspect: if you must add more than {additive_threshold} net new lines with zero
  removals, you MUST fill "justification".
- Every hunk MUST cite at least one finding_id from the evidence above. An
  uncited hunk will be dropped before the PR opens.
- old_text must match the current file content EXACTLY (it is applied by literal
  replacement). Use "" for a pure addition appended to the file.
- If no edit will help - the failure is architectural, not a wording problem -
  return verdict "no_diff_helps" with a "no_diff_reason".

Return ONLY a JSON object:
{{"verdict":"propose_pr"|"no_diff_helps","hunks":[{{"file","old_text","new_text","cited_finding_ids","rationale"}}],"justification":null,"no_diff_reason":null}}
"""


def synthesize(prompt: str, *, model: str = SYNTH_MODEL, timeout: int = 900) -> Proposal:
    """Dispatch the fable synthesis agent (headless one-shot) and parse its output.

    Integration seam: a one-shot ``claude -p`` is the right substrate for a
    synchronous tick that needs the result inline (not a pane, not a bg thread).
    Raises ProposalParseError on unparseable output, a timeout, or a spawn
    failure; the caller treats a raised error as a tool fault and takes no
    action this tick (a timeout must never crash the standing loop).
    """
    try:
        proc = subprocess.run(
            ["claude", "-p", prompt, "--model", model],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        raise ProposalParseError(f"synthesis agent timed out after {timeout}s") from exc
    except OSError as exc:
        raise ProposalParseError(f"synthesis agent spawn failed: {exc}") from exc
    if proc.returncode != 0:
        raise ProposalParseError(f"synthesis agent exited {proc.returncode}: {proc.stderr[:300]}")
    return parse_proposal(proc.stdout)


if __name__ == "__main__":  # pragma: no cover - smoke self-check
    p = parse_proposal('```json\n{"verdict":"propose_pr","hunks":[{"file":"a","new_text":"x","cited_finding_ids":["f1"]}]}\n```')
    assert p.verdict == "propose_pr" and p.hunks[0]["file"] == "a"
    p2 = parse_proposal('{"verdict":"no_diff_helps","no_diff_reason":"architectural"}')
    assert p2.verdict == "no_diff_helps" and not p2.hunks
    try:
        parse_proposal("no json here")
    except ProposalParseError:
        pass
    else:
        raise AssertionError("expected ProposalParseError")
    print("synthesize ok")
