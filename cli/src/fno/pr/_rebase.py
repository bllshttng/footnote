"""In-package port of ``scripts/lib/rebase-resolve.sh`` (ab-d4c98550, US3).

Two-phase rebase with the conflict-delegation protocol. The exit-code contract
is load-bearing for skill orchestration (a caller dispatches the
conflict-resolver agent on exit 42, then calls back ``--continue``), so it is
preserved verbatim:

PHASE A (default): fetch + rebase onto BASE.
    0  -> status "clean"          (no conflicts)
    1  -> status "failed"         (conflict_resolution=fail or non-conflict error)
          status "refused"        (guardrails blocked auto-resolve)
          status "fetch_failed"   (git fetch failed)
    2  -> status "dirty"          (working tree has uncommitted changes)
    3  -> status "refused"        (called from main/master/develop/dev)
    42 -> status "needs_resolver" (guardrails passed; caller invokes the agent)

PHASE B (--continue): assume the agent staged + committed resolutions.
    0  -> status "resolved"
    42 -> status "needs_resolver" (more conflicts)
    1  -> status "failed"

Output: one JSON line on stdout; all git output + human messages go to stderr
so stdout stays clean for the caller to parse.
"""
from __future__ import annotations

import json
import os
import sys
from typing import List, Optional, Sequence, Tuple

from fno.pr._proc import ToolMissing, run

# Protected branches the bash refuses to rebase (exit 3).
_PROTECTED = {"main", "master", "develop", "dev"}


def _emit(status: str, base: str, extra: Optional[dict] = None) -> None:
    """Print ``{status, base} + extra`` as a compact JSON line to stdout.

    Matches the bash ``emit_json_full`` (jq ``{status,base} + $extra``): key
    order is status, base, then the extra keys in insertion order.
    """
    obj = {"status": status, "base": base}
    if extra:
        obj.update(extra)
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def _git_env() -> dict:
    """Environment for git calls with a NON-INTERACTIVE editor.

    ``git rebase --continue`` finalises the current patch by committing the
    staged resolution; with the default editor it blocks waiting for the
    commit message (on a headless runner: "Terminal is dumb, but EDITOR
    unset"). Forcing ``GIT_EDITOR``/``GIT_SEQUENCE_EDITOR`` to a no-op makes
    git reuse the prefilled message non-interactively, which is the correct
    posture for automated rebase tooling (a caller never edits a message
    here). Harmless for the non-committing git calls.
    """
    return {**os.environ, "GIT_EDITOR": "true", "GIT_SEQUENCE_EDITOR": "true"}


def _git(args: Sequence[str], cwd: str) -> Tuple[int, str, str]:
    """Run git; echo its combined output to stderr (the bash ``1>&2`` idiom)."""
    res = run(["git", *args], cwd=cwd, env=_git_env())
    # The bash pipes git's stdout+stderr to the caller's stderr so the JSON
    # line on stdout stays clean. Mirror that for diagnostics.
    if res.stdout:
        sys.stderr.write(res.stdout)
    if res.stderr:
        sys.stderr.write(res.stderr)
    return res.returncode, res.stdout, res.stderr


def _git_quiet(args: Sequence[str], cwd: str) -> Tuple[int, str, str]:
    """Run git WITHOUT echoing output (for queries whose stdout we consume)."""
    res = run(["git", *args], cwd=cwd, env=_git_env())
    return res.returncode, res.stdout, res.stderr


def _conflict_files(cwd: str) -> List[str]:
    """Currently-conflicting paths: ``git diff --name-only --diff-filter=U``."""
    _, out, _ = _git_quiet(["diff", "--name-only", "--diff-filter=U"], cwd)
    return [ln for ln in out.splitlines() if ln.strip()]


def _basename(path: str) -> str:
    return path.rsplit("/", 1)[-1]


