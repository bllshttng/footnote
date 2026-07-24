"""`fno pr logs [<pr>]` - why did CI fail, in 40 lines.

`fno pr` could merge, verify, rebase and report a verdict, but not answer the
one question an agent actually asks after a red check. With no verb for it the
agent reached past footnote to `gh run view --log`, which downloads a zip of
EVERY job in the run and pastes the whole thing into a transcript that then
re-reads it on every later request.

The output contract is `fno test`'s, so there is one convention rather than
two: spool everything to a log file, print a bounded tail plus the path, and
give a human one escape flag (`--full`).

Two shapes make this cheap. `statusCheckRollup[].detailsUrl` already ends in
`/actions/runs/<run>/job/<job>`, so the failing job's id needs no second
lookup; and `gh api .../actions/jobs/<id>/logs` returns that ONE job's log
rather than the run-wide archive.

Exit codes are `fno pr status`'s alphabet, so a caller branches on `$?`:
    0  green    - every check passed; nothing fetched, no log written
    1  red      - at least one check failed; its log is spooled and tailed
    2  pending  - a check is still running; neither pass nor fail
    3  unknown  - the PR has no checks
    4  error    - could not read CI state (auth, rate limit, no PR, bad JSON)
    127 gh missing

The load-bearing invariant is that 0 is reachable ONLY from a rollup that
parsed. A reader that says "all green" when it cannot see is the same false
green it exists to replace.
"""
from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path
from typing import Optional, Sequence

from fno.pr._proc import Result, ToolMissing, run
from fno.pr._status import _classify, _latest_per_name

_TAIL_LINES = 40
_LOG_NAME = "last-ci.log"

# .../actions/runs/<run_id>/job/<job_id>[?...] on any GitHub host.
_JOB_URL = re.compile(r"^https?://[^/]+/([^/]+)/([^/]+)/actions/runs/\d+/job/(\d+)")


def _check_name(check: dict) -> str:
    """A CheckRun's `name` or a StatusContext's `context`; never empty."""
    return str(check.get("name") or check.get("context") or "(unnamed check)")


def _gh_failure_reason(res: Result) -> str:
    """Name WHY gh failed, so the caller never reads a bare non-zero exit.

    Each branch is a distinct operator action (log in, wait, check the PR
    number), which is the whole value over echoing gh's stderr verbatim.
    """
    blob = (res.stderr + res.stdout).lower()
    if "gh auth login" in blob or "401" in blob or "authentication" in blob:
        return "authentication (run `gh auth login`)"
    if "rate limit" in blob or "429" in blob or "403" in blob:
        return "rate limit or forbidden"
    if "410" in blob or "gone" in blob or "expired" in blob:
        return "log expired (past GitHub's retention window)"
    if "no pull requests found" in blob or "404" in blob or "not found" in blob:
        return "no such PR, or no PR for the current branch"
    return "gh error"


def _fail(reason: str, res: Optional[Result] = None) -> int:
    sys.stderr.write(f"fno pr logs: cannot read CI state: {reason}\n")
    if res is not None:
        tail = (res.stderr or res.stdout).strip().splitlines()[-3:]
        for line in tail:
            sys.stderr.write(f"  gh: {line}\n")
    return 4


def _fetch_rollup(pr: Optional[str], cwd: Optional[str]) -> tuple[Optional[list], int]:
    """Return (rollup, 0) or (None, exit_code). No PR argument reads the branch."""
    cmd = ["gh", "pr", "view"]
    if pr:
        cmd.append(pr)
    cmd += ["--json", "statusCheckRollup"]
    res = run(cmd, cwd=cwd)
    if not res.ok:
        return None, _fail(_gh_failure_reason(res), res)
    if not res.stdout.strip():
        return None, _fail("gh returned no PR data")
    try:
        return (json.loads(res.stdout).get("statusCheckRollup") or []), 0
    except json.JSONDecodeError:
        return None, _fail("gh returned unparseable JSON")


