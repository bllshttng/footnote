//! What shape does a coverage answer take? Review info, coverage verdict types, and the coverage report.

use super::*;

/// Per-required-bot review verdict (grilled decision 5 / step 2).
#[derive(Debug)]
pub(super) struct ReviewInfo {
    /// Latest review/comment activity timestamp, or "none".
    pub(super) latest_ts: String,
    /// Required bots with no completed review pass. A pass is a top-level
    /// review with any non-empty state on ANY commit - in practice COMMENTED
    /// (verified on PR #447; codex reviews once per PR and never re-reviews,
    /// so requiring a pass on HEAD would make the gate unsatisfiable).
    pub(super) missing_bots: Vec<String>,
    /// Required bots that explicitly refused to review. A refusal is not a
    /// pass, but it is also not pending: it stays out of `missing_bots` so the
    /// stop gate can park the PR unless a fresh local review recovers it.
    pub(super) reviewer_refused: Vec<String>,
    /// Required bots whose best evidence pins a commit the freshness predicate
    /// no longer carries: (login, reviewed sha). Same gate weight as
    /// `missing_bots` - a stale bot still fails `all_required_passed` - but a
    /// different remedy, so it stays OUT of `missing_bots` and the block
    /// message names the sha it read and asks for a re-read.
    pub(super) stale_bots: Vec<(String, String)>,
}

impl ReviewInfo {
    /// Every required bot has at least one completed pass. Refused and stale
    /// reviewers fail this predicate; the caller separately recognizes a fresh
    /// local-review recovery from refusal.
    pub(super) fn all_required_passed(&self) -> bool {
        self.missing_bots.is_empty()
            && self.reviewer_refused.is_empty()
            && self.stale_bots.is_empty()
    }
}

/// Newest review-or-comment timestamp on a PR, or `"none"`.
///
/// Split out of `compute_review_info` because one caller (the no-progress
/// activity probe) wants ONLY this. Freshness does not and must not affect an
/// activity timestamp - a stale review is still activity - and giving that
/// caller the full ReviewInfo meant handing it a fabricated freshness resolver
/// whose verdicts nobody reads. A function that cannot return the other fields
/// makes that structural instead of a comment somebody later disbelieves.
pub(super) fn review_activity_ts(reviews_json: &Value) -> String {
    let mut latest = String::new();
    for (key, field) in [("reviews", "submittedAt"), ("comments", "createdAt")] {
        for item in reviews_json
            .get(key)
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
        {
            let ts = item.get(field).and_then(|v| v.as_str()).unwrap_or("");
            if !ts.is_empty() && ts > latest.as_str() {
                latest = ts.to_string();
            }
        }
    }
    if latest.is_empty() {
        "none".to_string()
    } else {
        latest
    }
}

pub(super) fn compute_review_info(
    reviews_json: &Value,
    required_bots: &[String],
    freshness: &dyn Fn(&str) -> Freshness,
) -> ReviewInfo {
    let reviews = reviews_json
        .get("reviews")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let comments = reviews_json
        .get("comments")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    let final_ts = review_activity_ts(reviews_json);

    // Each required bot's PRESENCE is the same question the coverage axis
    // asks, through the same ONE predicate (`bot_verdict`): a review object
    // that still counts, else a pinned clean-pass comment that still counts,
    // else an explicit refusal comment. Computing this with independent
    // scans is how `all_required_passed` reads true while coverage reads
    // `Refused` (or the inverse) and the run wedges between two gates that
    // never reconcile - the reader-divergence class exists to delete.
    // A STALE verdict lands in `stale_bots`, keeping the sha it read: the
    // remedy is a re-read, a different one from "has not reviewed", and the
    // block message must be able to say which. The gate weight is
    // identical either way. A refusal lands in `reviewer_refused`; without a
    // fresh local review the loop terminates via DoneAwaitingReview instead of
    // spinning. Scoped to the bot's own evidence inside bot_verdict, so a
    // stranger's comment never moves a required bot.
    let mut missing_bots: Vec<String> = Vec::new();
    let mut reviewer_refused: Vec<String> = Vec::new();
    let mut stale_bots: Vec<(String, String)> = Vec::new();
    for bot in required_bots {
        let (verdict, read_sha, _) = bot_verdict(bot, reviews, comments, freshness);
        match verdict {
            CoverageVerdict::Reviewed => {}
            CoverageVerdict::Stale => stale_bots.push((bot.clone(), read_sha)),
            CoverageVerdict::Refused => reviewer_refused.push(bot.clone()),
            _ => missing_bots.push(bot.clone()),
        }
    }

    ReviewInfo {
        latest_ts: final_ts,
        missing_bots,
        reviewer_refused,
        stale_bots,
    }
}

