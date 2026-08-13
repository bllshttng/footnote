"""The scope denominator and its refusal predicate (x-cbab).

Two facts drove this module's shape, both proven on real specimens in the plan:

1. Multi-deliverable scope cannot be DETECTED from node prose. x-0707's real ask
   is a coordinated noun phrase with zero numerals; x-0707's measurement line is
   five digit pairs that look exactly like an enumeration. A regex tight enough
   to skip the measurements misses the ask; a regex loose enough to catch the ask
   fires on every measurement bullet. So count is a DECLARATION, never a detection.

2. What CAN be read structurally is whether a denominator EXISTS at all. That is
   three field reads, zero prose, and it is the gate that refuses a plan-less code
   payload from starting (``denominator_absent``).

``enumerated_scope`` is the narrow exception: a high-precision predicate whose
only power is to WITHDRAW the cheap ``--deliverables`` exit for a node that is
unambiguously enumerated, forcing a real plan instead. It gates nothing on its
own, and a non-fire asserts nothing - it is never read as "singular".
"""
from __future__ import annotations

import re

__all__ = [
    "enumerated_scope",
    "denominator_absent",
    "is_code_payload",
    "absent_denominator_refusal",
]


# ── enumerated_scope: the one-way ratchet ─────────────────────────────────────
#
# Fires only on structures a human reads as an enumerated list. A false positive
# withdraws the cheap exit and buys the friction that gets gates switched off, so
# each detector is tuned for precision over recall. The pinned miss below is the
# proof recall was sacrificed on purpose.

_CARDINAL_WORDS = {
    "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
}
# Words ending in 's' that are not plural nouns. Small on purpose: the controls
# carry no cardinals, so this only shapes organic-node false positives, which the
# deliverables-1 ratio measurement watches after landing.
_NON_PLURAL = {
    "this", "thus", "was", "has", "his", "its", "plus", "versus", "always",
    "else", "since", "once", "some", "more", "less", "over", "into", "onto",
    "because", "across", "towards", "always", "is", "as", "us",
}

# (1), (2) ... parenthesized ordinals anywhere in the text.
_ORDINAL_PAREN = re.compile(r"\((\d{1,2})\)")
# A markdown/list numbered marker at a line start: "1. " / "2. ". Requires the
# line-start anchor so "see section 21." and "x-0707." (a sentence ending in a
# node id) do not read as enumeration.
_ORDINAL_LIST = re.compile(r"(?m)^\s*(\d{1,2})\.\s")
# "both <word> ... and <word>" - the literal two-member construction.
_BOTH_AND = re.compile(r"\bboth\b\s+[A-Za-z][^.]{0,60}?\s+and\s+[A-Za-z]", re.I)


def _plural_noun(tok: str) -> bool:
    """A coarse plural-noun test: a content word ending in 's' (not 'ss')."""
    t = tok.lower().strip(".,;:()\"'")
    return (
        len(t) >= 3
        and t.endswith("s")
        and not t.endswith("ss")
        and t not in _NON_PLURAL
    )


def _ordinal_run(text: str) -> bool:
    """Two or more distinct ordinal markers - ``(1) ... (2)`` or a numbered list."""
    nums = {int(m.group(1)) for m in _ORDINAL_PAREN.finditer(text)}
    nums.update(int(m.group(1)) for m in _ORDINAL_LIST.finditer(text))
    return len(nums) >= 2


def _cardinal_governs_plural(text: str) -> bool:
    """A cardinal 2-10 (word or digit) with a plural noun within three tokens."""
    tokens = re.findall(r"[A-Za-z]+|\d+", text)
    for i, tok in enumerate(tokens):
        is_cardinal = tok.lower() in _CARDINAL_WORDS or (
            tok.isdigit() and 2 <= int(tok) <= 10
        )
        if is_cardinal and any(_plural_noun(w) for w in tokens[i + 1 : i + 4]):
            return True
    return False


