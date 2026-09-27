"""The scope denominator and its derivation from node details.

1. Multi-deliverable scope cannot be DETECTED from node prose with high
   recall. x-aaaa's real ask is a coordinated noun phrase with zero numerals;
   x-aaaa's measurement line is five digit pairs that look exactly like an
   enumeration. So the derivation below reads only the unambiguous structures
   (parenthesized ordinals, a numbered list, a two-member construction) and
   answers 1 when none fires - the same precision-over-recall stance the
   refusal era had, pointed at deriving instead of blocking.

2. What CAN be read structurally is whether the node itself enumerates its
   work. ``enumerated_scope`` is a high-precision predicate over that
   structure; it feeds the ``target_denominator`` event's ``enumerated`` flag
   and the derivation's ordinal count. A non-fire asserts nothing.

Under lean dispatch, ``fno do target init`` on a plan-less code node stamps
``deliverables: N`` derived from the node's own details and proceeds. The
count is falsifiable (a reader can recount the node's enumeration), so
"shipped M of N" stays expressible without a blueprint. An explicit
``--deliverables N`` still wins.
"""
from __future__ import annotations

import re

__all__ = [
    "enumerated_scope",
    "derive_deliverables",
]


# ── enumerated_scope: the high-precision enumeration read ────────────────────
#
# Fires only on structures a human reads as an enumerated list. A false
# positive stamps a wrong denominator, so each detector stays tuned for
# precision over recall. The pinned miss below is the proof recall was
# sacrificed on purpose.

_CARDINAL_WORDS = {
    "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
}
# Words ending in 's' that are not plural nouns. Small on purpose: the controls
# carry no cardinals, so this only shapes organic-node false positives, which
# the derived-1 ratio measurement watches after landing.
_NON_PLURAL = {
    "this", "thus", "was", "has", "his", "its", "plus", "versus", "always",
    "else", "since", "once", "some", "more", "less", "over", "into", "onto",
    "because", "across", "towards", "always", "is", "as", "us",
}

# (1), (2) ... parenthesized ordinals anywhere in the text.
_ORDINAL_PAREN = re.compile(r"\((\d{1,2})\)")
# A markdown/list numbered marker at a line start: "1. " / "2. ". Requires the
# line-start anchor so "see section 21." and "x-aaaa." (a sentence ending in a
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


def _ordinal_numbers(text: str) -> set[int]:
    """Every distinct ordinal marker - ``(1) ... (2)`` or a numbered list."""
    nums = {int(m.group(1)) for m in _ORDINAL_PAREN.finditer(text)}
    nums.update(int(m.group(1)) for m in _ORDINAL_LIST.finditer(text))
    return nums


def _ordinal_run(text: str) -> bool:
    """Two or more distinct ordinal markers - ``(1) ... (2)`` or a numbered list."""
    return len(_ordinal_numbers(text)) >= 2


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

    A NON-FIRE ASSERTS NOTHING. x-aaaa's actual ask (a coordinated noun phrase
    with no numerals) does not fire, and that is correct, not a gap: detecting
    it would require parsing meaning, which is the trap this module exists to
    avoid. Such a node derives a count of 1, which is falsifiable in exactly
    the way the refusal era demanded.

    The weak detectors (cardinal-governs-plural, both-and) read the TITLE only:
    in a details body they fire on measured incident narrative ("collided as
    two PRs", "both rewrite X and Y"), which no human reads as a scope
    enumeration. Ordinals keep the full text - "(1) ... (2)" and a numbered
    list stay unambiguous wherever they appear.
    """
    text = f"{title}\n{details or ''}"
    return (
        _ordinal_run(text)
        or _cardinal_governs_plural(title)
        or bool(_BOTH_AND.search(title))
    )


def derive_deliverables(title: str, details: str) -> int:
    """The deliverables count a plan-less code node stamps at init.

    The node's own enumeration decides: the highest ordinal marker wins (gaps
    count - "(1) ... (4)" reads as four deliverables even with (3) unstated).
    A two-member construction without ordinals ("ship both X and Y") reads as
    2. Anything else is one deliverable, which is the honest floor for a node
    that names a single ask.
    """
    text = f"{title}\n{details or ''}"
    nums = _ordinal_numbers(text)
    if nums:
        return max(nums)
    if _BOTH_AND.search(title) or _cardinal_governs_plural(title):
        return 2
    return 1
