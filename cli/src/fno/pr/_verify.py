"""In-package port of verify-pr-merged.sh + verify-review-replies.sh.

ab-d4c98550, US2. Two external-outcome audits for the auto-merge / external-
review gates, ported from bash to gh-api subprocess + native JSON parsing.

verify --kind merged  (verify-pr-merged.sh):
    GitHub merge-state audit. Records the merge into the state-file frontmatter
    when MERGED; blocks with a specific reason when the merge cannot happen;
    runs ONE bounded remediation (single merge attempt + single 30s poll,
    anti-thrash; x-9d11: no --auto and no --delete-branch - checks enforced
    in-process, cleanup split from the merge) when OPEN + all-clean: attempt.
    Exit 0 merged/degrade-open, 1 blocked-with-reason, 2 substrate failure.

verify --kind reviews (verify-review-replies.sh):
    Walks PR review+comment history; flips external_review_passed back to false
    (exit 1) when a non-pending reviewer has no QUALIFYING reply (a PR-author
    reply strictly after that reviewer's latest review, @-mentioning them OR
    within 24h). This is the forgery-hole-closing branch.
    Exit 0 honest/degrade-open/no-reviewers, 1 flipped, 2 substrate failure.

Both keep stdout for the contract output and stderr for diagnostics.
"""
from __future__ import annotations

import datetime
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, List, Optional, Sequence

from fno.mutex import acquire_dir_mutex, release_dir_mutex
from fno.pr._proc import ToolMissing, run

# Check classification lives in fno.pr._status (_classify + _latest_per_name),
# shared with the merge verb so the two surfaces never disagree (round 12).


# ---------------------------------------------------------------------------
# shared helpers
# ---------------------------------------------------------------------------


def _gh_available() -> bool:
    return shutil.which("gh") is not None


def _repo_root(cwd: str) -> str:
    try:
        res = run(["git", "rev-parse", "--show-toplevel"], cwd=cwd)
    except ToolMissing:
        # git absent -> fall back to cwd rather than a raw traceback.
        return cwd
    return res.stdout.strip() if res.ok and res.stdout.strip() else cwd


def _dequote(val: str) -> str:
    """Strip ONE matched surrounding quote pair (no naive unbalanced strip).

    A naive ``.strip('"').strip("'")`` mangles a value that legitimately
    starts or ends with a quote; only strip when both ends match (gemini on
    PR #524).
    """
    if len(val) >= 2 and ((val[0] == '"' and val[-1] == '"') or (val[0] == "'" and val[-1] == "'")):
        return val[1:-1]
    return val


def _read_field(state_file: str, field: str) -> str:
    """Read ``field:`` from frontmatter (awk/sed parity: first match, dequoted)."""
    try:
        with open(state_file, "r", encoding="utf-8") as fh:
            for ln in fh:
                if ln.startswith(field + ":"):
                    return _dequote(ln[len(field) + 1:].strip())
    except OSError:
        pass
    return ""


def _alt(*vals: Any) -> Any:
    """jq ``//`` alternative: first value that is not None and not False."""
    for v in vals:
        if v is not None and v is not False:
            return v
    return None


class _Lock:
    """mkdir-based mutex on the shared ``${path}.lock.d`` convention.

    ``steal`` opts into age-based corpse stealing (see ``fno.mutex``). It is
    safe only where the critical section is a whole-line append, which another
    writer can interleave harmlessly. A read-modify-write section must NOT set
    it: a holder suspended past the threshold resumes and writes back a
    snapshot taken before the stealer's update, silently losing it.
    """

    def __init__(self, target: str, timeout: int = 30, *, steal: bool = False) -> None:
        self.dir = target + ".lock.d"
        self.timeout = timeout
        self.steal = steal
        self.token: str | None = None

    def acquire(self) -> bool:
        os.makedirs(os.path.dirname(self.dir) or ".", exist_ok=True)
        self.token = acquire_dir_mutex(Path(self.dir), self.timeout, steal=self.steal, poll_s=1)
        return self.token is not None

    def release(self) -> None:
        if self.token is not None:
            release_dir_mutex(Path(self.dir), self.token)
            self.token = None


