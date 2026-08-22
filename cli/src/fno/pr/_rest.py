"""REST reads behind `fno do pr status`.

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
import logging
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

log = logging.getLogger(__name__)


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


def _map_mergeable(value: Any) -> str:
    if value is True:
        return "MERGEABLE"
    if value is False:
        return "CONFLICTING"
    return "UNKNOWN"


def fetch_pr_info_rest(
    pr: str,
    cwd: Optional[str] = None,
    runner: Callable = run,
    repo: Optional[str] = None,
) -> "tuple[Optional[dict], str]":
    """Fetch state, head, base, and mergeability with one REST request."""
    if not str(pr).strip().isdigit():
        return None, f"REST reader needs a numeric PR number, got {pr!r}"
    slug = repo or _repo_slug(cwd, runner)
    if not slug:
        return None, "could not resolve owner/repo from `git remote get-url origin`"

    pulls = runner(["gh", "api", f"repos/{slug}/pulls/{pr}"], cwd=cwd)
    if not pulls.ok:
        return None, _rest_reason(pulls)
    try:
        pr_data = json.loads(pulls.stdout)
    except json.JSONDecodeError:
        return None, "gh api pulls/<n> returned output that is not JSON"
    if not isinstance(pr_data, dict):
        return None, "gh api pulls/<n> returned a JSON value that is not an object"

    head = pr_data.get("head")
    base = pr_data.get("base")
    if not isinstance(head, dict) or not isinstance(base, dict):
        return None, "gh api pulls/<n> carried malformed head/base objects"
    sha = head.get("sha")
    head_ref = head.get("ref")
    base_ref = base.get("ref")
    if not isinstance(sha, str) or not sha:
        return None, "gh api pulls/<n> carried no head sha"
    if not isinstance(head_ref, str) or not isinstance(base_ref, str):
        return None, "gh api pulls/<n> carried malformed head/base refs"
    merged_at = pr_data.get("merged_at")
    if merged_at is not None and not isinstance(merged_at, str):
        return None, "gh api pulls/<n> carried malformed merged_at"
    url = pr_data.get("html_url")
    if url is not None and (not isinstance(url, str) or not url):
        return None, "gh api pulls/<n> carried malformed HTML URL"
    return (
        {
            "pr": int(pr),
            "url": url,
            "state": _map_pr_state(pr_data),
            "head_sha": sha,
            "head_ref": head_ref,
            "base_ref": base_ref,
            "mergeable": _map_mergeable(pr_data.get("mergeable")),
            "merged_at": merged_at,
        },
        "",
    )


def resolve_current_pr_number_rest(
    *, cwd: Optional[str] = None, runner: Callable = run, repo: Optional[str] = None
) -> "tuple[Optional[int], str]":
    """Resolve the current branch's PR with the REST pulls-list endpoint."""
    slug = repo or _repo_slug(cwd, runner)
    if not slug or "/" not in slug:
        return None, "could not resolve owner/repo from `git remote get-url origin`"
    branch = runner(["git", "branch", "--show-current"], cwd=cwd)
    if not branch.ok or not branch.stdout.strip():
        return None, "could not resolve the current git branch"
    owner = slug.split("/", 1)[0]
    rows = runner(
        [
            "gh", "api",
            f"repos/{slug}/pulls?state=all&head={owner}:{branch.stdout.strip()}&per_page=2",
        ],
        cwd=cwd,
    )
    if not rows.ok:
        return None, _rest_reason(rows)
    try:
        payload = json.loads(rows.stdout)
    except json.JSONDecodeError:
        return None, "gh api pulls list returned output that is not JSON"
    if not isinstance(payload, list) or not payload:
        return None, f"no pull requests found for branch {branch.stdout.strip()}"
    number = payload[0].get("number") if isinstance(payload[0], dict) else None
    if not isinstance(number, int):
        return None, "gh api pulls list carried no PR number"
    return number, ""


