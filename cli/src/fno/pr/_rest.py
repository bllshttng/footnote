"""REST reads behind `fno pr status`.

The GraphQL quota is per-USER and shared by every session on the machine, so
the documented idle behaviour (watchers, stop-hook reads) exhausted it while
the core REST budget sat untouched. This module answers the settledness
question on REST: `pulls/{n}` for state + head sha, `commits/{sha}/check-runs`
for CheckRuns, `commits/{sha}/status` for legacy StatusContexts. The entries
are mapped onto the rollup shape `_status.verdict_for` already classifies, so
verdict semantics (latest-per-name dedup, pending-never-red, tie fail-closed)
are byte-identical to the GraphQL path.

REST is not a free lane: a 403 SECONDARY rate limit fires on request rate,
not budget, and was measured with the core bucket at 0/5000 used. A failure
here is loud - `(None, reason)` -> `verdict: error, settled: false` - and the
reason names a secondary limit so a caller backs off instead of retrying on
an interval. A quiet REST port would be worse than the GraphQL exhaustion it
replaces.
"""
from __future__ import annotations

import json
import re
from datetime import datetime, timezone
from typing import Any, Callable, Optional

from fno.pr._proc import run
from fno.pr._ritual import _parse_origin_slug

# GitHub's secondary (request-rate) limit says "You have exceeded a secondary
# rate limit". The primary REST limit is also an HTTP 403 but says only "API
# rate limit exceeded" - the word "secondary" is the discriminator, so a 403
# without it must fall through to the core-bucket branch.
_SECONDARY = re.compile(r"secondary rate limit", re.IGNORECASE)


def _repo_slug(cwd: Optional[str], runner: Callable = run) -> Optional[str]:
    """`owner/repo` from the git origin remote: local, no API spend."""
    try:
        r = runner(["git", "remote", "get-url", "origin"], cwd=cwd)
    except Exception:
        return None
    if not r.ok:
        return None
    return _parse_origin_slug(r.stdout.strip())


def _rest_reason(res) -> str:
    """Explain a failed REST read, naming the failure class that changes what
    the caller should do next (back off vs. wait for reset vs. transient)."""
    lines = [ln.strip() for ln in (getattr(res, "stderr", "") or "").splitlines() if ln.strip()]
    text = " ".join(lines)
    if _SECONDARY.search(text):
        base = lines[0] if lines else "gh api failed with no message"
        return (
            base
            + " | this is the SECONDARY rate limit (request rate, not budget:"
            " the core bucket can read 5000 remaining here). Back off - retrying"
            " on a fixed interval sustains the refusal."
        )
    if "rate limit" in text.lower():
        base = lines[0] if lines else "gh api failed with no message"
        return (
            base
            + " | this is the CORE REST quota. Check"
            " `gh api rate_limit --jq .resources.core` and wait for its reset."
        )
    return lines[0] if lines else "gh api failed with no message"


def _map_pr_state(data: dict) -> str:
    """REST `state` (open/closed + `merged`) -> the OPEN/CLOSED/MERGED shape
    the GraphQL read emitted, so `pr_state` in the output is unchanged."""
    if data.get("merged"):
        return "MERGED"
    return {"open": "OPEN", "closed": "CLOSED"}.get(str(data.get("state") or "").lower(), "UNKNOWN")


