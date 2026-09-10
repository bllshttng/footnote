"""Compile the merge result before the merge lands.

Two green parents can merge red when git joins hunks that never met on one
machine: PR #1654 landed as 3334b826a133 and broke main with F821 at
cli/src/fno/graph/store.py:525 while both parents were green. This module
resolves the PR's base/head, skips the run when the head already contains its
base (CI on the head IS the merge result), and maps the exit vocabulary of
scripts/ci/check-merge-result.sh (0 ok, 3 red, 4 unknown). See
docs/architecture/authorized-merge.md, decision step 8.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from typing import Optional

from fno.pr._base_lineage import _fetch_ref, _probe, _rev

OK = 0
REFUSED_RED = 3
UNKNOWN = 4


def _canonical_cli_dir() -> str:
    try:
        from fno.paths import resolve_canonical_repo_root

        return str(resolve_canonical_repo_root() / "cli")
    except Exception:  # noqa: BLE001 - a degraded answer still runs the check
        return "cli"


def _gh_pr_refs(pr_number: int, cwd: str) -> Optional[tuple[str, str]]:
    res = _probe(["gh", "pr", "view", str(pr_number), "--json", "baseRefName,headRefOid"], cwd)
    if res is None or not res.ok:
        return None
    try:
        payload = json.loads(res.stdout)
        base, head = payload.get("baseRefName"), payload.get("headRefOid")
    except (json.JSONDecodeError, AttributeError):
        return None
    if not isinstance(base, str) or not base or not isinstance(head, str) or not head:
        return None
    return (base, head)


def _fetch_pull_head(pr_number: int, cwd: str) -> str:
    """GitHub publishes the mergeable head under refs/pull/<n>/head, which no
    branch refspec reaches; "" when the fetch or the rev-parse fails."""
    fetch = _probe(
        ["git", "fetch", "--no-tags", "origin", f"refs/pull/{pr_number}/head:refs/fno/merge-result/head"],
        cwd,
    )
    return _rev("refs/fno/merge-result/head", cwd) if fetch is not None and fetch.ok else ""


def _run_script(top: str, base_rev: str, head_oid: str, cwd: str) -> tuple[str, str]:
    """Drive scripts/ci/check-merge-result.sh; RUFF/MYPY point at the canonical
    cli's uv environment, because an extracted tree has no venv of its own."""
    env = dict(os.environ)
    if not shutil.which("ruff"):
        env["RUFF"] = f"uv run --project {_canonical_cli_dir()} ruff"
    if not shutil.which("mypy"):
        env["MYPY"] = f"uv run --project {_canonical_cli_dir()} mypy"
    try:
        proc = subprocess.run(
            ["bash", os.path.join(top, "scripts", "ci", "check-merge-result.sh"), top, base_rev, head_oid],
            capture_output=True, text=True, timeout=180, env=env, cwd=cwd, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return ("unknown", f"merge-result probe did not run to completion: {exc}")
    output = (proc.stdout or "").strip()
    if proc.returncode == 0:
        return ("ok", output or "merge result is statically green")
    if proc.returncode == 3:
        return ("red", output.replace("merge-result: red - ", "", 1) or output)
    detail = ((proc.stderr or "") + output).strip() or f"exit {proc.returncode}"
    return ("unknown", f"merge-result probe failed: {detail[:200]}")


def merge_result_verdict(pr_number: int, cwd: str) -> tuple[str, str]:
    """``(verdict, reason)``: ok | red | unknown for what merging PR ``pr_number``
    would produce. The static step never runs when the head already contains
    its base."""
    refs = _gh_pr_refs(pr_number, cwd)
    if refs is None:
        return ("unknown", f"could not read base/head of PR #{pr_number} (gh pr view failed)")
    base, head_oid = refs
    if not _fetch_ref(base, cwd):
        return ("unknown", f"could not fetch base branch '{base}'")
    base_rev = f"origin/{base}"
    if not _rev(base_rev, cwd):
        return ("unknown", f"base branch '{base}' did not resolve after fetch")
    contains = _probe(["git", "merge-base", "--is-ancestor", base_rev, head_oid], cwd)
    if contains is not None and contains.ok:
        return ("ok", f"head already contains {base} - CI on the head is the merge result")
    fetched = _fetch_pull_head(pr_number, cwd)
    if fetched != head_oid:
        return (
            "unknown",
            f"PR head moved during the probe (fetched {fetched[:8] or 'nothing'}, pr view said {head_oid[:8]})",
        )
    top = _rev("--show-toplevel", cwd) or cwd
    return _run_script(top, base_rev, fetched, cwd)


def run_merge_result_check(pr_number: int, cwd: Optional[str] = None) -> int:
    """CLI entry: 0 ok, 3 red, 4 unknown - ``run_base_lineage_check``'s shape."""
    verdict, reason = merge_result_verdict(pr_number, cwd or os.getcwd())
    if verdict == "ok":
        sys.stdout.write(f"merge-result: ok - {reason}\n")
        return OK
    if verdict == "red":
        sys.stderr.write(
            f"merge-result: REFUSED - {reason}\n  remedy: fno do pr rebase, fix, push, then retry\n"
        )
        return REFUSED_RED
    sys.stderr.write(f"merge-result: unknown - {reason}\n")
    return UNKNOWN
