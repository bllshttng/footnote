"""Resolve structured review targets sent to a Codex app-server thread."""
from __future__ import annotations

import re


_COMMIT_SHA = re.compile(r"[0-9a-fA-F]{7,64}")
_EXPLICIT_PR_REVIEW = re.compile(
    r"^(?:(?:low|medium|high|xhigh|max) --comment (?P<comment_pr>[1-9][0-9]*) )?"
    r"HEAD (?P<head>[0-9a-fA-F]{7,64})"
    r"(?: of PR (?P<pr>[1-9][0-9]*))? "
    r"against origin/(?P<base>[A-Za-z0-9][A-Za-z0-9._/-]*)$"
)


def explicit_review_pr_number(payload: str) -> int | None:
    """Return the PR number from a supported explicit review payload."""
    match = _EXPLICIT_PR_REVIEW.fullmatch(payload)
    if not match:
        return None
    value = match.group("pr") or match.group("comment_pr")
    return int(value) if value else None


def resolve_codex_review_target(
    payload: str, *, default_base: str | None = None
) -> tuple[str | None, bool]:
    """Resolve the structured review target without inventing instructions."""
    parts = payload.split(maxsplit=1)
    if len(parts) == 1:
        target = f"baseBranch:{default_base}" if default_base else None
        return target, False
    remainder = parts[1].strip()
    explicit_pr = _EXPLICIT_PR_REVIEW.fullmatch(remainder)
    if explicit_pr:
        # The PR/HEAD identity remains in the raw payload for the author and
        # audit trail. Codex review/start receives the PR's explicit base
        # scope; that scopes the BASE side only - codex still computes the
        # diff in the recipient session's cwd, so the head side is whatever
        # checkout the recipient sits in. The fire-side guard in _raw_send
        # measures that checkout and refuses an empty subject.
        return f"baseBranch:origin/{explicit_pr.group('base')}", False
    if remainder.startswith("HEAD "):
        # A malformed explicit target must not fall through to
        # uncommittedChanges, which would review a different diff.
        return None, False
    base = remainder.split()
    if base[0] == "--base":
        # A named base is an explicit scope request: a malformed form (dangling
        # flag, a flag-like value, trailing tokens) must refuse rather than
        # fall through to uncommittedChanges, which silently reviews a
        # different diff than the one the operator asked for.
        if len(base) == 2 and not base[1].startswith("--"):
            return f"baseBranch:{base[1]}", False
        return None, False
    if remainder == "--uncommitted":
        return "uncommittedChanges", False
    if _COMMIT_SHA.fullmatch(remainder):
        return f"commit:{remainder}", False
    if remainder.startswith("custom:") and remainder != "custom:":
        return remainder, False
    return None, False
