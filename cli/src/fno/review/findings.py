"""The finding classifier: the one place the blocking question is answered.

Three producers see three payload shapes (a ``ReportFindings`` tool call, a
fenced JSON block in a subagent's final text, a Codex ``review_output``
object). This module normalizes all three into one record vocabulary and
answers one question per record: BLOCKING or not.

Fail closed everywhere. An unreadable record, a missing required field, a
``CONFIRMED`` verdict, or an unrecognized category all block. The allowlist
names the harmless categories and never the blocking ones, so a category the
rule does not know is blocking, never silently harmless.

What the classifier does NOT claim: the author writes the category field on
the self-review lane, so a real defect declassified as ``style`` stays
declassified unless its ``verdict`` is ``CONFIRMED``. The improvement over a
clean-only gate is auditability, not incentive compatibility. The full
honest-limits statement ships in ``docs/architecture/review-coverage-termination.md``.
"""
from __future__ import annotations

import json
import re
import sys
from collections import Counter
from dataclasses import dataclass
from typing import Any, Iterable, Optional

BLOCKING = "blocking"
NONBLOCKING = "nonblocking"

#: The categories an ALLOWLIST admits as harmless. Shipped default for
#: ``config.review.nonblocking_categories``; a configured list EXTENDS this
#: rather than replacing it (a partial list must never drop a shipped
#: harmless category, or a ``typo`` finding would start blocking every PR
#: that names one extra entry).
DEFAULT_NONBLOCKING_CATEGORIES: tuple[str, ...] = (
    "style",
    "formatting",
    "naming",
    "docs",
    "typo",
    "nit",
    "simplification",
    "test-coverage",
)

#: A record missing any of these is a record the gate cannot weigh, so it
#: blocks. ``failure_scenario`` is required by the ReportFindings contract,
#: which is exactly why its presence discriminates nothing and its ABSENCE
#: does: a record without one did not come from a verification pass.
_REQUIRED_FIELDS = ("file", "summary", "failure_scenario")

_FENCE_RE = re.compile(r"```json\s*(.*?)```", re.DOTALL)

#: The one closed enum on a finding (``CONFIRMED | PLAUSIBLE``). It means an
#: adversarial verify pass survived, so it blocks whatever the author-tagged
#: category says.
_CONFIRMED = "confirmed"


class FindingsNormalizeError(ValueError):
    """The payload is not a shape ``normalize`` can read at all.

    Raised for a ``findings`` key that is absent or not an array: that is a
    producer contract violation, distinct from a record that is merely
    unmappable (which becomes a blocking record instead of an error).
    """


@dataclass
class FindingRecord:
    """One finding in the shared vocabulary every producer normalizes into."""

    category: Optional[str] = None
    verdict: Optional[str] = None
    file: Optional[str] = None
    line: Optional[Any] = None
    summary: Optional[str] = None
    failure_scenario: Optional[str] = None
    #: The payload carried none of the recognizable field names, or was not
    #: an object at all. Unreadable is not harmless: it blocks.
    unmappable: bool = False


def _clean(value: Any) -> Optional[str]:
    """A string value stripped, or None for absent/empty/non-string."""
    if not isinstance(value, str):
        return None
    stripped = value.strip()
    return stripped or None


def _record(item: Any) -> FindingRecord:
    """One array element into a record, whatever the producer.

    A non-object, or an object carrying none of the vocabulary this module
    reads, is ``unmappable``: the normalizer refuses to guess what an
    unrecognized shape means, and the fail-closed answer for an unreadable
    record is BLOCKING, never zero findings.
    """
    if not isinstance(item, dict):
        return FindingRecord(unmappable=True)
    record = FindingRecord(
        category=_clean(item.get("category")),
        verdict=_clean(item.get("verdict")),
        file=_clean(item.get("file")),
        line=item.get("line"),
        summary=_clean(item.get("summary")),
        failure_scenario=_clean(item.get("failure_scenario")),
    )
    if all(
        record.__dict__[name] is None
        for name in ("category", "verdict", "file", "line", "summary", "failure_scenario")
    ):
        record.unmappable = True
    return record


def _findings_array(payload: Any, path: str) -> Iterable[Any]:
    """Dig the findings array out of ``payload`` at dotted ``path``."""
    node: Any = payload
    for part in path.split("."):
        if not isinstance(node, dict):
            raise FindingsNormalizeError(f"payload is not an object at '{part}'")
        node = node.get(part)
    if not isinstance(node, list):
        raise FindingsNormalizeError(f"'{path}' is absent or not an array")
    return node


def normalize(payload: Any, source: str) -> list[FindingRecord]:
    """Normalize a producer payload into records.

    ``source`` selects the shape:

    - ``report_findings``: a ``PostToolUse`` payload; reads
      ``.tool_input.findings``.
    - ``fenced_json``: a subagent's final text; reads every `````json``
      fence. No fences means no records (the caller decides what absence
      means). A fence that is not a JSON array of objects is ONE unmappable
      record, because a parse failure is not zero findings.
    - ``codex_review_output``: the ``ExitedReviewMode`` ``review_output``
      object; reads ``.findings``. Field names outside the shared vocabulary
      map to ``unmappable`` records.
    - ``records``: a bare array already in the shared vocabulary (the
      findings-file shape ``fno do review classify`` reads).

    Raises :class:`FindingsNormalizeError` when the findings key itself is
    absent or not an array: that is "not a review", which the caller treats
    as silence, not as a classified record.
    """
    if source == "report_findings":
        return [_record(item) for item in _findings_array(payload, "tool_input.findings")]
    if source == "fenced_json":
        if not isinstance(payload, str):
            raise FindingsNormalizeError("fenced_json payload must be text")
        records: list[FindingRecord] = []
        for body in _FENCE_RE.findall(payload):
            try:
                data = json.loads(body)
            except ValueError:
                records.append(FindingRecord(unmappable=True))
                continue
            if not isinstance(data, list):
                records.append(FindingRecord(unmappable=True))
                continue
            records.extend(_record(item) for item in data)
        return records
    if source == "codex_review_output":
        return [_record(item) for item in _findings_array(payload, "findings")]
    if source == "records":
        if not isinstance(payload, list):
            raise FindingsNormalizeError("records payload must be an array")
        return [_record(item) for item in payload]
    raise FindingsNormalizeError(f"unknown source '{source}'")