def enumerated_scope(title: str, details: str) -> bool:
    """True iff title+details carry an unambiguous multi-deliverable enumeration.

    A NON-FIRE ASSERTS NOTHING. x-0707's actual ask (a coordinated noun phrase
    with no numerals) does not fire, and that is correct, not a gap: detecting it
    would require parsing meaning, which is the trap this module exists to avoid.
    The structural ``denominator_absent`` predicate below is what protects that
    node, not this ratchet.
    """
    text = f"{title}\n{details or ''}"
    return _ordinal_run(text) or _cardinal_governs_plural(text) or bool(_BOTH_AND.search(text))


# ── denominator_absent: the structural gate predicate ─────────────────────────


def is_code_payload(phases: list[str] | None) -> bool:
    """A code payload is one whose resolved phase set is not plan-only.

    Reuses fold's ``_PLAN_PHASES = {think, plan}`` vocabulary as its complement,
    so "planned" means the same thing here and on the telemetry surface and the
    two cannot drift into separate definitions. A run with no resolved phases is
    treated as code (fail closed): the common case is a build, and the cost of a
    false positive is "name a denominator", not "ship untracked scope".
    """
    if not phases:
        return True
    return not all(p in {"think", "plan"} for p in phases)


def denominator_absent(
    *, plan_path: str | None, deliverables: int | None, payload_is_code: bool
) -> bool:
    """The structural predicate: a code payload with no plan and no declared count.

    Three field reads, zero prose parsing. ``payload_is_code`` is resolved by the
    caller via :func:`is_code_payload` (or the merge-time PR-diff check); the
    other two are manifest fields. Fully reproducible.
    """
    no_plan = not (plan_path and plan_path.strip())
    return bool(payload_is_code and no_plan and deliverables is None)


def absent_denominator_refusal(
    *, node: dict | None, plan_path: str | None, deliverables: int | None
) -> str | None:
    """The refusal message for a plan-less code node dispatch, or None.

    None when the gate does not fire: a free-text idea input (it makes its own
    denominator via /blueprint), a non-code payload (docs/infra - out of this
    gate's scope per the plan), or a dispatch that already carries a denominator
    (a bound plan, or a ``--deliverables`` declaration). Returns the message for
    the caller to echo and exit on; this leaf module does no I/O.

    The cheap exit is a declared count on a SINGULAR node: a stamped N is
    falsifiable, so it is allowed. An enumerated node forfeits that exit - its own
    prose declares several deliverables, so a count there is a lie that ships one
    of N behind a falsifiable-but-cheap claim. So ``enumerated_scope`` firing
    refuses a ``--deliverables`` invocation too, not only the no-denominator case.
    """
    if not isinstance(node, dict):
        return None
    payload_is_code = node.get("domain") == "code"
    has_plan = bool(plan_path and plan_path.strip())
    if not payload_is_code or has_plan:
        return None
    enumerated = enumerated_scope(
        str(node.get("title") or ""), str(node.get("details") or "")
    )
    # A declared count on a singular node is the allowed cheap exit. On an
    # enumerated node it is withdrawn (fall through to the refusal below).
    if deliverables is not None and not enumerated:
        return None
    lines = [
        "fno target init: this code node has no scope denominator.",
        "A plan-less code dispatch makes 'shipped M of N' inexpressible - the",
        "shortfall surfaces only when a human counts by hand. Give it a denominator:",
        '  - /fno:blueprint quick "<feature>"   (a plan enumerating the tasks;',
        "    the plan_path fills and the denominator is its task count), OR",
        "  - re-run with --deliverables N        (declare the count; a stamped N",
        "    is falsifiable where an absent denominator is not).",
    ]
    if enumerated:
        lines += [
            "",
            "This node reads as multi-deliverable, so --deliverables is withdrawn:",
            "blueprint a plan that enumerates the work. A declared count on an",
            "enumerated node would let the run stamp 1 and ship one of several.",
        ]
    return "\n".join(lines)