/// The channel a review verdict came from. Two producers that share a name (the
/// `chatgpt-codex-connector` App vs the local `codex` CLI) are distinguished by
/// this axis, never by the reviewer string alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageProducer {
    /// A GitHub App bot that posts review objects via the reviews API. Can
    /// refuse on quota (the `usage_markers` / `body_is_usage_limit` path).
    GithubApp,
    /// A local reviewer that leaves NO GitHub object and instead emits a
    /// head-pinned `review_attestation` event (`emit-attestation.sh`). Never
    /// rate-limited by any App's quota: `/code-review`, the codex CLI, sigma.
    LocalAttestation,
}

/// One verdict for one reviewer over one producer axis. `reviewed` here
/// is derived from observed evidence, unlike the old boolean of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageVerdict {
    /// Posted a review object, or a `pass` attestation, against a commit whose
    /// code still matches HEAD (`Freshness::counts()`). The only verdict that
    /// counts toward coverage.
    Reviewed,
    /// Responded, but against a commit whose code no longer matches HEAD
    ///. Positive evidence that a reviewer READ AN OLDER COMMIT, which
    /// is a different fact from `Absent` (never responded) and needs a
    /// different response: nudge for a re-read, do not wait for a first read.
    /// Recorded rather than dropped so the trail shows what happened; excluded
    /// from the count, because inheriting a verdict across a commit its author
    /// never saw is the defect this variant exists to make visible.
    Stale,
    /// Responded and declined to review. Quota exhaustion is the first known
    /// shape (detected by `body_is_usage_limit`). Positive evidence a reviewer
    /// exists and will not help - exactly what a nudge or lane failover needs.
    Refused,
    /// Responded with a failure / unparseable payload.
    Errored,
    /// A configured reviewer that produced no response.
    Absent,
}

/// A first-class coverage value, separate from the objection predicate. Never
/// 0-on-error: a failed read is `Unknown`, which behaves as 0 for the autonomous
/// refusal (fail closed) and is reported as "unknown" in the receipt (fail
/// honest). Collapsing an API error into 0 produces false refusals; collapsing
/// it into a count reproduces the bug.
///
/// NOTE: a FAILED GitHub reviews read still never yields
/// `Unknown` - `read_pr_info` returns `Err` and the caller block-retries
/// (fail-safe: the session retries, it does not green or merge). `Unknown` IS
/// reachable in production through the login_skipped arm: a `no_external`
/// session on a repo with an active login gate suppressed the reads the
/// config demanded, and the honest answer for that axis is Unknown with its
/// retry remedy (pinned by the no_external-on-active-gate tests). The
/// receipt, schema enum, and tests exist so softening the error path is a
/// one-line change, not a redesign. Do not delete it as dead code without
/// understanding this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// `n` reviewers reviewed (excludes human approvals until the operator says
    /// otherwise). `Covered(0)` is a real, known zero - distinct from `Unknown`.
    Covered(usize),
    Unknown,
}

impl Coverage {
    /// `true` iff at least one non-human review was observed. `Unknown` is
    /// false: the autonomous path treats unknown as not-covered (fail closed).
    pub fn is_covered(&self) -> bool {
        matches!(self, Coverage::Covered(n) if *n > 0)
    }
}

/// A known zero: the same value every no-information site already wrote.
impl Default for Coverage {
    fn default() -> Self {
        Coverage::Covered(0)
    }
}