def resolve_nonblocking_categories(configured: Any) -> frozenset[str]:
    """The effective harmless-category set: default EXTENDED by ``configured``.

    Mirrors ``resolved_optional_apps``'s extend rule: unset resolves to the
    shipped default, and a configured list EXTENDS the default rather than
    replacing it (so no configured value can demote a shipped category like
    ``typo`` to blocking, and an empty list means the default, never
    "everything is harmless"). A malformed value (not a list of strings)
    degrades to the default with a warning rather than to "everything is
    harmless".
    """
    if configured is None:
        return frozenset(DEFAULT_NONBLOCKING_CATEGORIES)
    if not isinstance(configured, list) or not all(
        isinstance(entry, str) for entry in configured
    ):
        print(
            "fno review: config.review.nonblocking_categories is malformed; "
            "using the shipped default",
            file=sys.stderr,
        )
        return frozenset(DEFAULT_NONBLOCKING_CATEGORIES)
    resolved = set(DEFAULT_NONBLOCKING_CATEGORIES)
    for entry in configured:
        cleaned = entry.strip().lower()
        if cleaned:
            resolved.add(cleaned)
    return frozenset(resolved)


def _has_required_fields(record: FindingRecord) -> bool:
    return all(_clean(record.__dict__[name]) for name in _REQUIRED_FIELDS)


def classify(
    record: FindingRecord,
    nonblocking_categories: Iterable[str] | None = None,
) -> str:
    """BLOCKING or NONBLOCKING, by one rule evaluated in order.

    1. Unmappable or missing a required field blocks.
    2. ``verdict: CONFIRMED`` blocks, whatever the category.
    3. A category in the allowlist is nonblocking.
    4. Everything else blocks, including absent and unrecognized categories.
    """
    allow = frozenset(DEFAULT_NONBLOCKING_CATEGORIES)
    if nonblocking_categories is not None:
        allow = frozenset(nonblocking_categories)
    if record.unmappable or not _has_required_fields(record):
        return BLOCKING
    if _clean(record.verdict) and _clean(record.verdict).lower() == _CONFIRMED:
        return BLOCKING
    category = _clean(record.category)
    if category and category.lower() in allow:
        return NONBLOCKING
    return BLOCKING


def finding_key(record: FindingRecord) -> str:
    """The stable ``file:line:category`` identity of a finding.

    Round-over-round disposition matching keys on this, so a fixed finding
    stays the same finding across review rounds even as its summary text
    changes. Missing parts render empty; the key is an identity string, not
    a rendered sentence.
    """
    line = "" if record.line is None else str(record.line)
    category = _clean(record.category) or ""
    return f"{_clean(record.file) or ''}:{line}:{category.lower()}"


@dataclass
class FindingPrimitive:
    """The bounded per-finding tuple the attestation event carries.

    The gate re-derives blocking from ``category``/``verdict``/
    ``has_required_fields`` with its own copy of the rule and never reads
    ``blocking``, so a hand-written event claiming zero blocking findings
    over a CONFIRMED one is refused.
    """

    category: Optional[str]
    verdict: Optional[str]
    blocking: bool
    has_required_fields: bool
    finding_key: str

    def as_dict(self) -> dict[str, Any]:
        return {
            "category": self.category,
            "verdict": self.verdict,
            "blocking": self.blocking,
            "has_required_fields": self.has_required_fields,
            "finding_key": self.finding_key,
        }


@dataclass
class Summary:
    """What ``summarize`` returns: the counts plus the re-derivable record."""

    blocking_count: int
    nonblocking_count: int
    category_histogram: dict[str, int]
    findings: list[FindingPrimitive]

    def as_dict(self) -> dict[str, Any]:
        return {
            "findings_blocking": self.blocking_count,
            "findings_nonblocking": self.nonblocking_count,
            "category_histogram": self.category_histogram,
            "findings": [primitive.as_dict() for primitive in self.findings],
        }


def summarize(
    records: list[FindingRecord],
    nonblocking_categories: Iterable[str] | None = None,
) -> Summary:
    """Classify every record and count both directions.

    Every input record produces exactly one primitive, so nothing is dropped
    on the floor between producer and gate.
    """
    histogram: Counter[str] = Counter()
    primitives: list[FindingPrimitive] = []
    blocking = 0
    nonblocking = 0
    for record in records:
        verdict = classify(record, nonblocking_categories)
        histogram[(_clean(record.category) or "").lower()] += 1
        primitives.append(
            FindingPrimitive(
                category=_clean(record.category),
                verdict=_clean(record.verdict),
                blocking=verdict == BLOCKING,
                has_required_fields=_has_required_fields(record),
                finding_key=finding_key(record),
            )
        )
        if verdict == BLOCKING:
            blocking += 1
        else:
            nonblocking += 1
    return Summary(
        blocking_count=blocking,
        nonblocking_count=nonblocking,
        category_histogram=dict(histogram),
        findings=primitives,
    )
