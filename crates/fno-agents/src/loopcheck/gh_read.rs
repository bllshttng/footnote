//! What do git and the PR view say right now? Head sha/branch/cleanliness, the gh pr view read, and the GraphQL quota probe.

use super::*;

/// Whether a `review_attestation` line is about the PR under evaluation.
///
/// The events journal is shared across every worktree of a repo
/// (setup-worktree.sh links them to one canonical file), so an unscoped scan
/// reads every branch's attestations into every PR's verdict list. That is
/// noise in the common case and a false pass in the bad one: `review_freshness`
/// grants CarriedBaseSync on a code-diff identity match, which two branches
/// carrying the same delta (a cherry-pick, a duplicate-PR pair) satisfy - and
/// that shape only exists at DIFFERENT shas, since equal shas return Fresh
/// before any carry is computed.
///
/// Exact head equality is therefore admitted whatever branch the emitter stood
/// on: a foreign branch cannot share this head sha without being this commit,
/// and the spawned-reviewer lane depends on it (review-lanes.md) - the
/// reviewer's worktree necessarily carries a branch of its own (git refuses
/// two worktrees on one branch), so a branch-only match would read its
/// exact-HEAD pass as out of scope. The branch arm is what survives a head
/// move: a same-branch attestation can still carry, while a foreign
/// branch at a different head stays out of scope - the cherry-pick shape.
///
/// Named, not closed: a shared sha proves COMMIT identity, not PR identity.
/// The event carries no base ref or PR number, so a pass attested for one PR
/// clears another PR at the same commit with a different base (a
/// duplicate-PR pair). All PRs here share main as base, so the shape is
/// exotic; closing it needs `base` in the attestation payload (schema +
/// producer + this resolver), filed on the follow-up node.
///
/// `attested_branch` is empty for every event predating the field (a
/// detached HEAD now refuses to emit at all). Those fall back to exact head
/// equality only: a legacy attestation on a moved head is not scopeable and
/// must not count.
pub(super) fn attestation_in_scope(
    attested_branch: &str,
    attested_head: &str,
    head_branch: &str,
    head_sha: &str,
) -> bool {
    if !attested_head.is_empty() && attested_head == head_sha {
        return true;
    }
    !attested_branch.is_empty() && !head_branch.is_empty() && attested_branch == head_branch
}