def _emit_audit(
    repo_root: str, state_file: str, pr_number: str, reason: str, extra: Optional[dict] = None
) -> None:
    """Append a transcript_audit_failed event (best-effort; never fatal)."""
    events_file = os.path.join(repo_root, ".fno", "events.jsonl")
    nonce = _read_field(state_file, "provenance_nonce")
    sid = _read_field(state_file, "session_id")
    ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    data: dict = {
        "gate": "auto_merge_outcome",
        "reason": reason,
        "pr_number": pr_number,
        "nonce": nonce,
        "session_id": sid,
    }
    if extra:
        data.update(extra)
    event = {"ts": ts, "type": "transcript_audit_failed", "source": "hook", "data": data}
    _append_event_lenient(events_file, event, reason)


def _append_event_lenient(events_file: str, event: dict, reason: str) -> None:
    """Validate-with-warning, then append under the events mkdir-mutex.

    Mirrors the bash: a schema-validation failure logs a warning but the event
    is appended anyway (missing audit evidence is worse than a relaxed shape).
    """
    try:
        from fno.events import validate

        validate(event)
    except Exception:
        sys.stderr.write(
            f"pr-verify: schema validation failed for transcript_audit_failed "
            f"(reason={reason}); appending anyway\n"
        )
    lock = _Lock(events_file, 30, steal=True)  # whole-line append
    if not lock.acquire():
        sys.stderr.write(f"pr-verify: events.jsonl lock timeout (reason={reason})\n")
        return
    try:
        with open(events_file, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(event, separators=(",", ":")) + "\n")
    finally:
        lock.release()


def _record_merge(state_file: str, pr: str, merged_at: str) -> bool:
    """Append merged_prs (idempotent) + set merged_at in frontmatter, atomically.

    Direct port of the verify-pr-merged.sh embedded Python heredoc: pre-scan
    for an existing merged_at line so we never both insert and replace, write to
    a sibling tempfile, and os.replace() for atomic rename.
    """
    lock = _Lock(state_file, 30)
    if not lock.acquire():
        sys.stderr.write("verify-pr-merged: state-file lock timeout\n")
        return False
    try:
        with open(state_file, "r", encoding="utf-8") as fh:
            lines = fh.readlines()
        # Pre-scan: does an existing merged_at line live inside the frontmatter?
        fm_count = 0
        existing_merged_at_idx = None
        for i, ln in enumerate(lines):
            if ln.rstrip() == "---":
                fm_count += 1
                if fm_count == 2:
                    break
                continue
            if fm_count == 1 and ln.startswith("merged_at:"):
                existing_merged_at_idx = i
        out: List[str] = []
        saw_merged_prs = False
        in_fm = False
        fm_seen = 0
        for i, ln in enumerate(lines):
            if ln.rstrip() == "---":
                fm_seen += 1
                in_fm = fm_seen == 1
                if fm_seen == 2 and existing_merged_at_idx is None:
                    out.append(f'merged_at: "{merged_at}"\n')
                out.append(ln)
                continue
            if in_fm and ln.startswith("merged_prs:"):
                import re as _re

                m = _re.match(r"merged_prs:\s*\[(.*)\]\s*$", ln)
                if m:
                    existing = [x.strip() for x in m.group(1).split(",") if x.strip()]
                    if pr not in existing:
                        existing.append(pr)
                    out.append("merged_prs: [" + ", ".join(existing) + "]\n")
                else:
                    out.append(ln)
                saw_merged_prs = True
                continue
            if in_fm and i == existing_merged_at_idx:
                out.append(f'merged_at: "{merged_at}"\n')
                continue
            out.append(ln)
        if not saw_merged_prs:
            out.append(f"merged_prs: [{pr}]\n")
        state_dir = os.path.dirname(state_file) or "."
        fd, tmp = tempfile.mkstemp(prefix=".target-state.", suffix=".tmp", dir=state_dir)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                fh.writelines(out)
            os.replace(tmp, state_file)
        except Exception:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise
        return True
    except Exception:
        return False
    finally:
        lock.release()


