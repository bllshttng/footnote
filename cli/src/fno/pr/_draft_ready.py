"""The ready-for-review draft guard: one config key, two enforcement points.

``config.pr.open_ready`` (default True) is the user's rule - PRs open ready,
never draft, unless the user asks in the moment - as a knob instead of rule
text. Two consumers:

- The gh proxy's delegate path (``gh_proxy.delegate``) refuses draft-intent
  argv (``gh pr create --draft``, ``gh pr ready <n> --draft``) from any
  Footnote-launched process. ``worker_environment`` puts the shim on every
  spawned worker's PATH, so this is the one door all six harnesses cross.
- The pr-watch sweep (``pr_watch._dispatch``) flips an open fno-bound draft
  PR back to ready and journals the flip.

The one escape is an operator law row at a ``pr-draft:`` subject whose
decision equals ``DRAFT_DECISION`` - the same three-state law read the
review-coverage waiver gate prescribes (``_coverage_gate.law_authority``).
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Callable, Optional, Sequence

from fno.pr._quota import command_args

#: The one decision value that counts as an affirmative draft ruling. The
#: ``fno inbox law set`` door mints exactly this string when the operator
#: grants a draft; row existence carries no polarity (a note at the subject
#: is not a waiver), mirroring ``WAIVER_DECISION`` in ``_coverage_gate``.
DRAFT_DECISION = "draft permitted for this pull request"

_DRAFT_TOKENS = ("--draft", "--draft=true")


def draft_subject_for_branch(branch: str) -> str:
    return f"pr-draft:{branch}"


def draft_subject_for_pr(slug: str, pr_number: int) -> str:
    return f"pr-draft:{slug}#{pr_number}"


def draft_argv_intent(command: Sequence[str]) -> tuple[bool, Optional[str], Optional[int]]:
    """Draft intent in ``command_args()``-normalized gh argv.

    Returns ``(is_intent, kind, pr_number)``: kind is ``"create"`` for
    ``pr create`` carrying ``--draft`` (no PR exists yet; the subject keys on
    the branch) and ``"ready"`` for ``pr ready <n> --draft`` (the subject
    keys on the PR). ``--draft=false`` creates ready in gh and is not intent.
    """
    if len(command) < 2 or command[0] != "pr":
        return False, None, None
    sub, rest = command[1], command[2:]
    if not any(token in rest for token in _DRAFT_TOKENS):
        return False, None, None
    if sub == "create":
        return True, "create", None
    if sub == "ready":
        for token in rest:
            if token.isdigit():
                return True, "ready", int(token)
    return False, None, None


def draft_law_authority(
    subject: str, *, list_fn: Optional[Callable] = None
) -> tuple[str, str]:
    """Three-state law resolution for one ``pr-draft:`` subject.

    ``(status, probe)`` with status ``single`` / ``none`` / ``unknown`` -
    the exact shape of ``law_authority`` in ``_coverage_gate.py``. Only an
    ``authority_source == "operator"`` row with decision equal to
    ``DRAFT_DECISION`` counts: a draft exception is the user's call, and a
    ``chat_attested`` row cannot carry it.
    """
    try:
        if list_fn is None:
            from fno.decide import list_decisions

            list_fn = list_decisions
        _label, rows, damaged = list_fn(subject, lane="law", state="live")
    except Exception as exc:  # noqa: BLE001 - a dead probe is unknown, never none
        return "unknown", f"decision probe failed for {subject}: {type(exc).__name__}: {exc}"
    rows = [row for row in rows if str(row.get("authority_source") or "") == "operator"]
    if damaged:
        noun = "row" if damaged == 1 else "rows"
        return "unknown", f"decision probe: {damaged} damaged {noun} for {subject}"
    if not rows:
        return "none", ""
    if len(rows) > 1:
        return "unknown", f"decision probe: conflicting law rows for {subject}"
    decision = rows[0].get("decision")
    if decision is None:
        return "unknown", f"decision probe: single law row carries no decision for {subject}"
    if str(decision) == DRAFT_DECISION:
        return "single", ""
    return "none", ""


def _current_branch(cwd: Optional[str]) -> Optional[str]:
    try:
        proc = subprocess.run(
            ["git", "branch", "--show-current"],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    branch = proc.stdout.strip() if proc.returncode == 0 else ""
    return branch or None


def _open_ready(cwd: Optional[str]) -> bool:
    """``pr.open_ready`` for the repo at cwd; default True on any load
    failure - the rule's default state is on, and a broken config file
    never disables a guard."""
    if not cwd:
        return True
    try:
        from fno.config import load_settings_for_repo

        return bool(load_settings_for_repo(Path(cwd)).pr.open_ready)
    except Exception:  # noqa: BLE001 - fail toward the rule, never away from it
        return True


def _refusal_text(subject: str) -> str:
    return (
        f"gh proxy: --draft refused (config.pr.open_ready): pull requests open "
        f"ready for review, never draft. To grant this one, record the operator "
        f"ruling from your own terminal: "
        f'fno inbox law set {subject} "{DRAFT_DECISION}" --rationale "<why>". '
        f"To turn the rule off for this repo: config pr.open_ready = false."
    )


def draft_refusal(
    args: Sequence[str],
    cwd: Optional[str] = None,
    *,
    open_ready_fn: Optional[Callable[[Optional[str]], bool]] = None,
    authority_fn: Optional[Callable[[str], tuple[str, str]]] = None,
    branch_fn: Optional[Callable[[Optional[str]], Optional[str]]] = None,
) -> Optional[str]:
    """The proxy-side guard: a refusal line, or None when the call is admitted.

    Runs only on draft-intent argv, so the ordinary gh call pays nothing.
    """
    command = command_args(list(args))
    is_intent, _kind, pr_number = draft_argv_intent(command)
    if not is_intent:
        return None
    open_ready = (open_ready_fn or _open_ready)(cwd)
    if not open_ready:
        return None
    authority = authority_fn or draft_law_authority
    if _kind == "ready" and pr_number is not None:
        from fno.graph._reconcile import resolve_current_repo_slug

        slug = resolve_current_repo_slug(cwd)
        subject = draft_subject_for_pr(slug or "unknown-repo", pr_number)
    else:
        branch = (branch_fn or _current_branch)(cwd)
        subject = draft_subject_for_branch(branch or "unknown-branch")
    status, _probe = authority(subject)
    if status == "single":
        return None
    return _refusal_text(subject)


# ---------------------------------------------------------------------------
# The sweep flip (pr-watch candidate loop)
# ---------------------------------------------------------------------------


def run_draft_flip(
    cand,
    obs,
    *,
    emit: Callable[[str, dict], object],
    ruling_fn: Optional[Callable[[str], tuple[str, str]]] = None,
    runner: Callable[..., "subprocess.CompletedProcess"] = subprocess.run,
    open_ready_fn: Optional[Callable[[Optional[str]], bool]] = None,
) -> str:
    """Flip one observed draft PR to ready, unless an operator ruling spares it.

    Returns a receipt string; never raises - the tick's candidate loop treats
    a flip failure as one degraded row, not a broken sweep. The ruling check
    mirrors the proxy guard: only an operator-source ``single`` spares the PR.
    """
    cwd = str(cand.repo_dir) if cand.repo_dir else None
    slug = cand.repo_slug or "unknown-repo"
    subject = draft_subject_for_pr(slug, cand.pr_number)
    authority = ruling_fn or draft_law_authority
    status, _probe = authority(subject)
    if status == "single":
        return f"spared by operator ruling at {subject}"
    if not (open_ready_fn or _open_ready)(cwd):
        return "open_ready=false; no flip"
    cmd = ["gh", "pr", "ready", str(cand.pr_number)]
    try:
        result = runner(cmd, cwd=cwd, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.SubprocessError) as exc:
        emit(
            "pr_watch_draft_flip",
            {"pr": cand.pr_number, "repo": slug, "node": cand.node_id, "outcome": "error", "error": str(exc)[:200]},
        )
        return f"flip failed: {exc}"
    if result.returncode != 0:
        err = (result.stderr or result.stdout or "").strip()[:200]
        emit(
            "pr_watch_draft_flip",
            {"pr": cand.pr_number, "repo": slug, "node": cand.node_id, "outcome": "error", "error": err},
        )
        return f"flip refused: {err}"
    emit(
        "pr_watch_draft_flip",
        {"pr": cand.pr_number, "repo": slug, "node": cand.node_id, "outcome": "flipped"},
    )
    return "flipped"