pub(super) fn git_head_sha(git_bin: &str, cwd: &Path) -> String {
    match git_bounded(git_bin, &["rev-parse", "HEAD"], cwd) {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// True when the working tree has no uncommitted change.
///
/// `merge-base --is-ancestor` sees committed history only, so without this a
/// rebase onto a base that already holds the merge would read as shipped while
/// uncommitted follow-up edits sat in the tree. Uncommitted work IS unshipped
/// work, which is the same charter the #447 guard was written under.
///
/// A git that cannot answer reads as DIRTY, so an unreadable tree never
/// widens what counts as shipped.
pub(super) fn git_tree_clean(git_bin: &str, cwd: &Path) -> bool {
    matches!(
        git_bounded(git_bin, &["status", "--porcelain"], cwd),
        Some(o) if o.status.success() && o.stdout.is_empty()
    )
}

/// True when local HEAD carries nothing the base does not already have.
///
/// Resolves the base the same way [`classify_payload`] does, so the two cannot
/// disagree about which remote branch is the mainline. That heuristic is
/// `origin/main` then `origin/master` and nothing else: on a repo whose
/// mainline is named anything else, NEITHER ref resolves and this returns
/// false, so the post-merge wedge simply stays unfixed there rather than
/// misfiring. The PR's own `baseRefName` would answer exactly, but it is a
/// local in `read_pr_info` rather than a field on [`PrInfo`], and threading it
/// through is unwarranted for a repo shape this project does not have.
///
/// A base that does not resolve, a git that errors, and a HEAD that is
/// genuinely ahead all answer false, which is the conservative direction: see
/// [`head_is_shipped`].
pub(super) fn git_head_on_base(git_bin: &str, cwd: &Path) -> bool {
    for base in ["origin/main", "origin/master"] {
        match git_bounded(
            git_bin,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{base}^{{commit}}"),
            ],
            cwd,
        ) {
            Some(v) if v.status.success() => {}
            _ => continue,
        }
        // `--is-ancestor` exits 0 when HEAD is reachable from base, 1 when it
        // is not, and >1 on a real error. Only a clean 0 counts, so an errored
        // probe reads as "not shipped" rather than as consent to terminate.
        return matches!(
            git_bounded(git_bin, &["merge-base", "--is-ancestor", "HEAD", base], cwd),
            Some(o) if o.status.success()
        );
    }
    false
}

/// True when every local commit is already shipped.
///
/// Two ways that holds: the PR records this exact head, or local HEAD is
/// already reachable from the base, which is what a post-merge rebase or
/// fast-forward onto main produces.
///
/// The second disjunct cannot re-open the codex P1 on #447 that the first one
/// guards. That defect was unpushed work terminating as DonePRGreen, and a
/// genuine unpushed commit stacked on a merged PR is NOT an ancestor of the
/// base, so it still reads as unshipped. The disjunct only ever releases a
/// HEAD with no commits of its own left to ship.
///
/// The equality arm is checked first and makes no subprocess call, so the
/// common path costs nothing. A git that cannot answer leaves the old
/// behavior exactly as it was.
pub(super) fn head_is_shipped(pr: &PrInfo, local_head: &str, git_bin: &str, cwd: &Path) -> bool {
    if pr.head_oid.is_empty() {
        return false;
    }
    if pr.head_oid == local_head {
        return true;
    }
    // The clean-tree condition applies to THIS arm only, deliberately. The
    // equality arm above has always terminated on a dirty tree and changing
    // that is a different decision with its own blast radius; the rule here is
    // only that the new arm must not WIDEN what counts as shipped.
    git_tree_clean(git_bin, cwd) && git_head_on_base(git_bin, cwd)
}

pub(super) fn git_head_branch(git_bin: &str, cwd: &Path) -> Option<String> {
    let out = git_bounded(git_bin, &["branch", "--show-current"], cwd)?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// `gh pr view` exits 1 both when no PR exists and when gh itself fails.
/// "No PR" is real world-state - the fingerprint should record it and the
/// NoProgress backstop should keep ticking - while an outage must freeze the
/// streak (US4). Distinguish via gh's deterministic no-PR stderr message. If
/// gh ever changes the message, no-PR fires degrade to outage semantics
/// (freeze -> budget ceiling): safe, never a premature termination.
pub(super) fn is_no_pr_stderr(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .to_lowercase()
        .contains("no pull requests found")
}

/// Capture the last ~200 bytes of stderr as a lossy UTF-8 string.
pub(super) fn stderr_tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    if s.len() <= 200 {
        s.to_string()
    } else {
        // Byte index must land on a char boundary or the slice panics
        // (gemini HIGH on PR #447): walk forward to the next boundary.
        let mut start = s.len() - 200;
        while start < s.len() && !s.is_char_boundary(start) {
            start += 1;
        }
        s[start..].to_string()
    }
}

pub(super) const PR_VIEW_FIELDS: &str =
    "state,number,headRefName,headRefOid,mergeable,mergeStateStatus,baseRefName,author";

pub(super) fn pr_head_oid(pr_json: &Value) -> Option<String> {
    pr_json
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .filter(|oid| !oid.is_empty())
        .map(str::to_string)
}

pub(super) fn read_pr_view(
    gh_bin: &str,
    cwd: &Path,
    selector: Option<&str>,
) -> Result<Option<Value>, GhReadError> {
    let rest_adapter = internal_gh_adapter(gh_bin);
    let metadata_read = if rest_adapter {
        "pr_info_rest"
    } else {
        "pr_view"
    };
    let metadata_parse = if rest_adapter {
        "pr_info_rest_parse"
    } else {
        "pr_view_parse"
    };
    let mut args = vec!["pr", "view"];
    if let Some(selector) = selector {
        args.push(selector);
    }
    args.extend(["--json", PR_VIEW_FIELDS]);
    let out = bounded_read(
        gh_bin.as_ref(),
        &args,
        cwd,
        metadata_read,
        stopgate_read_timeout(),
    )?;
    if !out.status.success() {
        return if is_no_pr_stderr(&out.stderr_tail) {
            Ok(None)
        } else {
            Err(GhReadError::failed(
                metadata_read,
                stderr_tail(&out.stderr_tail),
            ))
        };
    }
    serde_json::from_slice(&out.stdout)
        .map(Some)
        .map_err(|_| GhReadError::parse_failed(metadata_parse))
}

/// Resolve a numeric PR selector without consulting the checkout branch.
/// `Ok(None)` is the real-world no-PR state; `Err` is an unreadable GitHub
/// response and must not fall back to local HEAD. Returns the raw `pr_json`
/// alongside the head, so a caller that goes on to build a full `PrInfo` can
/// reuse this read instead of issuing a second `gh pr view` for the same
/// selector.
pub(super) fn read_pr_head_oid(
    gh_bin: &str,
    cwd: &Path,
    selector: &str,
) -> Result<Option<(String, Value)>, GhReadError> {
    let Some(pr_json) = read_pr_view(gh_bin, cwd, Some(selector))? else {
        return Ok(None);
    };
    let head = pr_head_oid(&pr_json).ok_or_else(|| {
        let read = if internal_gh_adapter(gh_bin) {
            "pr_info_rest_parse"
        } else {
            "pr_view_parse"
        };
        GhReadError::failed(read, "missing headRefOid".to_string())
    })?;
    Ok(Some((head, pr_json)))
}

/// The GraphQL bucket's state, from `gh api rate_limit`.
///
/// That endpoint is REST and primary-exempt, so the probe is free even while
/// GraphQL sits at 0 - which is its whole job: it distinguishes "the call
/// cannot succeed for N minutes" from "gh blipped", the two outcomes a bare
/// read failure conflates. None on any failure: a failed probe must never
/// fabricate an exhaustion verdict (a false "resets in 40m" would stall a
/// healthy session for no reason).
pub(super) struct GraphqlQuota {
    pub(super) remaining: i64,
    pub(super) reset_epoch: i64,
    /// The CORE bucket from the same probe read. `refusal_is_secondary`
    /// classifies a rate-limit refusal on it (a refusal with core still high
    /// is the request-rate limit, not this bucket). Option because a payload
    /// can name graphql and not core.
    pub(super) core_remaining: Option<i64>,
}

/// Below this GraphQL remaining count, a no-promise fire stands down entirely:
/// the last of the budget belongs to the operation that
/// ships. Code default, named in the PR body - never the operator's config.

pub(super) fn probe_graphql_quota(gh_bin: &str, cwd: &Path) -> Option<GraphqlQuota> {
    // Advisory by contract: any failure (including a timeout kill) is None -
    // a failed probe must never fabricate an exhaustion verdict.
    let out = bounded_read(
        gh_bin.as_ref(),
        &["api", "rate_limit"],
        cwd,
        "graphql_quota",
        stopgate_read_timeout(),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let g = v.get("resources")?.get("graphql")?;
    Some(GraphqlQuota {
        remaining: g.get("remaining").and_then(|x| x.as_i64())?,
        reset_epoch: g.get("reset").and_then(|x| x.as_i64())?,
        core_remaining: v
            .pointer("/resources/core/remaining")
            .and_then(|x| x.as_i64()),
    })
}

/// Whether a refusal's stderr smells like ANY rate limit. This is the wide
/// TRIGGER only, never the verdict: GitHub controls the wording, and its
/// measured 2026-08-24 secondary body says only "API rate limit exceeded for
/// user ID ... (HTTP 403)" - no "secondary" anywhere - so a phrase gate
/// missed the real refusal and read it as an unclassified blip.
pub(super) fn stderr_smells_rate_limit(stderr: &str) -> bool {
    stderr.to_lowercase().contains("rate limit")
}

/// Whether a failed gh read's refusal is GitHub's SECONDARY (request-rate)
/// limit, judged against the LIVE exempt bucket, never against wording.
///
/// `gh api rate_limit` is exempt from both limits and answered DURING the
/// measured refusal with core 4980/5000, so the probe is the one signal
/// GitHub cannot reword: a rate-limit refusal that no drained bucket
/// explains IS the secondary limit. A drained bucket that does explain it
/// (graphql at 0 on a graphql read, core at EXACTLY 0 - a low-but-positive
/// core is not proof, a secondary refusal lands with core wherever it
/// stood) is the primary quota, whose own reasons live elsewhere. No probe
/// at all still says secondary: an unreadable instrument must not send the
/// session to wait for a primary reset that never comes, while backing off
/// is safe under either truth. Mirrors `fno.pr._rest`'s live-bucket
/// classifier and its fail-safe. One deliberate difference: this fn also
/// classifies GRAPHQL reads, so a drained graphql bucket on a graphql read
/// names the primary quota here; the Python classifier sees REST reads only
/// (whose primary quota is core) and needs no graphql arm.
pub(super) fn refusal_is_secondary(
    stderr: &str,
    probe: Option<&GraphqlQuota>,
    failed_read_was_graphql: bool,
) -> bool {
    if !stderr_smells_rate_limit(stderr) {
        return false;
    }
    let Some(q) = probe else {
        return true;
    };
    if failed_read_was_graphql && q.remaining == 0 {
        return false;
    }
    !matches!(q.core_remaining, Some(0))
}

pub(super) fn internal_gh_adapter(gh_bin: &str) -> bool {
    Path::new(gh_bin)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "fno-gh-loopcheck" | "fno-gh-coverage"))
}

