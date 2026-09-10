"""Compile the merge result before the merge lands.

Two green parents can merge into a red main: git joins hunks that never met
on one machine. Specimen: PR #1654 merged as ``3334b826a133`` and broke main
with ``F821 Undefined name _TAG_SHUTDOWN`` at ``cli/src/fno/graph/store.py:525``.
Parent ``e8c2bac9e661`` defined the constant at line 85 and used it at 526; the
PR head ``9d75ec4f17bf`` had neither. The two hunks sat 440 lines apart with no
textual conflict, so the combined tree compiled on no machine that ran CI:
every check runs on the PR head, and main carries no require-branches-up-to-date
protection.

The probe computes ``git merge-tree --write-tree`` locally, extracts ``cli/``
from that tree, and runs the repo-wide ruff + mypy step on it
(``scripts/ci/check-python-static.sh``, the one copy CI itself runs). A PR
whose head already contains its base skips the static run: CI on the head IS
the merge result.

Verdicts are ``ok`` / ``red`` / ``unknown``, the same exit vocabulary as
``_base_lineage``. Deliberate limits: only the Python static step runs - a
``cargo check`` of the merge result would take minutes inside the repo-wide
merge lock, and the specimen was Python. On the auto-merge arm the probe is a
snapshot at arm time, the same snapshot the lineage and overlap guards
already take.
"""
from __future__ import annotations

import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from typing import Optional

from fno.pr._base_lineage import _fetch_ref, _probe, _rev

OK = 0
REFUSED_RED = 3
UNKNOWN = 4

#: The static step runs inside the repo-wide merge lock (`_merge._merge_lock`),
#: so it is bounded like every other probe there. ruff+mypy measured 10.7 s on
#: this repo; 180 s is the ceiling, and a timeout reads as `unknown`.
_STATIC_TIMEOUT_S = 180


def merge_tree(base_rev: str, head_rev: str, cwd: str) -> tuple[str, str]:
    """``(tree_oid, "")`` on a clean merge; ``("", reason)`` otherwise.

    Exit 1 means conflicted: stdout carries ``CONFLICT (...): Merge conflict in
    <path>`` lines, and the reason names them. Any other exit is
    ``("", "unknown: ...")`` - a git that could not answer is never a red.
    """
    res = _probe(["git", "merge-tree", "--write-tree", base_rev, head_rev], cwd)
    if res is None:
        return ("", "unknown: git merge-tree could not run")
    if res.ok:
        lines = res.stdout.splitlines()
        if not lines or not lines[0].strip():
            return ("", "unknown: git merge-tree printed no tree")
        return (lines[0].strip(), "")
    if res.returncode == 1:
        conflicts = [
            line.split(" in ", 1)[1].strip()
            for line in res.stdout.splitlines()
            if "CONFLICT" in line and " in " in line
        ]
        named = ", ".join(conflicts[:3]) if conflicts else "unnamed paths"
        return ("", f"git cannot merge cleanly: conflict in {named}")
    detail = (res.stderr or res.stdout).strip()[:200]
    return ("", f"unknown: git merge-tree exit {res.returncode}: {detail}")


def _canonical_cli_dir() -> str:
    """The canonical checkout's cli/, whose warm venv backs ``uv run``.

    An extracted tree has no venv, so ``uv run`` inside it fails (the hatch
    build wants LICENSE + NOTICE at the repo root). The tools must therefore
    run from the canonical cli's environment while checking the foreign tree.
    """
    try:
        from fno.paths import resolve_canonical_repo_root

        return str(resolve_canonical_repo_root() / "cli")
    except Exception:  # noqa: BLE001 - a degraded answer still runs the check
        return "cli"


