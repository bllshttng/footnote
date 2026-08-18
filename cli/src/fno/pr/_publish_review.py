"""Post a review verdict to GitHub as the reviewer lane's own identity (x-93ea).

GitHub refuses an approving review from the PR's author: the API accepts the
call and silently records ``COMMENTED``. Since one account authors and reviews
every PR here, every verdict has landed as an undifferentiated comment and
``reviewDecision`` reads empty. This module is the producer that fixes it: it
posts as the distinct machine account named by ``review.bot_identity``, so a
clean pass can carry ``APPROVED`` and branch protection with required approving
reviews becomes satisfiable at all.

Fail-closed by construction, in this order: config missing -> skipped; token
missing -> skipped; identity collision with the PR author -> refused; stale
head pin -> refused; unmappable verdict -> refused. Only then does the POST
fire, and the result carries the ``reviewDecision`` GitHub reports back rather
than trusting the POST receipt - a receipt saying "posted APPROVE" while GitHub
recorded ``COMMENTED`` is the exact lie this module exists to remove.

Deliberately a one-function surface: ``fno event emit -t review_attestation``
mirrors through it (every review verdict funnels there), and the hidden
``fno pr publish-review`` verb calls it for backfill. One implementation, two
doors.
"""
from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from fno.pr._proc import ToolMissing, run

# gh calls are bounded so a hung network stack cannot hang the emit chokepoint.
_GH_TIMEOUT = 30.0

_VERDICT_EVENTS = {"pass": "APPROVE", "fail": "REQUEST_CHANGES"}


@dataclass
class PublishResult:
    """Outcome of one publish attempt. ``status``:

    - ``posted``   - the POST succeeded and ``review_decision`` carries what
                     GitHub read back (never assumed from the POST alone)
    - ``skipped``  - not configured / no token / no open PR / dry-run; posting
                     was not the right action, nothing is wrong
    - ``refused``  - posting would manufacture a false verdict (identity
                     collision, stale head pin, unmappable verdict)
    - ``failed``   - gh errored or is missing; ``stderr`` carries the detail
    """

    status: str
    reason: str = ""
    event: Optional[str] = None  # APPROVE | REQUEST_CHANGES (the mapped event)
    review_decision: Optional[str] = None  # reviewDecision read back from GitHub
    stderr: Optional[str] = None
    receipt: str = ""

    @property
    def ok(self) -> bool:
        return self.status == "posted"


def _resolve_repo_root(cwd: str) -> Path:
    """Git toplevel from ``cwd``, falling back to ``cwd`` itself.

    ``load_settings_for_repo`` reads ``<root>/.fno/`` with no upward walk
    (same trap ``_review_lane_configured`` documents), so a publish invoked
    from a subdirectory must resolve the toplevel first or it would silently
    see only the global config layer and report the lane unconfigured.
    """
    res = run(["git", "rev-parse", "--show-toplevel"], cwd=cwd, timeout=_GH_TIMEOUT)
    return Path(res.stdout.strip()) if res.ok and res.stdout.strip() else Path(cwd)


def _gh_scalar(args, cwd: str) -> Optional[str]:
    """One scalar field via ``gh ... --jq .field``, or None.

    ``--jq`` prints the field BARE (``bllshttng``, not ``"bllshttng"``), so
    unlike `_gh_api_json` in `_verify.py` this must not json.loads. A failed
    call or a JSON ``null`` (which serialises to the literal string) reads as
    None so callers degrade open rather than compare garbage.
    """
    res = run(["gh", *args], cwd=cwd, timeout=_GH_TIMEOUT)
    out = res.stdout.strip() if res.ok else ""
    if not out or out == "null":
        return None
    return out


def _receipt(status: str, reason: str) -> str:
    # The posted line carries the event, identity, and readback inline (the
    # plan's documented shape); every other status wraps its reason.
    if status == "posted":
        return f"bot-review: {reason}"
    return f"bot-review: {status} ({reason})"


def publish_review(
    *,
    pr_number: int,
    head_sha: str,
    verdict: str,
    reviewer: str,
    cwd: str,
    dry_run: bool = False,
) -> PublishResult:
    """Post ``verdict`` on PR ``pr_number`` as ``review.bot_identity``.

    Never raises: every failure path lands in the result so the best-effort
    emit chokepoint can print one receipt line and move on.
    """
    try:
        return _publish_review(
            pr_number=pr_number,
            head_sha=head_sha,
            verdict=verdict,
            reviewer=reviewer,
            cwd=cwd,
            dry_run=dry_run,
        )
    except ToolMissing as exc:
        return PublishResult(
            status="failed",
            reason=str(exc),
            stderr=str(exc),
            receipt=_receipt("failed", str(exc)),
        )
    except Exception as exc:  # noqa: BLE001 - the emit mirror must never blow up
        return PublishResult(
            status="failed",
            reason=f"unexpected error: {exc}",
            stderr=str(exc),
            receipt=_receipt("failed", f"unexpected error: {exc}"),
        )


