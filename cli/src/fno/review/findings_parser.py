"""Shared strict-JSON findings parser for the review panel (ab-6c8f4c61).

Both the claude runner (gated, behind the cross-model opt-in) and the
codex/gemini ``agents_spawn_runner`` converge on this one parser so the
scorer and report-builder stay provider-agnostic (Locked Decision 5).

Contract: the agent reply is a JSON array of finding objects, each shaped like
the :class:`~fno.review.orchestrator.Finding` dataclass::

    [{"severity": "high", "message": "missing null check",
      "file": "src/foo.py", "line": 42}, ...]

An empty array (``[]``) is a valid clean review (zero findings). Anything that
is not a JSON array of well-formed finding objects raises
:class:`FindingsParseError`, which the runners convert to a soft per-agent
failure (recorded + reported, never a panel abort).
"""
from __future__ import annotations

import json

from fno.review.orchestrator import Finding

_RAW_HEAD_LEN = 200

# Prompt addendum that makes an agent emit findings in the strict JSON contract
# this parser expects, AND forbids interactive/clarifying questions (headless
# determinism, Domain Pitfall). Appended to an agent's prompt at dispatch time
# ONLY when cross-model is engaged - the 6 bundled prompt files are never
# modified, so the legacy all-claude `::finding::` path stays byte-for-byte
# unchanged when cross-model is OFF (Locked Decision 7 / the Execution-Strategy
# reconciliation). Both runners (claude when gated ON, agents_spawn always)
# append this so they converge on `parse_findings_json`.
JSON_FINDINGS_CONTRACT = """

---
OUTPUT CONTRACT (STRICT - cross-model review panel):
Respond with ONE JSON array and NOTHING else. No prose, no explanation, no
markdown code fence, no clarifying or interactive questions - you are running
headless and any non-JSON output is discarded.

Each array element is a finding object:
  {"severity": "critical|high|medium|low|info",
   "message": "<one-line description>",
   "file": "<path or omit>",
   "line": <integer or omit>}

If you found no issues, respond with exactly: []
"""


def json_findings_prompt(prompt: str) -> str:
    """Return ``prompt`` with the strict-JSON findings contract appended.

    Truly idempotent: a prompt that already carries the contract is returned
    unchanged, so a double call cannot append it twice (gemini review). The 6
    bundled prompt files are left untouched; this runtime append is the GATED
    amendment that engages only on a cross-model run.
    """
    if JSON_FINDINGS_CONTRACT in prompt:
        return prompt
    return f"{prompt.rstrip()}{JSON_FINDINGS_CONTRACT}"

# Recognized severities (mirrors orchestrator.Finding). An unrecognized
# severity is kept verbatim (lowercased) rather than rejected - the report
# counts criticals/highs by exact string, so a typo'd severity simply won't
# escalate the verdict, which is the safe direction.
_KNOWN_SEVERITIES = frozenset(
    {"critical", "high", "medium", "low", "info"}
)


# Shared WorkerOutcome.error prefix for a terminal parse failure (the provider
# replied, just not in the JSON contract). Both runners use this so the report
# renders one consistent "agent errored (unparseable findings)" note (AC3-ERR),
# and the selector treats it as terminal (never retried on claude).
PARSE_FAILURE_PREFIX = "findings-parse-failed"


class FindingsParseError(ValueError):
    """Raised when an agent reply does not honor the JSON findings contract.

    Carries the offending ``agent`` and a truncated ``raw_head`` of the reply
    so the runner can surface a useful soft-failure note in the report.
    """

    def __init__(self, agent: str, message: str, *, raw_head: str = "") -> None:
        super().__init__(message)
        self.agent = agent
        self.raw_head = raw_head


def _strip_code_fence(text: str) -> str:
    """Strip a single leading ```json / ``` fence (and its closing ```).

    Models frequently wrap a JSON array in a markdown code fence even when told
    not to. Stripping a *single* well-formed fence is lenient enough to accept
    that common shape while still rejecting free-form prose (which fails the
    JSON parse below and soft-fails the agent per AC3-ERR).
    """
    s = text.strip()
    if not s.startswith("```"):
        return s
    lines = s.splitlines()
    # Drop the opening fence line (``` or ```json or ```JSON).
    lines = lines[1:]
    # Drop a trailing closing fence if present.
    if lines and lines[-1].strip() == "```":
        lines = lines[:-1]
    return "\n".join(lines).strip()


def _coerce_line(value: object) -> int | None:
    """Best-effort int coercion for a finding's line number; None on failure."""
    if isinstance(value, bool):  # bool is an int subclass; not a line number
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        s = value.strip()
        if s.lstrip("-").isdigit():
            return int(s)
    return None


def parse_findings_json(agent: str, reply_text: str) -> list[Finding]:
    """Parse ``reply_text`` (a strict JSON findings array) into ``Finding``s.

    Args:
        agent: the agent name; stamped on every parsed Finding and carried on
            any :class:`FindingsParseError`.
        reply_text: the agent's reply. Expected to be a JSON array (optionally
            wrapped in one markdown code fence).

    Returns:
        A list of :class:`Finding` (possibly empty for a clean ``[]`` reply).

    Raises:
        FindingsParseError: the reply is not valid JSON, the JSON root is not a
            list, or any item is not an object carrying string
            ``severity`` + ``message``.
    """
    raw_head = (reply_text or "")[:_RAW_HEAD_LEN]
    text = _strip_code_fence(reply_text or "")
    if not text:
        raise FindingsParseError(agent, "empty reply", raw_head=raw_head)

    try:
        data = json.loads(text)
    except (json.JSONDecodeError, TypeError) as exc:
        raise FindingsParseError(
            agent, f"reply is not valid JSON: {exc}", raw_head=raw_head
        ) from exc

    if not isinstance(data, list):
        raise FindingsParseError(
            agent,
            f"expected a JSON array of findings, got {type(data).__name__}",
            raw_head=raw_head,
        )

    findings: list[Finding] = []
    for i, item in enumerate(data):
        if not isinstance(item, dict):
            raise FindingsParseError(
                agent, f"finding #{i} is not an object", raw_head=raw_head
            )
        severity = item.get("severity")
        message = item.get("message")
        if not isinstance(severity, str) or not severity.strip():
            raise FindingsParseError(
                agent,
                f"finding #{i} missing a string 'severity'",
                raw_head=raw_head,
            )
        if not isinstance(message, str) or not message.strip():
            raise FindingsParseError(
                agent,
                f"finding #{i} missing a string 'message'",
                raw_head=raw_head,
            )
        file_val = item.get("file")
        findings.append(
            Finding(
                agent=agent,
                severity=severity.strip().lower(),
                message=message.strip(),
                file=file_val if isinstance(file_val, str) and file_val else None,
                line=_coerce_line(item.get("line")),
                raw=json.dumps(item, ensure_ascii=False, sort_keys=True),
            )
        )
    return findings