def _git_archive_cli(tree: str, toplevel: str) -> Optional[bytes]:
    """The tar bytes of ``cli/`` in ``tree``, or None. ``_proc.run`` is
    text-mode, which would corrupt the archive, so this shells out directly.
    Runs from the toplevel: the ``cli`` pathspec is cwd-relative, and a probe
    launched from a subdirectory would otherwise find no ``cli`` under it."""
    try:
        proc = subprocess.run(
            ["git", "archive", "--format=tar", tree, "cli"],
            cwd=toplevel,
            capture_output=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return proc.stdout if proc.returncode == 0 else None


_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def _red_reason(combined: str) -> str:
    """The first 3 error lines, paths re-rooted from the extracted ``cli/`` to
    the repo root - the reader of a merge refusal wants the path CI names."""
    errors: list[str] = []
    for raw in combined.splitlines():
        line = _ANSI_RE.sub("", raw.strip())
        if line.startswith("src/"):
            errors.append("cli/" + line)
        if len(errors) >= 3:
            break
    if not errors:
        errors = [line.strip() for line in combined.splitlines() if line.strip()][:3]
    return "; ".join(errors) if errors else "static step failed without diagnosable output"


def static_verdict_for_tree(tree: str, cwd: str) -> tuple[str, str]:
    """``ok`` | ``red`` | ``unknown`` for the repo-wide static step on ``tree``.

    Extracts ``cli/`` from the tree and runs
    ``scripts/ci/check-python-static.sh`` on it with RUFF/MYPY pointing at the
    canonical cli's environment. ``red`` carries the first 3 error lines.
    """
    toplevel = _rev("--show-toplevel", cwd) or cwd
    tmp = tempfile.mkdtemp(prefix="fno-merge-result-")
    try:
        archive = _git_archive_cli(tree, toplevel)
        if archive is None:
            return ("unknown", "could not read cli/ from the merge tree (git archive failed)")
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            try:
                tar.extractall(tmp, filter="data")
            except TypeError:  # Python < 3.12 lacks the filter kwarg
                tar.extractall(tmp)
        cli_tree = os.path.join(tmp, "cli")
        if not os.path.isdir(cli_tree):
            return ("ok", "merge tree carries no cli/ - nothing for the static step to check")
        env = dict(os.environ)
        env["NO_COLOR"] = "1"
        env["RUFF"] = "ruff" if shutil.which("ruff") else f"uv run --project {_canonical_cli_dir()} ruff"
        env["MYPY"] = "mypy" if shutil.which("mypy") else f"uv run --project {_canonical_cli_dir()} mypy"
        try:
            proc = subprocess.run(
                ["bash", os.path.join(toplevel, "scripts", "ci", "check-python-static.sh"), cli_tree],
                capture_output=True,
                text=True,
                timeout=_STATIC_TIMEOUT_S,
                env=env,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            return ("unknown", f"static step did not run to completion: {exc}")
        combined = (proc.stdout or "") + (proc.stderr or "")
        if proc.returncode == 0:
            tail = [line.strip() for line in combined.splitlines() if line.strip()]
            return ("ok", tail[-1] if tail else "static step passed")
        return ("red", _red_reason(combined))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _gh_pr_refs(pr_number: int, cwd: str) -> Optional[tuple[str, str]]:
    """``(baseRefName, headRefOid)`` from ``gh pr view``; None on any failure."""
    res = _probe(
        ["gh", "pr", "view", str(pr_number), "--json", "baseRefName,headRefOid"],
        cwd,
    )
    if res is None or not res.ok:
        return None
    try:
        payload = json.loads(res.stdout)
        base = payload.get("baseRefName")
        head = payload.get("headRefOid")
    except (json.JSONDecodeError, AttributeError):
        return None
    if not isinstance(base, str) or not base or not isinstance(head, str) or not head:
        return None
    return (base, head)


def _fetch_pull_head(pr_number: int, cwd: str) -> str:
    """Fetch the PR head the way GitHub exposes it; the fetched oid, or "".

    GitHub publishes the mergeable head under ``refs/pull/<n>/head``, which no
    branch refspec reaches. ``--no-tags`` everywhere: the probe wants one oid,
    not the remote's tag set.
    """
    fetch = _probe(
        [
            "git",
            "fetch",
            "--no-tags",
            "origin",
            f"refs/pull/{pr_number}/head:refs/fno/merge-result/head",
        ],
        cwd,
    )
    if fetch is None or not fetch.ok:
        return ""
    return _rev("refs/fno/merge-result/head", cwd)


def merge_result_verdict(pr_number: int, cwd: str) -> tuple[str, str]:
    """``(verdict, reason)`` for what merging PR ``pr_number`` would produce.

    ``ok`` when the head already contains its base (CI on the head is the merge
    result), ``red`` when the merge conflicts or its tree fails the static
    step, ``unknown`` when a probe could not answer.
    """
    refs = _gh_pr_refs(pr_number, cwd)
    if refs is None:
        return ("unknown", f"could not read base/head of PR #{pr_number} (gh pr view failed)")
    base, head_oid = refs
    if not _fetch_ref(base, cwd):
        return ("unknown", f"could not fetch base branch '{base}'")
    base_rev = f"origin/{base}"
    if not _rev(base_rev, cwd):
        return ("unknown", f"base branch '{base}' did not resolve after fetch")
    fetched = _fetch_pull_head(pr_number, cwd)
    if fetched != head_oid:
        return (
            "unknown",
            f"PR head moved during the probe (fetched {fetched[:8] or 'nothing'}, "
            f"pr view said {head_oid[:8]})",
        )
    tree, reason = merge_tree(base_rev, fetched, cwd)
    if tree and tree == _rev(f"{fetched}^{{tree}}", cwd):
        return ("ok", f"head already contains {base} - CI on the head is the merge result")
    if not tree:
        if reason.startswith("git cannot merge cleanly"):
            return ("red", reason)
        return ("unknown", reason)
    return static_verdict_for_tree(tree, cwd)


def run_merge_result_check(pr_number: int, cwd: Optional[str] = None) -> int:
    """CLI entry: 0 ok, 3 red, 4 unknown - ``run_base_lineage_check``'s shape."""
    repo = cwd or os.getcwd()
    verdict, reason = merge_result_verdict(pr_number, repo)
    if verdict == "ok":
        sys.stdout.write(f"merge-result: ok - {reason}\n")
        return OK
    if verdict == "red":
        sys.stderr.write(
            f"merge-result: REFUSED - {reason}\n"
            f"  remedy: fno do pr rebase, fix, push, then retry\n"
        )
        return REFUSED_RED
    sys.stderr.write(f"merge-result: unknown - {reason}\n")
    return UNKNOWN
