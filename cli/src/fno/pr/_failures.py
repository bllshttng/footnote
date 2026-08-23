"""Why a red check is red: the failing step, its first error, the steps fail-fast never reached.

A check NAME is not a failure (ruling d-bdb035b6): on 2026-08-22 two PRs both
reported `smoke` red within fifteen minutes for unrelated reasons - PR 1069
stopped at the pytest step, PR 1059 at the lint step with pytest already
green - and a reader who generalized one log to the other shipped a wrong
diagnosis. This module turns a failing check run into the three facts that
separate those cases, all from data the job already carries:

- the failing smoke-runner step, from the `smoke: step failed, stopping
  (fail-fast): <name>` line `fno.test_cmd._execute_steps` prints (no pytest
  or ruff output parsing);
- the first error line of that step's own output block;
- the steps that never ran. Fail-fast makes an unreached step read exactly
  like a passed one, so absence is never evidence of green: at the runner
  level the `smoke: planned:` prologue minus the `smoke: pass|fail` lines
  names them; at the GitHub-job level the steps AFTER the failed one do
  (their recorded conclusion is `skipped`, identical to a condition-skip).

Pure helpers first (log text / steps list in, facts out); `collect_failures`
is the one I/O entry `_status` calls, and it degrades loudly - a check whose
log could not be fetched is reported as unavailable, never silently dropped.
"""

from __future__ import annotations

import json
import re
from typing import Callable, Optional, Sequence

from fno.pr._logs import _job_ref
from fno.pr._proc import run

# `smoke: step failed, stopping (fail-fast): <name>` (test_cmd.py), plus the
# older `stopping fail-fast:` wording the 2026-08-22 incident logs carried.
_STEP_FAILED = re.compile(r"step failed, stopping \(?fail-fast\)?:\s*(.+?)\s*$")
# The runner's per-step completion line: `smoke: pass   12s  <name>`.
_RUNNER_DONE = re.compile(r"^smoke: (?:pass|fail)\s+\d+(?:\.\d+)?s\s+(.+?)\s*$")
# The full-mode prologue, one line per planned step (comma-free by design:
# three registry step names contain a comma of their own).
_RUNNER_PLANNED = re.compile(r"^smoke: planned:\s*(.+?)\s*$")
# CI wraps each step in a workflow-command group; locally it is a banner line.
_GROUP = re.compile(r"^::group::\s*(.+?)\s*$")
_BANNER = re.compile(r"^===\s*(.+?)\s*===$")
# GitHub's raw job logs prefix every line with an ISO timestamp.
_TS = re.compile(r"^\d{4}-\d{2}-\d{2}T[\d:.]+Z?\s*")
# First-error detectors, most specific signal first per LINE (the earliest
# matching line in the block wins, not the strongest pattern). `::` lines are
# workflow commands, never content.
_ERROR_PATTERNS = (
    re.compile(r"\bFAILED\b"),
    re.compile(r"^E\s{2,}"),  # pytest assertion detail
    re.compile(r"\b[EFNW]\d{3}\b"),  # ruff/flake8 code: file.py:52:1: E402 ...
    re.compile(r"\bTraceback \(most recent call last\)"),
    re.compile(r"\berror:", re.IGNORECASE),  # mypy: file.py:12: error: ...
    re.compile(r"\bERROR\b"),
    re.compile(r"\bAssertionError\b"),
)
_ERROR_LINE_CAP = 240

# Cap on failing checks detailed per status read: the rollup of a very red PR
# can carry dozens; five covers every observed incident shape, and the
# truncation is itself reported, never silent.
MAX_DETAILED_FAILURES = 5


def _content(line: str) -> str:
    """A log line with GitHub's timestamp prefix and trailing space removed."""
    return _TS.sub("", line.rstrip("\n")).strip()


def failing_step(log: str) -> Optional[str]:
    """The step name on the first `step failed, stopping (fail-fast)` line.

    Fail-fast breaks at the first failure, so the first match is THE failure;
    `--keep-going` runs (which can print several) are not the merge-gate shape
    and the first is still the one that stopped the canonical job.
    """
    for raw in log.splitlines():
        m = _STEP_FAILED.search(_content(raw))
        if m:
            return m.group(1)
    return None


def first_error(log: str, step: Optional[str]) -> Optional[str]:
    """The first error-looking line of the failing step's own output block.

    The runner prints the step-failed line AFTER the child exits, so the error
    lives BEFORE it, inside the step's `::group::` / `=== name ===` block. A
    log without the marker lines falls back to the whole log before the
    step-failed line: the block boundary is an optimization, never a
    precondition. Nothing matched degrades to the block's last non-empty line
    (the tail is where shells leave the actual error); `None` only when there
    is no output at all.
    """
    lines = [_content(ln) for ln in log.splitlines()]
    end = next((i for i, ln in enumerate(lines) if _STEP_FAILED.search(ln)), len(lines))
    start = 0
    if step:
        for i in range(end - 1, -1, -1):
            g = _GROUP.match(lines[i]) or _BANNER.match(lines[i])
            if g and g.group(1) == step:
                start = i + 1
                break
    block = [
        ln
        for ln in lines[start:end]
        if ln and not ln.startswith("::") and not ln.startswith("smoke: ")
    ]
    for line in block:
        if any(p.search(line) for p in _ERROR_PATTERNS):
            return line[:_ERROR_LINE_CAP]
    return block[-1][:_ERROR_LINE_CAP] if block else None