def fetch_pr_rest(
    pr: str,
    cwd: Optional[str] = None,
    runner: Callable = run,
) -> "tuple[Optional[dict], str]":
    """Same contract as the old GraphQL `_fetch`: `(pr_json, reason)`.

    `pr_json` carries `state`, `statusCheckRollup`, `headRefOid` - the keys
    `run_status` reads. `reason` is empty on success and names the failure
    class otherwise; `(None, reason)` must reach the caller as a loud
    `verdict: error`, never as an absent answer.
    """
    if not str(pr).strip().isdigit():
        return None, f"REST reader needs a numeric PR number, got {pr!r}"
    slug = _repo_slug(cwd, runner)
    if not slug:
        return None, "could not resolve owner/repo from `git remote get-url origin`"

    pulls = runner(["gh", "api", f"repos/{slug}/pulls/{pr}"], cwd=cwd)
    if not pulls.ok:
        return None, _rest_reason(pulls)
    try:
        pr_data = json.loads(pulls.stdout)
    except json.JSONDecodeError:
        return None, "gh api pulls/<n> returned output that is not JSON"
    sha = str((pr_data.get("head") or {}).get("sha") or "")
    if not sha:
        return None, "gh api pulls/<n> carried no head sha"

    rollup: list[dict[str, Any]] = []
    # Paginate past the first 100: a commit with more check runs than one page
    # would silently drop the tail, and a failing check on page 2 then reads
    # as green. total_count (the endpoint's own count) bounds the loop; a
    # runner that omits it (fakes, older payloads) stops after page 1.
    page = 1
    check_runs: list[dict[str, Any]] = []
    while True:
        checks = runner(
            ["gh", "api",
             f"repos/{slug}/commits/{sha}/check-runs?per_page=100&page={page}"],
            cwd=cwd,
        )
        if not checks.ok:
            return None, _rest_reason(checks)
        try:
            payload = json.loads(checks.stdout)
            check_runs.extend(payload.get("check_runs") or [])
            total = payload.get("total_count")
        except json.JSONDecodeError:
            return None, "gh api check-runs returned output that is not JSON"
        if not isinstance(total, int) or len(check_runs) >= total or not page < 10:
            break
        page += 1
    for cr in check_runs:
        # `_classify`/`_entry_ts` uppercase and alt-chain internally, so the
        # lowercase REST enum values and the started_at mapping need no case
        # work here.
        rollup.append(
            {
                "name": cr.get("name"),
                "status": cr.get("status") or "",
                "conclusion": cr.get("conclusion") or "",
                "startedAt": cr.get("started_at") or "",
            }
        )

    # Legacy StatusContexts ride the combined-status endpoint. Failure here is
    # tolerated the same way an empty rollup section was on GraphQL: no
    # contexts reported rather than a loud error for a check-set the repo does
    # not use. CheckRuns above stay authoritative for CI settledness. The one
    # exception: zero CheckRuns AND a failed statuses read is NOT "no checks"
    # - a legacy-status-only repo would read verdict `unknown` while its real
    # verdict went unread, so that combination stays loud.
    statuses = runner(["gh", "api", f"repos/{slug}/commits/{sha}/status"], cwd=cwd)
    if not statuses.ok and not check_runs:
        return None, _rest_reason(statuses)
    if statuses.ok:
        try:
            for sc in json.loads(statuses.stdout).get("statuses") or []:
                rollup.append(
                    {
                        "context": sc.get("context"),
                        "state": (sc.get("state") or "").upper(),
                        "createdAt": sc.get("created_at") or "",
                    }
                )
        except json.JSONDecodeError:
            pass

    return (
        {"state": _map_pr_state(pr_data), "statusCheckRollup": rollup, "headRefOid": sha},
        "",
    )


def list_prs_rest(
    slug: str,
    *,
    state: str = "open",
    runner: Callable = run,
    cwd: Optional[str] = None,
    per_page: int = 100,
    max_pages: int = 20,
    timeout: Optional[float] = 30.0,
) -> "tuple[Optional[list[dict]], str]":
    """List a repo's PRs on REST: `(rows, reason)`.

    Measured with the GraphQL bucket at 0/5000, this endpoint answered the
    open-PR question completely and left core untouched, which is why the
    pr-watch bulk sweep routes here instead of `gh pr list` (GraphQL bills
    by point cost; one oversized sweep drained the shared per-user budget).
    Paginates on `page=` until a short page or the `max_pages` ceiling; rows
    are reduced to ``{"number", "state"}`` with ``_map_pr_state``, so the
    caller sees the OPEN/CLOSED/MERGED shape the GraphQL path produced.
    `(None, reason)` on any failure, matching `fetch_pr_rest`'s loud contract.
    """
    rows: list[dict[str, Any]] = []
    for page in range(1, max_pages + 1):
        res = runner(
            ["gh", "api", f"repos/{slug}/pulls?state={state}&per_page={per_page}&page={page}"],
            cwd=cwd,
            timeout=timeout,
        )
        if not res.ok:
            return None, _rest_reason(res)
        try:
            payload = json.loads(res.stdout)
        except json.JSONDecodeError:
            return None, f"gh api pulls list page {page} returned output that is not JSON"
        if not isinstance(payload, list):
            return None, f"gh api pulls list page {page} was not a JSON array"
        for row in payload:
            if not isinstance(row, dict):
                continue
            number = row.get("number")
            if not isinstance(number, int):
                continue
            rows.append({"number": number, "state": _map_pr_state(row)})
        if len(payload) < per_page:
            break
    return rows, ""


def graphql_remaining(
    runner: Callable = run, cwd: Optional[str] = None
) -> "tuple[Optional[int], Optional[str]]":
    """Read the shared GraphQL budget: `(remaining, reset_iso)`.

    `gh api rate_limit` does not itself count against any bucket. Both values
    are None on any failure, and a caller MUST read None as unknown, never as
    zero: skipping work because the instrument is unreadable is an absence
    read as evidence.
    """
    try:
        res = runner(["gh", "api", "rate_limit"], cwd=cwd)
    except Exception:  # noqa: BLE001 - unreadable instrument, never a skip (AC6)
        return None, None
    if not res.ok:
        return None, None
    try:
        graphql = json.loads(res.stdout)["resources"]["graphql"]
        remaining = graphql["remaining"]
        reset_epoch = graphql["reset"]
    except (json.JSONDecodeError, KeyError, TypeError):
        return None, None
    if not isinstance(remaining, int) or not isinstance(reset_epoch, (int, float)):
        return None, None
    reset_iso = datetime.fromtimestamp(reset_epoch, tz=timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    return remaining, reset_iso