def _auto_merge():
    from fno.config import load_settings

    return load_settings().auto_merge


# ---------------------------------------------------------------------------
# verify --kind merged
# ---------------------------------------------------------------------------


def _fetch_pr_state(pr: str, cwd: str) -> Optional[dict]:
    res = run(
        [
            "gh",
            "pr",
            "view",
            pr,
            "--json",
            "state,mergedAt,isDraft,reviewDecision,statusCheckRollup",
        ],
        cwd=cwd,
    )
    if not res.ok or not res.stdout.strip():
        return None
    try:
        return json.loads(res.stdout)
    except json.JSONDecodeError:
        return None


def run_verify_merged(
    pr_number: str,
    state_file: str,
    cwd: Optional[str] = None,
    *,
    sleep_fn=time.sleep,
) -> int:
    repo = cwd or os.getcwd()
    if not pr_number:
        sys.stderr.write("verify-pr-merged: --pr-number required\n")
        return 2
    if not state_file or not os.access(state_file, os.R_OK):
        sys.stderr.write(f"verify-pr-merged: --state-file unreadable: {state_file}\n")
        return 2
    if not _gh_available():
        sys.stderr.write("verify-pr-merged: gh missing; degrading open\n")
        return 0

    repo_root = _repo_root(repo)
    remediation = _auto_merge().remediation

    pr_json = _fetch_pr_state(pr_number, repo)
    if pr_json is None:
        sys.stderr.write("verify-pr-merged: gh pr view failed; degrading open\n")
        return 0

    state = pr_json.get("state") or ""
    merged_at = pr_json.get("mergedAt") or ""
    is_draft = pr_json.get("isDraft") or False
    review_decision = pr_json.get("reviewDecision") or ""

    if state == "MERGED":
        if _record_merge(state_file, pr_number, merged_at):
            sys.stdout.write(f"verify-pr-merged: PR #{pr_number} MERGED at {merged_at}\n")
            return 0
        sys.stderr.write(
            "verify-pr-merged: record_merge failed (state lock or python error); "
            "state file may be stale\n"
        )
        return 2
    if state == "CLOSED":
        _emit_audit(repo_root, state_file, pr_number, "pr_closed_without_merge", {"pr_state": "CLOSED"})
        sys.stdout.write(
            f"pr_closed_without_merge: PR #{pr_number} is CLOSED (not merged) "
            "— investigate before re-promising\n"
        )
        return 1
    if state != "OPEN":
        sys.stderr.write(f"verify-pr-merged: unknown PR state: {state}; degrading open\n")
        return 0

    # OPEN: check merge preconditions before remediation.
    if is_draft is True:
        _emit_audit(repo_root, state_file, pr_number, "pr_is_draft", {"pr_state": "OPEN"})
        sys.stdout.write(
            f"pr_is_draft: PR #{pr_number} is in draft state; un-draft before auto-merge can run\n"
        )
        return 1
    if review_decision == "CHANGES_REQUESTED":
        _emit_audit(
            repo_root, state_file, pr_number, "review_changes_requested",
            {"review_decision": "CHANGES_REQUESTED"},
        )
        sys.stdout.write(
            f"review_changes_requested: PR #{pr_number} has CHANGES_REQUESTED; "
            "address the review before auto-merge\n"
        )
        return 1

    # Same posture as the merge verb (round 10): the checks precondition applies
    # only when require_checks_pass is on, and PENDING is not failing - a
    # still-running check flows into _bounded_remediation, which reports
    # not-green without the misleading "failing" label. Judging pending here
    # would make verify refuse what `fno pr merge` merges.
    if _auto_merge().require_checks_pass:
        failing = _failing_required(pr_json.get("statusCheckRollup") or [])
        if failing:
            failing_csv = ",".join(failing)
            _emit_audit(
                repo_root, state_file, pr_number, "required_checks_failing",
                {"failing_checks": failing_csv},
            )
            sys.stdout.write(f"required_checks_failing: {failing_csv}\n")
            return 1

    # All preconditions clean.
    if remediation == "verify_only":
        _emit_audit(
            repo_root, state_file, pr_number, "remediation_disabled",
            {"remediation": "verify_only"},
        )
        sys.stdout.write(
            f"remediation_disabled: PR #{pr_number} is mergeable but "
            "config.auto_merge.remediation: verify_only — merge manually or flip the setting\n"
        )
        return 1

    return _bounded_remediation(pr_number, state_file, repo, repo_root, sleep_fn)