/// Authorship of a local attestation lives in [`authorship`]: the enum, its
/// classifier, and the manifest fallback all answer one question.
///
/// How a local attestation was scoped to this PR: it is scopeable while
/// scope lasts (`attested_branch`: it named this PR's head branch, or it pins
/// this PR's exact head sha), or it predates the `branch` field and was
/// admitted on exact head equality alone (`legacy_head_match`). The second is
/// the one a refusal must NAME rather than silently drop: a pre-branch-field
/// attestation on a moved head is unscopeable, and a reader told only "0
/// reviewed" cannot tell it from nobody-ever-reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestationScope {
    AttestedBranch,
    LegacyHeadMatch,
}

/// One reviewer's classification, for the `review_coverage` event and receipts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerVerdict {
    pub producer: CoverageProducer,
    pub name: String,
    pub verdict: CoverageVerdict,
    /// A human GitHub approval (state `APPROVED`, non-bot author). Computed but
    /// EXCLUDED from the coverage count: whether it should count is the
    /// operator's call (lean: exclude; a solo self-approval is self-cert).
    /// One predicate flip in `CoverageReport::coverage_count` includes it.
    #[serde(skip_serializing_if = "is_false", default)]
    pub human_approval: bool,
    /// Whether this human approval's login is the PR author's (or the PR
    /// author could not be read, which asserts the same thing fail-closed).
    /// Only meaningful alongside `human_approval`: a `false` here is what
    /// lets a human approval count when `github_approval_satisfies` is on.
    /// GitHub refuses an author's own approval server-side; this field
    /// asserts the property the gate depends on rather than inferring it.
    #[serde(skip_serializing_if = "is_false", default)]
    pub author_approval: bool,
    /// Whether a local attestation was emitted by the authoring session
    /// (`SelfAttested`), a different one (`OtherSession`), or that is
    /// unknowable (`Unknown`). `coverage_count` never reads it: a
    /// `SelfAttested` verdict counts toward coverage exactly like any other,
    /// and a PR whose only attestation is self-attested is covered. Whether
    /// that should stay true is a later gate decision, not this field.
    /// Only meaningful on `local_attestation` verdicts; github_app and
    /// human approvals carry `Unknown` since a GitHub login has no session
    /// to compare. Defaults to `Unknown` so every pre-existing attestation
    /// lands there unchanged. `Unknown` serializes as `"unknown"`, never
    /// as an absent key: a consumer reading absent as "not self_attested"
    /// once cleared a PR whose only review was the author's own.
    #[serde(default = "default_attestation_origin")]
    pub attestation_origin: AttestationOrigin,
    /// The commit this reviewer actually read: a github_app review object's
    /// `.commit.oid`, or a local attestation's `data.head_sha`. Empty when
    /// unknowable (a review object with no commit, a verdict with no review),
    /// which [`review_freshness`] treats as `Stale` - fail closed.
    ///
    /// This is the field whose absence WAS the defect: the event pinned
    /// the head at EVAL time, so a bot verdict rendered twelve hours and two
    /// commits earlier serialized as coverage for a commit its author never
    /// saw.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub reviewed_sha: String,
    /// Whether `reviewed_sha` still describes the code at HEAD. `None` on a
    /// verdict with no review behind it (`Absent`, `Refused`), where there is
    /// nothing to be fresh or stale about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<Freshness>,
    /// How this verdict was scoped to the PR under evaluation. Only meaningful
    /// (and only serialized) on `local_attestation` verdicts: github_app
    /// evidence is scoped by being a review object ON this PR, so it needs no
    /// scope field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<AttestationScope>,
    /// The refusal class from the `review_invocation stage=refused` row this
    /// verdict was minted from (`empty_diff`, `unresolvable_base`). Only
    /// meaningful on `Refused` local verdicts: it is what lets a reader name
    /// WHY the attempt produced no verdict instead of guessing one diagnosis
    /// for both classes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal_reason: Option<String>,
    /// Positive fresh-context provenance from the attestation
    /// (`reviewer_context`: fresh | shared | unknown); None when the event
    /// predates the field. Measures context independence, never who invoked
    /// the verb. The coverage count never reads it; the posture verdict's
    /// `independent` component does (AC4-ERR: only a positive fresh
    /// marker satisfies rung 4 - `other_session` or unknown never does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_context: Option<String>,
    /// Whether a verdict from this reviewer was OWED: the login is in the
    /// resolved required set, or the verdict is a local-attestation lane the
    /// config floored. An optional GitHub App is honored if present and owed
    /// nothing, so its refusal must not rename an uncovered row after it.
    /// Absent on serialize when true, so every pre-field row reads REQUIRED and
    /// keeps today's exact semantics.
    #[serde(skip_serializing_if = "is_true", default = "default_true")]
    pub required: bool,
    /// Whether the review behind this verdict PASSED: a local attestation with
    /// `verdict == pass`, or a GitHub review object that approved. The counted
    /// axis (`Reviewed`) says the reviewer READ the head whatever it concluded
    ///, so the pass subset lives here and reaches the row only as the
    /// aggregate `passed_count` - never serialized per verdict, because no
    /// reader keys on it and the schema stays byte-stable.
    #[serde(skip)]
    pub passed: bool,
}

