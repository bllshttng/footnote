"""The spawn's semantic phase label: which kind of work does the message name?

A phase stamps the node's ``sessions[]`` row and follows the assignment for
life - a review row is a retirement blocker the row never grows out of - so
the label comes from the work's own shape and never from a guess. Explicit
``--session-phase`` wins before this helper runs; this is the inference of
last resort. Unlabelable messages (arbitrary prose, unknown verbs) answer
``""`` and the caller skips the row write, naming the skip.
"""

from __future__ import annotations

from fno.agents.harness_map import is_target_family

REVIEW_VERB_PREFIXES = ("/code-review", "/review", "/fno:review")


def infer_phase(message: str | None) -> str:
    """``do`` | ``review`` | ``blueprint`` | ``think`` | ``""`` when
    unlabelable. Harness-qualified spellings (``/fno:`` and ``$fno:``) are
    normalized via ``lstrip("/$")`` before matching; a leading token without
    a slash or dollar never reads as a review."""
    verb = (message or "").lstrip().split(maxsplit=1)[0] if message else ""
    bare = verb.lstrip("/$")
    if is_target_family(message):
        return "do"
    if verb.startswith(REVIEW_VERB_PREFIXES) or bare.startswith("fno:review"):
        return "review"
    if bare.startswith("fno:blueprint"):
        return "blueprint"
    if bare.startswith("fno:think"):
        return "think"
    return ""