def _failing_required(rollup: Sequence[dict]) -> List[str]:
    """Failing checks (whole rollup), classified by the SAME truth table the
    merge verb uses - a second hand-built state table is how verify ends up
    refusing what `fno pr merge` merges (round 12). _latest_per_name drops
    superseded runs; _classify reads pass/fail/pending with the shared
    semantics (a REQUESTED or empty-conclusion check is pending, not failing).
    No isRequired filter - `gh pr view` never emits that key (see
    _merge._checks_verdict), so with require_checks_pass every check counts."""
    from fno.pr._status import _classify, _latest_per_name

    failing: List[str] = []
    for c in _latest_per_name(rollup):
        if _classify(c) == "fail":
            failing.append(str(_alt(c.get("name"), c.get("context"), "unnamed")))
    return failing


def _remote_delete_cleanup(pr_number: str, cwd: str, auto_merge) -> None:
    """Post-merge remote-branch delete (x-9d11), warn-only: reuses the merge
    verb's helper so both executors treat cleanup identically and neither can
    fail its merge with a cleanup result."""
    if not getattr(auto_merge, "delete_branch_on_merge", False):
        return
    from fno.pr import _merge as _merge_mod

    note = _merge_mod._post_merge_remote_delete(int(pr_number), cwd, auto_merge)
    if note:
        sys.stderr.write(
            f"verify-pr-merged: post-merge cleanup {note} (non-fatal)\n"
        )