def _publish_review(
    *,
    pr_number: int,
    head_sha: str,
    verdict: str,
    reviewer: str,
    cwd: str,
    dry_run: bool,
) -> PublishResult:
    # 1. Config + token. Missing either is a skip, not an error: an unconfigured
    #    lane must behave byte-identically to today apart from the receipt line.
    try:
        from fno.config import load_settings_for_repo

        review_cfg = load_settings_for_repo(_resolve_repo_root(cwd)).review
    except Exception as exc:  # noqa: BLE001 - config errors skip, never crash the emit
        return PublishResult(
            status="skipped",
            reason=f"config unreadable: {exc}",
            receipt=_receipt("skipped", f"config unreadable: {exc}"),
        )
    identity = (review_cfg.bot_identity or "").strip() or None
    token_env = (review_cfg.bot_token_env or "").strip() or None
    if not identity:
        return PublishResult(
            status="skipped",
            reason="review.bot_identity unset",
            receipt=_receipt("skipped", "review.bot_identity unset"),
        )
    if not token_env:
        return PublishResult(
            status="skipped",
            reason="review.bot_token_env unset",
            receipt=_receipt("skipped", "review.bot_token_env unset"),
        )
    # A fine-grained PAT in an env var is deliberately the whole token story:
    # a GitHub App installation token (JWT signing + hourly refresh) is the
    # upgrade path when more than this one repo needs the reviewer identity.
    token = os.environ.get(token_env, "")
    if not token:
        return PublishResult(
            status="skipped",
            reason=f"${token_env} unset or empty",
            receipt=_receipt("skipped", f"${token_env} unset or empty"),
        )

    # 2. Identity collision. This is the whole defect the node exists to fix:
    #    posting as the PR author would have GitHub accept the call and record
    #    COMMENTED - manufacturing the exact empty-reviewDecision lie. Compare
    #    bot-suffix-stripped logins (GitHub appends ``[bot]`` to app logins).
    from fno.pr._reviews import _strip_bot

    author = _gh_scalar(
        ["pr", "view", str(pr_number), "--json", "author", "--jq", ".author.login"],
        cwd,
    )
    if not author:
        return PublishResult(
            status="skipped",
            reason=f"could not read author of #{pr_number}",
            receipt=_receipt("skipped", f"could not read author of #{pr_number}"),
        )
    if _strip_bot(author) == _strip_bot(identity):
        detail = f"bot identity {identity} is the PR author"
        return PublishResult(
            status="refused",
            reason=detail,
            receipt=_receipt("refused", detail),
        )

    # 3. Head pin. The attestation is evidence about ONE commit; approving a PR
    #    whose head has moved past it approves commits nobody reviewed. A stale
    #    approval is worse than no approval.
    pr_head = _gh_scalar(
        ["pr", "view", str(pr_number), "--json", "headRefOid", "--jq", ".headRefOid"],
        cwd,
    )
    if not pr_head:
        return PublishResult(
            status="skipped",
            reason=f"could not read head sha of #{pr_number}",
            receipt=_receipt("skipped", f"could not read head sha of #{pr_number}"),
        )
    if pr_head != head_sha:
        detail = f"stale pin: attested {head_sha[:8]} but PR head is {pr_head[:8]}"
        return PublishResult(
            status="refused",
            reason=detail,
            receipt=_receipt("refused", detail),
        )

    # 4. Verdict map. Anything the attestation schema does not enumerate is a
    #    verdict no GitHub event exists for; posting it as a comment would be
    #    the empty-reviewDecision shape again.
    event = _VERDICT_EVENTS.get(verdict)
    if event is None:
        detail = f"unmappable verdict {verdict!r}"
        return PublishResult(
            status="refused",
            reason=detail,
            receipt=_receipt("refused", detail),
        )

    if dry_run:
        detail = f"dry-run: would post {event} as {identity}"
        return PublishResult(
            status="skipped",
            reason=detail,
            event=event,
            receipt=_receipt("skipped", detail),
        )

    # 5. The POST, authenticated as the bot in the subprocess env ONLY: the
    #    caller's own gh auth must survive this call unchanged, and the token
    #    must not leak into any other subprocess this process later spawns.
    slug = _gh_scalar(
        ["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"], cwd
    )
    if not slug:
        return PublishResult(
            status="skipped",
            reason="could not resolve repo slug",
            receipt=_receipt("skipped", "could not resolve repo slug"),
        )
    body = f"fno review mirror: reviewer={reviewer} verdict={verdict} head={head_sha}"
    post_env = {**os.environ, "GH_TOKEN": token}
    res = run(
        [
            "gh",
            "api",
            "-X",
            "POST",
            f"/repos/{slug}/pulls/{pr_number}/reviews",
            "-f",
            f"event={event}",
            "-f",
            f"commit_id={head_sha}",
            "-f",
            f"body={body}",
        ],
        cwd=cwd,
        env=post_env,
        timeout=_GH_TIMEOUT,
    )
    if not res.ok:
        detail = f"gh api POST reviews failed (exit {res.returncode})"
        return PublishResult(
            status="failed",
            reason=detail,
            event=event,
            stderr=res.stderr.strip() or None,
            receipt=_receipt("failed", detail),
        )

    # 6. Read back what GitHub actually recorded. Load-bearing, not decoration:
    #    the readback is the only honest answer to "did it land as APPROVED?".
    review_decision = _gh_scalar(
        ["pr", "view", str(pr_number), "--json", "reviewDecision", "--jq", ".reviewDecision"],
        cwd,
    )
    detail = (
        f"posted {event} as {identity} on #{pr_number} "
        f"(reviewDecision={review_decision or ''})"
    )
    return PublishResult(
        status="posted",
        reason=detail,
        event=event,
        review_decision=review_decision,
        receipt=_receipt("posted", detail),
    )
