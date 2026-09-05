//! The coverage verdict vocabulary: which channel a verdict rode in on,
//! what the verdict said, and how a local attestation was scoped. Serialization
//! shapes live with the enums; the counting rules stay in `loopcheck`.

use serde::{Deserialize, Serialize};

// ── review coverage (x-0eaf) ──────────────────────────────────────────────────
//
// The old gate's `reviewed` boolean was a claim about reviews computed
// entirely from what did NOT happen: nobody is still owed, no finding is
// outstanding, no reviewer is unattested. A quota refusal is dropped from
// `missing_bots` (PR #214) and reads as a pass; on a config with no required
// bots, nothing can object, so `reviewed` is true on zero reviews.
//
// Coverage is the missing predicate: did anyone actually review? It is a
// first-class value reported everywhere, never folded back into the objection
// boolean (collapsing it back undoes this node).
//
// Producer axis, not producer string. Two review producers share the display
// name "codex": the `chatgpt-codex-connector` GitHub App (posts review objects,
// can refuse on quota) and the local `codex` CLI (posts none, never rate-limited
// by the App's quota). They are told apart by `CoverageProducer`, never by the
// reviewer string (x-9ae8's one-word-two-entities disease). A third local lane,
// claude `/code-review`, shares the `LocalAttestation` axis.

/// The channel a review verdict came from. Two producers that share a name (the
/// `chatgpt-codex-connector` App vs the local `codex` CLI) are distinguished by
/// this axis, never by the reviewer string alone (x-9ae8, x-0eaf).
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

/// One verdict for one reviewer over one producer axis (x-0eaf). `reviewed` here
/// is derived from observed evidence, unlike the old boolean of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageVerdict {
    /// Posted a review object, or a `pass` attestation, against a commit whose
    /// code still matches HEAD (`Freshness::counts()`). The only verdict that
    /// counts toward coverage.
    Reviewed,
    /// Responded, but against a commit whose code no longer matches HEAD
    /// (x-5b99). Positive evidence that a reviewer READ AN OLDER COMMIT, which
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