def _bounded_remediation(
    pr_number: str, state_file: str, cwd: str, repo_root: str, sleep_fn
) -> int:
    """Single gh pr merge attempt + single 30s poll (anti-thrash; x-9d11: the
    verb executes - no --auto, no --delete-branch)."""
    # Stacked-base guard: this arm reaches `gh pr merge` without passing through
    # `fno pr merge`, so it needs its own call - a guard on one of N reachable
    # merge paths is decorative. An unevaluated probe proceeds with a
    # breadcrumb; only a confirmed stale base refuses, because it needs a
    # retarget rather than a retry.
    from fno.pr import _base_lineage

    lineage, why = _base_lineage.lineage_verdict(pr_number, cwd)
    if lineage == "stale" and not _base_lineage.bypassed():
        _emit_audit(
            repo_root, state_file, pr_number, "merge_refused_stacked_base", {"reason": why}
        )
        sys.stdout.write(f"merge_refused_stacked_base: PR #{pr_number} {why}\n")
        return 1
    if lineage == "stale":
        _base_lineage.emit_bypass_escape(pr_number, cwd, why)
        sys.stderr.write(
            f"verify-pr-merged: stacked-base guard bypassed "
            f"({_base_lineage.BYPASS_ENV}); {why}\n"
        )
    elif lineage == "unknown":
        sys.stderr.write(
            f"verify-pr-merged: stacked-base probe unavailable ({why}); "
            "remediating without the lineage guard\n"
        )

    auto_merge = _auto_merge()
    strategy = auto_merge.merge_strategy
    cmd = ["gh", "pr", "merge", pr_number, f"--{strategy}"]
    # x-9d11: no --auto (one arming path - finalize owns the queue; an executor
    # reads the checks and merges green itself) and no --delete-branch (gh's
    # local delete is the worktree false-failure shape). require_checks_pass is
    # enforced here with the shared verdict helper, head-pinned like _do_merge.
    # x-9d11 AC5-CON: same one-arming-path rule as _do_merge - if finalize
    # already armed GitHub's queue, stand down instead of racing it.
    from fno.pr import _merge as _merge_mod

    try:
        armed = _merge_mod._already_armed(int(pr_number), cwd)
    except ToolMissing:
        # _already_armed calls _gh, which can raise ToolMissing same as the
        # sibling _checks_verdict call below - it owes the same handler its
        # sibling call sites have (review round 12).
        sys.stderr.write("verify-pr-merged: gh CLI not installed\n")
        return 127
    if armed:
        _emit_audit(
            repo_root, state_file, pr_number, "merge_attempt_did_not_complete",
            {"reason": "already armed in GitHub auto-merge queue"},
        )
        sys.stdout.write(
            f"merge_attempt_did_not_complete: PR #{pr_number} already armed "
            "in GitHub's auto-merge queue (fno-agents finalize); the queue "
            "merges it when checks pass\n"
        )
        return 1

    if auto_merge.require_checks_pass:
        try:
            verdict, _counts, head_read = _merge_mod._checks_verdict(
                int(pr_number), cwd
            )
        except ToolMissing:
            # _checks_verdict deliberately propagates it (the module contract
            # reserves 127 for a missing gh); this third call site owes the
            # same handler its siblings have (review round 7).
            sys.stderr.write("verify-pr-merged: gh CLI not installed\n")
            return 127
        if verdict != "green":
            _emit_audit(
                repo_root,
                state_file,
                pr_number,
                "merge_attempt_did_not_complete",
                {"checks": verdict},
            )
            sys.stdout.write(
                f"merge_attempt_did_not_complete: PR #{pr_number} checks are "
                f"{verdict}; require_checks_pass forbids merging without green "
                f"(retry when green)\n"
            )
            return 1
        if not head_read:
            # Same fail-closed rule as _do_merge: green but unpinnable is not
            # mergeable - an unpinned merge could land a head the verdict never
            # described.
            _emit_audit(
                repo_root, state_file, pr_number, "merge_attempt_did_not_complete",
                {"checks": "green, head unreadable"},
            )
            sys.stdout.write(
                f"merge_attempt_did_not_complete: PR #{pr_number} checks read "
                "green but the head SHA was unreadable; refusing to merge a "
                "head the verdict cannot be pinned to\n"
            )
            return 1
        cmd += ["--match-head-commit", head_read]
    res = run(cmd, cwd=cwd)
    gh_stderr = res.stderr or ""
    if res.ok:
        # Re-fetch once; if still OPEN do ONE bounded 30s poll.
        for attempt in range(2):
            pr_json = _fetch_pr_state(pr_number, cwd)
            state = (pr_json or {}).get("state") or ""
            if state == "MERGED":
                merged_at = (pr_json or {}).get("mergedAt") or ""
                _record_merge(state_file, pr_number, merged_at)
                _remote_delete_cleanup(pr_number, cwd, auto_merge)
                sys.stdout.write(f"verify-pr-merged: PR #{pr_number} MERGED at {merged_at}\n")
                return 0
            if attempt == 0:
                sleep_fn(30)
        _emit_audit(
            repo_root, state_file, pr_number, "merge_attempt_did_not_complete",
            {"final_state": (pr_json or {}).get("state") or ""},
        )
        sys.stdout.write(
            f"merge_attempt_did_not_complete: PR #{pr_number} still "
            f"{(pr_json or {}).get('state') or ''} after merge attempt + 30s poll\n"
        )
        return 1

    # gh pr merge exited non-zero. ALWAYS re-read the PR state before reporting
    # failure: gh exits non-zero whenever a POST-merge step fails (local branch
    # delete, base-branch checkout, remote delete) after a successful server-side
    # merge, and the error phrasing varies across git versions and failure points
    # (the checkout-refused and delete phrasings differ; older git says "checked
    # out at"). Matching phrasings ages badly; the durable signal is the PR's
    # state. If it MERGED, record it and return 0 - otherwise the /target gate
    # that calls verify sees a failure, leaves the merge unrecorded in
    # target-state.md, and re-verifies forever.
    pr_json = _fetch_pr_state(pr_number, cwd)
    if (pr_json or {}).get("state") == "MERGED":
        merged_at = (pr_json or {}).get("mergedAt") or ""
        _record_merge(state_file, pr_number, merged_at)
        _remote_delete_cleanup(pr_number, cwd, auto_merge)
        sys.stdout.write(f"verify-pr-merged: PR #{pr_number} MERGED at {merged_at}\n")
        return 0
    # The re-read itself failed (`_fetch_pr_state` returns None on a gh error or
    # unparseable output), so the merge state is UNKNOWN, not "not merged". The
    # `(pr_json or {})` idiom above flattens those two apart-cases together;
    # reporting merge_attempt_failed on an unreadable state asserts a failure we
    # cannot see, and the /target gate then treats a possibly-landed merge as
    # broken. Report the substrate failure (exit 2) so the caller retries the
    # read instead of acting on a guess. Mirrors the same guard in _merge.py.
    if pr_json is None:
        _emit_audit(
            repo_root, state_file, pr_number, "merge_state_unreadable",
            {"stderr": gh_stderr.splitlines()[0] if gh_stderr.strip() else ""},
        )
        sys.stdout.write(
            f"merge_state_unreadable: could not read PR #{pr_number} state after "
            "gh pr merge exited non-zero; cannot confirm whether the merge landed\n"
        )
        return 2
    lowered = gh_stderr.lower()
    if any(
        tok in lowered
        for tok in ("freshness", "stale state", "state file", "state-file mtime", "git-protection")
    ):
        _emit_audit(
            repo_root, state_file, pr_number, "merge_blocked_by_freshness_cap",
            {"stderr_token": "freshness_cap"},
        )
        sys.stdout.write(
            f"merge_blocked_by_freshness_cap: git-protection.py blocked the merge "
            f"(state-file mtime > 1h). Run '! gh pr merge {pr_number} --{strategy}' via shell to bypass.\n"
        )
        return 1
    first = gh_stderr.splitlines()[0] if gh_stderr.strip() else ""
    _emit_audit(repo_root, state_file, pr_number, "merge_attempt_failed", {"stderr": first})
    sys.stdout.write(f"merge_attempt_failed: gh pr merge exited non-zero. stderr: {first}\n")
    return 1