pub(crate) fn coverage_adapter(gh_bin: &str) -> String {
    let path = Path::new(gh_bin);
    if path.file_name().and_then(|name| name.to_str()) != Some("fno-gh-loopcheck") {
        return gh_bin.to_string();
    }
    match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent
            .join("fno-gh-coverage")
            .to_string_lossy()
            .into_owned(),
        None => "fno-gh-coverage".to_string(),
    }
}

pub(crate) fn is_graphql_read(read: &str) -> bool {
    matches!(
        read,
        "pr_view"
            | "pr_view_parse"
            | "pr_checks"
            | "pr_checks_parse"
            | "pr_reviews"
            | "pr_reviews_parse"
            | "pr_commits"
            | "pr_commits_parse"
    )
}

/// The self-teaching exhaustion message. A session that reads it must stop
/// retrying the GraphQL reads this window and know where the answer still
/// lives - anything less and it burns a fire every tick on a call that
/// cannot succeed until the reset.
pub(super) fn graphql_exhausted_reason(q: &GraphqlQuota) -> String {
    let now = Utc::now().timestamp();
    let mins = ((q.reset_epoch - now) / 60).max(0);
    format!(
        "GraphQL quota exhausted ({} remaining, resets in ~{}m). `gh pr view` / \
         `gh pr checks` cannot succeed until the reset: stop retrying them this \
         window. `fno do pr status <n>` still answers its CI verdict on the REST \
         budget (the optional review-thread check inside it is still GraphQL, \
         coalesced under its own TTL cache so a repeat poll costs nothing).",
        q.remaining, mins
    )
}