def unreached_runner_steps(log: str) -> Optional[list[str]]:
    """Planned smoke steps with no completion line: fail-fast never ran them.

    `None` on a log with no `smoke: planned:` prologue (written before that
    line existed): an old log's step list cannot be recovered, and an honest
    "unknown" beats a fabricated empty list that would read "nothing was
    unreached".
    """
    planned = [
        m.group(1)
        for m in map(_RUNNER_PLANNED.match, (_content(ln) for ln in log.splitlines()))
        if m
    ]
    if not planned:
        return None
    done = {
        m.group(1) for m in map(_RUNNER_DONE.match, (_content(ln) for ln in log.splitlines())) if m
    }
    return [name for name in planned if name not in done]


def unreached_job_steps(steps: Sequence[dict]) -> list[str]:
    """GitHub-job steps after the failed one, minus cleanup bookkeeping.

    Measured on a real failed job: steps after the failure record
    `conclusion: "skipped"`, byte-identical to a condition-skip, so position
    after the failure is the only discriminator. `Post *` and `Complete job`
    are GitHub's own cleanup phases and always trail the failure; they are
    never work that fail-fast hid. No failed step (a cancelled or
    startup-failure job) yields `[]`: with no failure point, position says
    nothing.
    """
    failed_at = next(
        (i for i, s in enumerate(steps) if str(s.get("conclusion") or "").lower() == "failure"),
        None,
    )
    if failed_at is None:
        return []
    out = []
    for s in steps[failed_at + 1 :]:
        name = str(s.get("name") or "")
        if not name or name.startswith("Post ") or name == "Complete job":
            continue
        out.append(name)
    return out


def _check_name(check: dict) -> str:
    return str(check.get("name") or check.get("context") or "(unnamed check)")


def collect_failures(
    failing: Sequence[dict],
    cwd: Optional[str] = None,
    runner: Callable = run,
) -> list[dict]:
    """Detail entries for the failing rollup rows, loudest facts first.

    Per check: two REST reads (the job object for `steps[]`, its log text).
    A row that is not an Actions job (a commit StatusContext, e.g.
    `stacked-base-guard`) is named with no log claim; a fetch failure names
    its class rather than vanishing - an omitted check reads as passed, which
    is the exact lie this module exists to stop. Capped at
    MAX_DETAILED_FAILURES with an explicit truncation entry.
    """
    out: list[dict] = []
    for check in list(failing)[:MAX_DETAILED_FAILURES]:
        entry: dict = {"check": _check_name(check)}
        ref = _job_ref(check)
        if ref is None:
            entry["detail"] = "not an Actions job (commit status); no job log to read"
            out.append(entry)
            continue
        owner, repo, job_id = ref
        entry["job_id"] = job_id
        steps: Sequence[dict] = []
        job = runner(["gh", "api", f"repos/{owner}/{repo}/actions/jobs/{job_id}"], cwd=cwd)
        if job.ok:
            try:
                steps = json.loads(job.stdout).get("steps") or []
            except json.JSONDecodeError:
                steps = []
        log_res = runner(["gh", "api", f"repos/{owner}/{repo}/actions/jobs/{job_id}/logs"], cwd=cwd)
        log_text = log_res.stdout if log_res.ok else ""
        if not log_res.ok:
            entry["detail"] = f"log unavailable: {(log_res.stderr or 'gh error').strip()[:160]}"
        if log_text:
            step = failing_step(log_text)
            if step:
                entry["step"] = step
                err = first_error(log_text, step)
                if err:
                    entry["first_error"] = err
            runner_unreached = unreached_runner_steps(log_text)
            if runner_unreached:
                entry["unreached_steps"] = runner_unreached
            elif not step and steps:
                # No runner lines in this log: it is a plain multi-step job,
                # so the GitHub steps after the failed one are the unreached
                # work. (A smoke shard's wrapper step names no runner step,
                # and its job-level later steps are only cleanup - which
                # unreached_job_steps already excludes.)
                job_unreached = unreached_job_steps(steps)
                if job_unreached:
                    entry["unreached_steps"] = job_unreached
                failed_job_step = next(
                    (
                        str(s.get("name"))
                        for s in steps
                        if str(s.get("conclusion") or "").lower() == "failure"
                    ),
                    None,
                )
                if failed_job_step:
                    entry.setdefault("step", failed_job_step)
        elif steps:
            job_unreached = unreached_job_steps(steps)
            if job_unreached:
                entry["unreached_steps"] = job_unreached
        out.append(entry)
    if len(failing) > MAX_DETAILED_FAILURES:
        out.append(
            {
                "check": f"({len(failing) - MAX_DETAILED_FAILURES} more failing check(s) not detailed)",
            }
        )
    return out