# ---------------------------------------------------------------------------
# verify --kind reviews
# ---------------------------------------------------------------------------


def _gh_api_json(args: Sequence[str], cwd: str) -> Optional[Any]:
    res = run(["gh", "api", *args], cwd=cwd)
    if not res.ok or not res.stdout.strip():
        return None
    try:
        return json.loads(res.stdout)
    except json.JSONDecodeError:
        return None


def _parse_ts(s: str) -> datetime.datetime:
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))


def _has_qualifying_reply(
    comments: Sequence[dict], login: str, last_at: str, pr_author: str
) -> bool:
    """Port of the verify-review-replies qualifying-reply predicate.

    A comment qualifies when (a) authored by the PR author, (b) created strictly
    after the reviewer's latest review, AND (c) it @-mentions the reviewer OR
    was posted within 24h of that review. Loosening any clause reopens the
    gate-forgery hole (feedback_gate_forgery_external_review, PR #234).
    """
    try:
        last_dt = _parse_ts(last_at)
    except (ValueError, TypeError):
        # A malformed/empty reviewer timestamp can't anchor the window; treat
        # the reviewer as un-replied-to rather than crashing (gemini on #524).
        return False
    window = datetime.timedelta(hours=24)
    for c in comments:
        if c.get("login") != pr_author:
            continue
        created = c.get("created_at")
        if not created:
            continue
        try:
            c_dt = _parse_ts(created)
        except Exception:
            continue
        if c_dt <= last_dt:
            continue
        body = c.get("body") or ""
        if ("@" + login) in body or (c_dt - last_dt) <= window:
            return True
    return False