def fetch_pr_rest(
    pr: str,
    cwd: Optional[str] = None,
    runner: Callable = run,
) -> "tuple[Optional[dict], str]":
    """Same contract as the old GraphQL `_fetch`: `(pr_json, reason)`.

    `pr_json` carries `state`, `statusCheckRollup`, `headRefOid`, `headRefName`,
    `mergeable` - the keys `run_status` reads. `reason` is empty on success and names the
    failure class otherwise; `(None, reason)` must reach the caller as a loud
    `verdict: error`, never as an absent answer.
    """
    slug = _repo_slug(cwd, runner)
    if not slug:
        return None, "could not resolve owner/repo from `git remote get-url origin`"
    info, reason = fetch_pr_info_rest(pr, cwd=cwd, runner=runner, repo=slug)
    if info is None:
        return None, reason
    sha = info["head_sha"]

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
            if not isinstance(payload, dict):
                return None, "gh api check-runs returned a JSON value that is not an object"
            page_runs = payload.get("check_runs")
            if not isinstance(page_runs, list) or not all(isinstance(row, dict) for row in page_runs):
                return None, "gh api check-runs carried malformed check_runs"
            check_runs.extend(page_runs)
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

    # Legacy StatusContexts ride the combined-status endpoint. This read is a
    # separate check class, so failure is always loud: green CheckRuns do not
    # prove an unread required legacy context is green.
    statuses = runner(["gh", "api", f"repos/{slug}/commits/{sha}/status"], cwd=cwd)
    if not statuses.ok:
        return None, _rest_reason(statuses)
    if statuses.ok:
        try:
            status_payload = json.loads(statuses.stdout)
            if not isinstance(status_payload, dict):
                return None, "gh api status returned a JSON value that is not an object"
            status_rows = status_payload.get("statuses")
            if not isinstance(status_rows, list) or not all(
                isinstance(row, dict) for row in status_rows
            ):
                return None, "gh api status carried malformed statuses"
            for sc in status_rows:
                rollup.append(
                    {
                        "context": sc.get("context"),
                        "state": (sc.get("state") or "").upper(),
                        "createdAt": sc.get("created_at") or "",
                    }
                )
        except json.JSONDecodeError:
            return None, "gh api status returned output that is not JSON"

    return (
        {
            "state": info["state"],
            "statusCheckRollup": rollup,
            "headRefOid": sha,
            # The BRANCH, not just the sha: the in-flight-review guard keys its
            # hold on the branch (registration happens in a PreToolUse hook that
            # cannot afford a `gh` call to map one to a PR), so the merge side
            # is where the mapping is paid. Already fetched - `fetch_pr_info_rest`
            # validates head.ref on the same request.
            "headRefName": info["head_ref"],
            "mergeable": info["mergeable"],
        },
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
    details: bool = False,
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
            summary: dict[str, Any] = {"number": number, "state": _map_pr_state(row)}
            if details:
                head = row.get("head")
                if not isinstance(head, dict) or not isinstance(head.get("ref"), str):
                    return None, f"gh api pulls list page {page} carried malformed head ref"
                if not isinstance(row.get("title"), str) or not isinstance(
                    row.get("html_url"), str
                ):
                    return None, f"gh api pulls list page {page} carried malformed title/url"
                summary.update(
                    {
                        "title": row["title"],
                        "headRefName": head["ref"],
                        "url": row["html_url"],
                    }
                )
            rows.append(summary)
        if len(payload) < per_page:
            break
    else:
        # Every page came back full, so the ceiling cut a listing that had more
        # rows. Loud on purpose: the old gh pr list path logged "possibly
        # truncated" for the same condition, and a silent ceiling is a sweep
        # that reads complete while missing its tail.
        log.warning(
            "gh api pulls list for %s hit the max_pages=%d ceiling with a full last page:"
            " listing is possibly truncated after %d rows",
            slug,
            max_pages,
            len(rows),
        )
    return rows, ""


def graphql_remaining(
    runner: Callable = run, cwd: Optional[str] = None, timeout: Optional[float] = 30.0
) -> "tuple[Optional[int], Optional[str]]":
    """Read the shared GraphQL budget: `(remaining, reset_iso)`.

    `gh api rate_limit` does not itself count against any bucket. Both values
    are None on any failure, and a caller MUST read None as unknown, never as
    zero: skipping work because the instrument is unreadable is an absence
    read as evidence.
    """
    try:
        res = runner(["gh", "api", "rate_limit"], cwd=cwd, timeout=timeout)
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