def check_guardrails(conflict_files: Sequence[str], cwd: str) -> Optional[dict]:
    """Port of the bash ``check_guardrails``.

    Returns ``None`` when every file is safe to auto-resolve. Returns the
    ``refused`` extra dict ``{"reason": ..., "files": [...]}`` when any file
    must be hand-resolved. The first failing reason wins (bash overwrites
    ``refuse_reason`` per file, so the LAST refused file's reason is what the
    bash printed); we mirror that "last reason wins" behaviour.
    """
    refused: List[str] = []
    reason = ""
    for f in conflict_files:
        # Migration files.
        if (
            ("/migrations/" in f or f.startswith("migrations/"))
            or (f == "schema.prisma" or f.endswith("/schema.prisma"))
            or (f.startswith("supabase/migrations/") or "/supabase/migrations/" in f)
            or (f.endswith(".sql") and "migration" in f)
        ):
            refused.append(f)
            reason = "migration file in conflict"
            continue

        # Secret / env files.
        if (
            (f == ".env" or f.startswith(".env.") or ".env." in f)
            or ("/secrets/" in f or f.startswith("secrets/"))
            or "/config/secrets/" in f
        ):
            refused.append(f)
            reason = "secret or env file in conflict"
            continue

        # Lock files and git config files (by basename).
        base = _basename(f)
        if base in {
            "package-lock.json",
            "yarn.lock",
            "Cargo.lock",
            "Gemfile.lock",
            "uv.lock",
            "poetry.lock",
        }:
            refused.append(f)
            reason = "lock file in conflict - hand-resolution required"
            continue
        if base in {".gitattributes", ".gitignore"}:
            refused.append(f)
            reason = "git config file in conflict"
            continue

        # Mass conflicts: more than 3 conflict markers in a single file.
        marker_count = _count_conflict_markers(os.path.join(cwd, f))
        if marker_count > 3:
            refused.append(f)
            reason = (
                f"mass conflicts ({marker_count} hunks) in {f} - "
                "design problem, not merge problem"
            )
            continue

    if refused:
        return {"reason": reason, "files": refused}
    return None


def _count_conflict_markers(path: str) -> int:
    """Count ``<<<<<<< `` lines in a file (bash ``grep -c '^<<<<<<< '``)."""
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            return sum(1 for ln in fh if ln.startswith("<<<<<<< "))
    except OSError:
        return 0


def _needs_resolver_extra(conflict_files: Sequence[str], cwd: str) -> dict:
    _, diff, _ = _git_quiet(["diff", "--cc"], cwd)
    diff_preview = "\n".join(diff.splitlines()[:200]) if diff else ""
    return {"files": list(conflict_files), "diff_preview": diff_preview}


def _phase_b_continue(base: str, cwd: str) -> int:
    """``--continue``: resume from git's native in-progress rebase state.

    Conflicts are checked BEFORE the exit code: in a multi-commit rebase
    ``git rebase --continue`` exits non-zero (1) when it pauses on NEW
    conflicts in a SUBSEQUENT commit, so gating the needs_resolver path on
    ``rc == 0`` would misreport "more conflicts remain" as a hard failure and
    break the caller's resolve-loop (the documented PHASE B contract + AC3-EDGE
    require exit 42 when conflicts remain). Caught by gemini on PR #524.
    """
    sys.stderr.write("Running git rebase --continue...\n")
    rc, _, _ = _git(["rebase", "--continue"], cwd)
    conflicts = _conflict_files(cwd)
    if conflicts:
        refused = check_guardrails(conflicts, cwd)
        if refused is not None:
            sys.stderr.write("Guardrails refused during --continue phase\n")
            _emit("refused", base, refused)
            _git(["rebase", "--abort"], cwd)
            return 1
        _emit("needs_resolver", base, _needs_resolver_extra(conflicts, cwd))
        return 42
    if rc == 0:
        _, log_out, _ = _git_quiet(["log", "--oneline", "--not", base], cwd)
        commits = [ln for ln in log_out.splitlines() if ln.strip()][:20]
        _emit("resolved", base, {"resolution_commits": commits})
        return 0
    _emit("failed", base, {"reason": "git rebase --continue failed"})
    return 1