def run_verify_reviews(pr_number: str, state_file: str, cwd: Optional[str] = None) -> int:
    repo = cwd or os.getcwd()
    if not pr_number:
        sys.stderr.write("verify-review-replies: --pr-number required\n")
        return 2
    if not state_file or not os.access(state_file, os.R_OK):
        sys.stderr.write(f"verify-review-replies: --state-file unreadable: {state_file}\n")
        return 2
    if not _gh_available():
        sys.stderr.write("verify-review-replies: gh missing; degrading open\n")
        return 0

    repo_root = _repo_root(repo)

    repo_name = run(["gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"], cwd=repo)
    repo_slug = repo_name.stdout.strip() if repo_name.ok else ""
    if not repo_slug:
        sys.stderr.write("verify-review-replies: gh repo view failed; degrading open\n")
        return 0

    reviews = _gh_api_json(
        [
            f"/repos/{repo_slug}/pulls/{pr_number}/reviews",
            "--jq",
            "[ .[] | select(.state != \"PENDING\") | {login: .user.login, submitted_at: .submitted_at} ]",
        ],
        repo,
    )
    if reviews is None:
        sys.stderr.write("verify-review-replies: gh api reviews failed; degrading open\n")
        return 0

    # Collapse to one entry per reviewer with their newest submitted_at.
    reviewer_map: dict = {}
    for r in sorted(reviews, key=lambda x: x.get("submitted_at") or "", reverse=True):
        login = r.get("login")
        if login and login not in reviewer_map:
            reviewer_map[login] = r.get("submitted_at")
    if not reviewer_map:
        # No non-pending reviewers - nothing to audit.
        return 0

    pr_author_res = run(["gh", "pr", "view", pr_number, "--json", "author", "--jq", ".author.login"], cwd=repo)
    pr_author = pr_author_res.stdout.strip() if pr_author_res.ok else ""
    if pr_author == "null":  # a null author serialises to the literal string
        pr_author = ""
    if not pr_author:
        sys.stderr.write("verify-review-replies: could not fetch PR author; degrading open\n")
        return 0

    issue_comments = _gh_api_json(
        [
            f"/repos/{repo_slug}/issues/{pr_number}/comments",
            "--jq",
            "[ .[] | {login: .user.login, created_at: .created_at, body: .body} ]",
        ],
        repo,
    ) or []
    review_comments = _gh_api_json(
        [
            f"/repos/{repo_slug}/pulls/{pr_number}/comments",
            "--jq",
            "[ .[] | {login: .user.login, created_at: .created_at, body: .body} ]",
        ],
        repo,
    ) or []
    all_comments = list(issue_comments) + list(review_comments)

    missing: List[str] = []
    for login, last_at in reviewer_map.items():
        if not login or not last_at:
            continue
        if not _has_qualifying_reply(all_comments, login, last_at, pr_author):
            missing.append(login)

    if not missing:
        return 0

    # Emit one transcript_audit_failed event per missing reviewer.
    ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    nonce = _read_field(state_file, "provenance_nonce")
    sid = _read_field(state_file, "session_id")
    events_file = os.path.join(repo_root, ".fno", "events.jsonl")
    for reviewer in missing:
        last_at = reviewer_map.get(reviewer) or ""
        event = {
            "ts": ts,
            "type": "transcript_audit_failed",
            "source": "hook",
            "data": {
                "gate": "external_review_passed",
                "reason": "missing_reply_to_reviewer",
                "pr_number": pr_number,
                "reviewer": reviewer,
                "last_review_at": last_at,
                "nonce": nonce,
                "session_id": sid,
            },
        }
        _append_event_lenient(events_file, event, f"missing_reply:{reviewer}")

    sys.stdout.write(
        "verify-review-replies: external_review_passed flipped back to false "
        f"— missing replies to: {' '.join(missing)}\n"
    )
    return 1
