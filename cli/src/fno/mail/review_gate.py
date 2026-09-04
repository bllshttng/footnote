"""The review machinery of the raw-send lane, in the module its question names.

Everything here answers one question: may THIS review fire at THAT
recipient. The codex half resolves the structured review target and
measures the recipient's checkout so a review can never complete cleanly
over an empty diff; the keystroke half asks the capability table whether
the recipient's harness can fire a review verb at all (x-a3e8). Extracted
from mail/cli, which was over the shrink budget and had grown a second
review question inside itself.
"""

from __future__ import annotations

import re
import subprocess

from fno.agents.harness_map import capabilities as _harness_capabilities

# Single-sourced from the capability table (the codex row's review_verbs, the
# same place the verb normalizer reads native_verbs): a second hand-written
# enumeration would drift the first time a verb is added.
_CODEX_REVIEW_VERBS = frozenset(_harness_capabilities("codex")["review_verbs"])
_COMMIT_SHA = re.compile(r"[0-9a-fA-F]{7,64}")
_EXPLICIT_PR_REVIEW = re.compile(
    r"^HEAD (?P<head>[0-9a-fA-F]{7,64}) of PR (?P<pr>[1-9][0-9]*) "
    r"against origin/(?P<base>[A-Za-z0-9][A-Za-z0-9._/-]*)$"
)


def codex_default_review_base(cwd: str | None) -> str | None:
    """Return the repository-declared origin default branch, never a guessed name."""
    if not cwd:
        return None
    try:
        proc = subprocess.run(
            [
                "git",
                "-C",
                cwd,
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    ref = proc.stdout.strip()
    return ref if proc.returncode == 0 and ref else None


def codex_review_subject_nonempty(cwd: str | None, base_ref: str) -> tuple[bool, str]:
    """Whether the recipient session's checkout has branch-side changes to read.

    A ``baseBranch`` review/start diffs the RECIPIENT session's checkout against
    the base: the target scopes the base side only, and codex computes the diff
    in the thread's cwd. A session living on the base branch therefore reviews
    an empty diff, reports clean with an empty findings array, and attests
    nothing - the measured 2026-08-30 shape where reviews "succeeded" at
    recipients on main while the PRs sat at review_coverage_uncovered with a
    receipt in hand. This is the fire-side mirror of emit-attestation.sh's
    empty-diff refusal: refuse BEFORE the RPC when the measured subject is
    empty or unmeasurable, naming the checkout so the remedy is obvious.

    The measurement is the COMMITTED range (merge-base..HEAD), matching the
    ``baseBranch`` target's own scope; the protocol's separate
    ``uncommittedChanges`` target exists precisely because baseBranch does not
    read the working tree. A checkout whose branch-side work is entirely
    uncommitted reads empty HERE as it would in the review itself; the caller
    who wants the working tree asks for ``/review --uncommitted`` and skips
    this guard.
    """
    if not cwd:
        return False, (
            "the recipient session's cwd is unknown, so the review subject "
            "cannot be measured; re-register the row (`fno agents register`) "
            "or fire from the PR worktree session"
        )

    def _git(*args: str) -> str | None:
        try:
            proc = subprocess.run(
                ["git", "-C", cwd, *args],
                capture_output=True,
                text=True,
                timeout=2,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        return proc.stdout.strip() if proc.returncode == 0 else None

    head = _git("rev-parse", "HEAD")
    if not head:
        return False, (
            f"the recipient session's checkout at {cwd} has no resolvable HEAD"
        )
    merge_base = _git("merge-base", head, base_ref) or _git(
        "merge-base", head, base_ref.removeprefix("origin/")
    )
    if not merge_base:
        return False, (
            f"the recipient session's checkout at {cwd} (HEAD {head[:8]}) "
            f"cannot resolve {base_ref} to a merge-base; fetch the base or "
            "fire from a checkout that tracks it"
        )
    names = _git("diff", "--name-only", f"{merge_base}..{head}")
    if names is None:
        # A failed or timed-out measurement is NOT a measured empty diff: the
        # refusal below would assert "0 changed files" on a git call that never
        # completed, sending the caller to move the review when nothing was
        # wrong with it. Unmeasurable keeps its own message, like the HEAD and
        # merge-base stages above.
        return False, (
            f"the recipient session's checkout at {cwd} (HEAD {head[:8]}) "
            f"could not be measured against {base_ref} (git timed out or "
            "failed); measure it by hand before firing the review"
        )
    count = len([line for line in names.splitlines() if line])
    if count == 0:
        return False, (
            f"the recipient session's checkout at {cwd} (HEAD {head[:8]}) has "
            f"0 changed files against {base_ref}; the review would read an "
            "empty diff, complete cleanly, and attest nothing"
        )
    return True, f"{count} changed files against {base_ref} at HEAD {head[:8]}"


def codex_review_target(
    payload: str, *, default_base: str | None = None
) -> tuple[str | None, bool]:
    """Resolve the structured review target without inventing custom instructions."""
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
        # measures that checkout and refuses an empty subject (2026-08-30:
        # open PRs sat uncovered because their recipients lived on the base
        # branch and honestly reviewed nothing).
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
    return "uncommittedChanges", True


def keystroke_review_refusal(name: str, harness: str, first_token: str) -> str | None:
    """The full refusal text when a raw review verb cannot ride this
    recipient's keystroke lane, or ``None`` when it may (x-a3e8). The row
    answer is the table's; the remedy bullets are the mail lane's."""
    from fno.agents.harness_map import review_lane_block

    row_reason = review_lane_block(harness or "", first_token)
    if row_reason is None:
        return None
    return (
        f"{name!r} runs {harness or 'an undeclared harness'}, whose "
        f"{row_reason}\n"
        "  - to have the model READ this anyway, drop --raw (a wrapped "
        "send delivers it as text, which is all this lane could do with "
        "it)\n"
        "  - run the review on a harness whose row reads native: "
        "/code-review on a claude worker, /review on a codex daemon "
        "thread"
    )