def _phase_a(base: str, cwd: str) -> int:
    """Initial rebase onto ``base``."""
    # (1) Refuse on protected branches.
    _, curr, _ = _git_quiet(["rev-parse", "--abbrev-ref", "HEAD"], cwd)
    curr = curr.strip()
    if curr in _PROTECTED:
        sys.stderr.write(f"Error: refusing to rebase on protected branch '{curr}'\n")
        _emit("refused", base, {"reason": f"on protected branch '{curr}'"})
        return 3

    # (2) Dirty tree guard.
    _, status_out, _ = _git_quiet(["status", "--porcelain"], cwd)
    if status_out.strip():
        sys.stderr.write("Error: working tree has uncommitted changes\n")
        _emit("dirty", base, {"reason": "working tree has uncommitted changes"})
        return 2

    # (3) Fetch - fail loudly so we don't rebase against stale origin.
    rc, _, ferr = _git_quiet(["fetch", "origin", "--quiet"], cwd)
    if rc != 0:
        first = ferr.splitlines()[0] if ferr.strip() else ""
        _emit("fetch_failed", base, {"reason": first})
        return 1

    # (4) Attempt the rebase.
    rc, _, _ = _git(["rebase", base], cwd)
    if rc == 0:
        _emit("clean", base, {})
        return 0

    # (5) Collect conflicts.
    conflicts = _conflict_files(cwd)
    if not conflicts:
        _emit("failed", base, {"reason": "rebase failed (non-conflict error)"})
        _git(["rebase", "--abort"], cwd)
        return 1

    # (6) conflict_resolution policy.
    if _conflict_resolution() == "fail":
        _git(["rebase", "--abort"], cwd)
        sys.stderr.write("conflict_resolution=fail; aborting rebase\n")
        _emit(
            "failed",
            base,
            {"reason": "conflicts detected, conflict_resolution=fail", "files": conflicts},
        )
        return 1

    # (7) Guardrails.
    refused = check_guardrails(conflicts, cwd)
    if refused is not None:
        sys.stderr.write("Guardrails refused auto-resolution\n")
        _emit("refused", base, refused)
        _git(["rebase", "--abort"], cwd)
        return 1

    # (8) Guardrails passed - leave the rebase in-progress for the agent.
    sys.stderr.write(
        "Conflicts detected; guardrails passed. "
        "Caller must invoke conflict-resolver agent.\n"
    )
    _emit("needs_resolver", base, _needs_resolver_extra(conflicts, cwd))
    return 42


def _conflict_resolution() -> str:
    """Resolve config.auto_merge.conflict_resolution ("opus"|"fail")."""
    try:
        from fno.config import load_settings

        return load_settings().config.auto_merge.conflict_resolution
    except Exception:
        # Settings unreadable -> bash default ("opus", auto-resolve attempted).
        return "opus"


def run_rebase(argv: Sequence[str], cwd: Optional[str] = None) -> int:
    """Entry point. Parses ``--base=`` / ``--continue`` and runs the phase."""
    base = "origin/main"
    phase_continue = False
    for arg in argv:
        if arg.startswith("--base="):
            base = arg[len("--base="):]
        elif arg == "--continue":
            phase_continue = True
    repo = cwd or os.getcwd()
    try:
        rc = _phase_b_continue(base, repo) if phase_continue else _phase_a(base, repo)
    except ToolMissing:
        # git not on PATH -> clean failed verdict, never a raw traceback.
        sys.stderr.write("Error: git CLI not installed or not found on PATH\n")
        _emit("failed", base, {"reason": "git CLI not found"})
        return 1
    if rc == 0:
        # Advisory-only nudge (x-91b5, AC1-UI): a hand-rebase is a common
        # stale-base escape the loop should have caught. The nudge NEVER emits
        # on its own - it just prints the copy-paste tag line so retro's
        # autonomy-debt ranking can see stale-base interventions the operator
        # chooses to record.
        sys.stderr.write(
            "note: if this rebase resolved a stale base the loop should have "
            'caught, tag it: fno event gate-escape stale-base --detail "hand-rebase"\n'
        )
    return rc