def _job_ref(check: dict) -> Optional[tuple[str, str, str]]:
    """(owner, repo, job_id) from a CheckRun's detailsUrl, else None.

    A StatusContext (an external status, not an Actions job) carries a
    targetUrl that no jobs API can serve; the caller reports the URL instead
    of pretending it can fetch a log.
    """
    url = str(check.get("detailsUrl") or check.get("targetUrl") or "")
    m = _JOB_URL.match(url)
    return (m.group(1), m.group(2), m.group(3)) if m else None


def _spool(root: Path, text: str) -> Optional[Path]:
    """Write `text` to <root>/.fno/last-ci.log via temp+rename.

    The rename is what lets two agents spool concurrently without a reader
    ever seeing a half-written log. Returns None on a write failure, which the
    caller reports rather than discarding the log it just paid to fetch.
    """
    d = root / ".fno"
    dest = d / _LOG_NAME
    tmp = d / f".{_LOG_NAME}.{os.getpid()}.tmp"
    try:
        d.mkdir(parents=True, exist_ok=True)
        tmp.write_text(text, encoding="utf-8")
        os.replace(tmp, dest)
    except OSError as exc:
        sys.stderr.write(f"fno pr logs: fetched the log but could not write {dest}: {exc}\n")
        tmp.unlink(missing_ok=True)
        return None
    return dest


def run_logs(
    pr: Optional[str] = None,
    *,
    job: Optional[str] = None,
    lines: int = _TAIL_LINES,
    full: bool = False,
    cwd: Optional[str] = None,
    root: Optional[Path] = None,
) -> int:
    from fno import paths

    rollup, code = _fetch_rollup(pr, cwd)
    if rollup is None:
        return code

    deduped = _latest_per_name(rollup)
    if not deduped:
        print("no checks on this PR")
        return 3

    failing = [c for c in deduped if _classify(c) == "fail"]
    pending = [c for c in deduped if _classify(c) == "pending"]

    if not failing:
        if pending:
            print(f"{len(pending)} check(s) still running, none failed yet:")
            for c in pending:
                print(f"  pending: {_check_name(c)}")
            return 2
        print(f"all {len(deduped)} checks green")
        return 0

    names = [_check_name(c) for c in failing]
    print(f"{len(failing)} failing check(s): {', '.join(names)}")

    target = failing[0]
    if job:
        matched = [c for c in failing if _check_name(c) == job]
        if not matched:
            matched = [c for c in failing if job.lower() in _check_name(c).lower()]
        if not matched:
            sys.stderr.write(
                f"fno pr logs: no failing check matches --job {job!r}; "
                f"failing: {', '.join(names)}\n"
            )
            return 1
        target = matched[0]

    ref = _job_ref(target)
    if ref is None:
        url = target.get("detailsUrl") or target.get("targetUrl") or "(no url)"
        print(f"{_check_name(target)} is not a GitHub Actions job; see {url}")
        return 1

    owner, repo, job_id = ref
    print(f"fetching: {_check_name(target)} (job {job_id})", flush=True)
    res = run(["gh", "api", f"repos/{owner}/{repo}/actions/jobs/{job_id}/logs"], cwd=cwd)
    if not res.ok:
        sys.stderr.write(
            f"fno pr logs: could not fetch the log for {_check_name(target)}: "
            f"{_gh_failure_reason(res)}\n"
        )
        return 1

    dest = _spool(root or paths.resolve_repo_root(), res.stdout)
    if dest is None:
        return 1

    log_lines = res.stdout.splitlines(keepends=True)
    if full:
        sys.stdout.write(res.stdout)
        print(f"\nfull log: {dest}")
        return 1

    shown = log_lines[-lines:] if lines > 0 else []
    print(
        f"last {len(shown)} lines of {_check_name(target)} - read from the end, "
        f"expand upward if needed: tail -200 {dest}"
    )
    sys.stdout.write("".join(shown))
    if shown and not shown[-1].endswith("\n"):
        sys.stdout.write("\n")
    return 1


def main(argv: Sequence[str]) -> int:
    try:
        return run_logs(str(argv[0]) if argv else None)
    except ToolMissing:
        sys.stderr.write("fno pr logs: gh not found on PATH\n")
        return 127