/// The one counting rule for human GitHub approvals: a `reviewed` verdict
/// counts when it is not a human approval, or when the resolved flag is on
/// AND the approver is provably not the PR author (`author_approval` false
/// requires a read PR author; an unreadable one asserts the exclude side).
pub(super) fn human_approval_counts(v: &ReviewerVerdict, flag: bool) -> bool {
    !v.human_approval || (flag && !v.author_approval)
}

/// The coverage over a PR plus the per-reviewer verdicts that produced it.
#[derive(Debug, Clone, Default)]
pub struct CoverageReport {
    pub coverage: Coverage,
    pub verdicts: Vec<ReviewerVerdict>,
    /// The resolved `config.review.github_approval_satisfies`, captured at
    /// classify time so every count on this report applies one rule. False on
    /// the bare `classify_coverage` spelling (today's semantics, for the
    /// unit-test corpus); the production call sites pass the resolved flag.
    pub github_approval_satisfies: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Reviewed,
    ReviewerRefused,
    Unreviewed,
}

impl CoverageReport {
    /// Count of `reviewed` verdicts, excluding human approvals. This is the one
    /// place that decides whether a human GitHub approval counts; flip the
    /// `!v.human_approval` guard to include them (the operator's deferred call).
    /// How many of the counted verdicts are the author attesting its own diff.
    /// Recorded, never gating - see `coverage_event_data` for why.
    pub fn self_attested_count(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| {
                v.verdict == CoverageVerdict::Reviewed
                    && human_approval_counts(v, self.github_approval_satisfies)
                    && v.attestation_origin == AttestationOrigin::SelfAttested
            })
            .count()
    }

    pub fn coverage_count(&self) -> Option<usize> {
        match &self.coverage {
            Coverage::Unknown => None,
            Coverage::Covered(_) => Some(
                self.verdicts
                    .iter()
                    .filter(|v| {
                        v.verdict == CoverageVerdict::Reviewed
                            && human_approval_counts(v, self.github_approval_satisfies)
                    })
                    .count(),
            ),
        }
    }

    /// The pass subset of the counted verdicts: reviews that concluded PASS,
    /// against [`Self::coverage_count`]'s "the reviewer read the head" whatever
    /// it concluded. At a discharged budget the two can differ by design - a
    /// spent round with declined findings reads reviewed without passing.
    pub fn passed_count(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| {
                v.verdict == CoverageVerdict::Reviewed
                    && human_approval_counts(v, self.github_approval_satisfies)
                    && v.passed
            })
            .count()
    }
}

pub(super) fn is_false(b: &bool) -> bool {
    !b
}

pub(super) fn is_true(b: &bool) -> bool {
    *b
}

pub(super) fn default_true() -> bool {
    true
}
