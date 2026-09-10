"""Compile the merge result before the merge lands.

Two green parents can merge red when git joins hunks that never met (PR #1654:
3334b826a133, F821 at store.py:525, both parents green). The merge-tree and
the static step live in scripts/ci/check-merge-result.sh.
"""
import json
import os
import shutil
import subprocess
import sys

from fno.pr._base_lineage import _fetch_ref, _probe, _rev

OK, REFUSED_RED, UNKNOWN = 0, 3, 4


def _gh_pr_refs(pr_number: int, cwd: str) -> tuple[str, str] | None:
    res = _probe(["gh", "pr", "view", str(pr_number), "--json", "baseRefName,headRefOid"], cwd)
    try:
        payload = json.loads(res.stdout) if res is not None and res.ok else {}
        refs = (payload["baseRefName"], payload["headRefOid"])
        return refs if all(isinstance(v, str) and v for v in refs) else None
    except (json.JSONDecodeError, AttributeError, KeyError, TypeError):
        return None


def _fetch_pull_head(pr_number: int, cwd: str) -> str:
    ref = f"refs/pull/{pr_number}/head:refs/fno/merge-result/head"
    fetch = _probe(["git", "fetch", "--no-tags", "origin", ref], cwd)
    return _rev("refs/fno/merge-result/head", cwd) if fetch is not None and fetch.ok else ""


def _run_script(top: str, base_rev: str, head_oid: str, cwd: str) -> tuple[str, str]:
    try:
        from fno.paths import resolve_canonical_repo_root

        cli = str(resolve_canonical_repo_root() / "cli")
    except Exception:  # noqa: BLE001
        cli = "cli"
    env = dict(os.environ)
    env["RUFF"] = "ruff" if shutil.which("ruff") else f"uv run --project {cli} ruff"
    env["MYPY"] = "mypy" if shutil.which("mypy") else f"uv run --project {cli} mypy"
    script = os.path.join(top, "scripts", "ci", "check-merge-result.sh")
    try:
        proc = subprocess.run(
            ["bash", script, top, base_rev, head_oid],
            capture_output=True, text=True, timeout=180, env=env, cwd=cwd, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return ("unknown", f"merge-result probe did not run: {exc}")
    out = (proc.stdout or "").strip()
    if proc.returncode == 0:
        return ("ok", out or "merge result is statically green")
    if proc.returncode == 3:
        return ("red", out.replace("merge-result: red - ", "", 1) or out)
    detail = ((proc.stderr or "") + out).strip() or f"exit {proc.returncode}"
    return ("unknown", f"merge-result probe failed: {detail[:200]}")


def merge_result_verdict(pr_number: int, cwd: str) -> tuple[str, str]:
    refs = _gh_pr_refs(pr_number, cwd)
    if refs is None:
        return ("unknown", f"could not read base/head of PR #{pr_number} (gh pr view failed)")
    base, head_oid = refs
    if not _fetch_ref(base, cwd):
        return ("unknown", f"could not fetch base branch '{base}'")
    base_rev = f"origin/{base}"
    contains = _probe(["git", "merge-base", "--is-ancestor", base_rev, head_oid], cwd)
    if contains is not None and contains.ok:
        return ("ok", f"head already contains {base} - CI on the head is the merge result")
    fetched = _fetch_pull_head(pr_number, cwd)
    if fetched != head_oid:
        return ("unknown", f"PR head moved during the probe ({fetched[:8] or 'nothing'} != {head_oid[:8]})")
    return _run_script(_rev("--show-toplevel", cwd) or cwd, base_rev, fetched, cwd)


def run_merge_result_check(pr_number: int, cwd: str | None = None) -> int:
    verdict, reason = merge_result_verdict(pr_number, cwd or os.getcwd())
    if verdict == "ok":
        sys.stdout.write(f"merge-result: ok - {reason}\n")
        return OK
    if verdict == "red":
        sys.stderr.write(f"merge-result: REFUSED - {reason}\n  remedy: rebase, fix, push, retry\n")
        return REFUSED_RED
    sys.stderr.write(f"merge-result: unknown - {reason}\n")
    return UNKNOWN
