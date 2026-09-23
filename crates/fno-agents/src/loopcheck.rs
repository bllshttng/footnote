//! `fno-agents loop-check` verb (Task 1.1, ).
//!
//! Single entry-point decision-maker for the target stop hook. Reads external
//! state (manifest, transcript, git, gh, events, ledger) and returns a JSON
//! decision object. The manifest is NEVER mutated; the only write surface is
//! append-only event logs.
//!
//! Module name starts with "loop" to match the LOC-ratchet glob `crates/fno-agents/src/loop*`.

use crate::{
    cancel_sentinel::check_cancel_sentinel,
    check_supersession::latest_per_name,
    completion_output::{allow_output, paused_output},
    delivery_completion::pr_passes,
    disposition_gate::disposition_blockers_on_chain,
    king_termination::read_king_board,
};
// The integration tests reach the blocker predicates through loopcheck, the
// facade they have always imported from; the predicates live in
// disposition_gate since the line-budget refactor moved them there.
// DispositionBlocker rides the pub use too: it is the return type of the
// facade's `disposition_blockers`, and a private import made it unnameable
// through the facade it is published under.
use crate::acceptance_evidence::{evaluate_done_probes, ProbeGate, PROBE_TIMEOUT};
use crate::bounded_spawn::{kill_process_group, killpg};
pub use crate::disposition_gate::{blockers_withhold, DispositionBlocker};
use crate::king_termination::{bound_breached, king_output, king_quiet_body};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
// ── public types ──────────────────────────────────────────────────────────────

/// Why the loop terminated. Serialized as the exact string enum the spec names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminationReason {
    DonePRGreen,
    DoneAdvisory,
    DoneDelivery,
    /// A batch-lane member (batch-lane Wave 2/3): its commits live on a shared
    /// batch branch and ship via the batch PR, not its own, so there is no
    /// per-node PR to go green. Terminal, but NOT a ship reason - the batch's
    /// own `/pr create` graduates the plan; a member must not.
    DoneBatched,
    /// Work complete (PR open, mergeable, reviewed, HEAD shipped) but `done()`
    /// fails SOLELY on CI-green because main itself is red on the same checks,
    /// and a bg agent cannot merge. Proven pre-existing main-red (strict
    /// check-name subset against current main HEAD) terminates the loop with a
    /// one-shot merge-recommendation notify instead of burning to NoProgress.
    /// Terminal, but NOT a ship reason (like DoneBatched): never merges, never
    /// marks the node done - a human merge then the out-of-band-merge reconcile
    /// path closes it, and DonePRGreen always wins when observable.
    DoneAwaitingMerge,
    /// A PR is green, mergeable, and nothing objected - but nothing reviewed it
    /// either (coverage 0 or Unknown; the old conjuncts all asked "did anyone
    /// object", never "did anyone review", ). Terminal on the first
    /// evaluation (no iteration spent waiting) and NOT a ship reason, shaped
    /// like `DoneAwaitingMerge`: `should_arm_auto_merge` arms only on
    /// `DonePRGreen`, so a human merge plus reconcile closes it. The
    /// discriminator is coverage, NOT the `attended` manifest field (:
    /// that field lies for spawned workers).
    DoneUnreviewed,
    /// Work complete (PR open, green, HEAD shipped) but `done()` fails because a
    /// required review bot is rate-limited: it posted a usage-limit (quota)
    /// comment instead of a review, so the gate cannot be auto-satisfied. The
    /// agent cannot make a rate-limited bot recover, so holding would wedge to
    /// budget death (the PR #214 shape); instead the loop terminates cleanly.
    /// Terminal, but NOT a ship reason (like DoneAwaitingMerge): never merges,
    /// never graduates - a human merges after a real review (or quota recovery
    /// / a local review and a re-run), then the out-of-band-merge reconcile
    /// path closes it. This is the fail-closed flip of the old "drop the bot
    /// and proceed" behavior that let ~10 PRs (#890-#912) merge unreviewed
    ///.
    DoneAwaitingReview,
    /// A plan-only thread reached the plan boundary cleanly (manifest `planned`
    /// flag + a promise). It produced planning output, not a delivery, so it is
    /// terminal but deliberately NOT a ship reason (out of finalize.SHIP_REASONS
    /// -> no plan stamp/graduate) and NOT a postmortem reason (a plan is not
    /// stuck). Benign like NoWork; distinct from DoneAdvisory, which DOES
    /// graduate. The scoreboard's `planned` bucket is keyed on the phase set,
    /// never on this terminal.
    DonePlanned,
    NoWork,
    Budget,
    NoProgress,
    /// A node held on an open operator question: the first fire blocked once
    /// naming the question and the decide verb; this fire is the second on
    /// the same still-open question, so the loop terminates instead of
    /// re-asking. The journal (not the fingerprint) carries the held state.
    /// Terminal, NOT a ship reason: the operator answers, a later run ships.
    HeldOnQuestion,
    Interrupted,
    Aborted,
}

// ── git / gh helpers ──────────────────────────────────────────────────────────

/// PR state vocabulary (fu-4faa3d). Parsed once at the read_pr_info boundary.
/// `as_str()` reproduces the exact legacy strings so the fingerprint (which
/// persists across fires in events.jsonl) stays byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PrState {
    Open,
    Merged,
    Closed,
    /// No PR, or an unrecognized gh state string (fail-closed, AC5-EDGE).
    #[default]
    None,
}

impl PrState {
    fn from_gh_str(s: &str) -> Self {
        match s {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            PrState::Open => "OPEN",
            PrState::Merged => "MERGED",
            PrState::Closed => "CLOSED",
            PrState::None => "none",
        }
    }

    fn is_open_or_merged(&self) -> bool {
        matches!(self, PrState::Open | PrState::Merged)
    }
}

/// CI conclusion vocabulary (fu-4faa3d). `render()` reproduces the exact
/// legacy strings ("FAILURE:{name}" carries the failing check name).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum CiConclusion {
    Success,
    /// Failing check name when one was identified.
    Failure(Option<String>),
    Pending,
    /// CI read skipped via ci.declared_none.
    Skipped,
    /// No checks found (fail-closed unless declared_none).
    #[default]
    None,
}

impl CiConclusion {
    fn render(&self) -> String {
        match self {
            CiConclusion::Success => "SUCCESS".to_string(),
            CiConclusion::Failure(Some(name)) => format!("FAILURE:{name}"),
            CiConclusion::Failure(None) => "FAILURE".to_string(),
            CiConclusion::Pending => "PENDING".to_string(),
            CiConclusion::Skipped => "skipped".to_string(),
            CiConclusion::None => "none".to_string(),
        }
    }

    fn is_ok(&self) -> bool {
        matches!(self, CiConclusion::Success | CiConclusion::Skipped)
    }
}

#[derive(Debug, Default)]
struct PrInfo {
    state: PrState,
    number: i64,
    /// PR head commit OID; must match local HEAD for DonePRGreen (codex P1
    /// on #447: a green PR must not complete a session with unpushed work).
    head_oid: String,
    ci_conclusion: CiConclusion,
    /// Every failing check/job name on the PR head (bucket fail|cancel), at the
    /// same granularity as `gh pr checks .name`. Feeds the DoneAwaitingMerge
    /// subset rule against main's failing set. Empty when CI is green/pending.
    failing_checks: Vec<String>,
    /// True iff any check on the PR head is still pending (a non-terminal
    /// bucket). `ci_conclusion` reports `Failure` as soon as ONE check fails even
    /// while others run, so the DoneAwaitingMerge terminal must consult this to
    /// avoid firing while the session's own in-flight job could still turn red.
    ci_has_pending: bool,
    /// GitHub mergeable state ("MERGEABLE" | "CONFLICTING" | "UNKNOWN"). The
    /// DoneAwaitingMerge terminal must not fire on a "CONFLICTING" PR: the human
    /// cannot merge past main-red until the branch is rebased, and the terminal
    /// would drop the node from retry circulation while it is un-mergeable.
    mergeable: String,
    /// Live merge-slot holder for this PR's base ref when ANOTHER PR holds it.
    /// A fail-open read of the local claims store - no GitHub spend.
    /// None on no hold, a self-held slot, or an unreadable store.
    merge_slot_holder: Option<u64>,
    /// GitHub `mergeStateStatus` == BEHIND (REST `mergeable_state` == behind):
    /// the base moved past this PR's head, so a rebase is work to do now and a
    /// merge-slot hold must not idle: the refusal stays for a hold the
    /// session can act on. Absent on either payload reads as false.
    base_behind: bool,
    /// Newest review/comment/inline-comment activity (ISO8601 or "none");
    /// folded into the fingerprint's 4th component on done() fires.
    latest_review_ts: String,
    reviewed: bool, // every required bot passed AND no unaddressed blocking finding
    /// Required bots with no completed review pass (names the gap in the
    /// block message, AC1-UI).
    missing_bots: Vec<String>,
    /// Per-missing-bot nudge classification for this fire, same order
    /// as `missing_bots`. Empty when the review reads were skipped or there is no
    /// PR. `missing_bots` stays the gate; this only changes idling and messaging.
    /// An EMPTY list with a non-empty `missing_bots` means "not classified" and
    /// is treated exactly like today (every missing bot idlable, today's string).
    bot_nudges: Vec<BotNudge>,
    /// Required bots whose best evidence READ AN OLDER COMMIT
    /// (`CoverageVerdict::Stale`): (login, reviewed sha). Same gate weight as
    /// `missing_bots` - a stale bot still fails `all_required_passed` - but a
    /// different remedy, so the block message names the sha it read and asks
    /// for a re-read instead of a first read. Nudge-classified
    /// alongside the missing bots so the re-read ask idles like one.
    stale_bots: Vec<(String, String)>,
    /// Blocking inline findings (codex P1 / gemini critical|high) whose
    /// thread has no qualifying ack (AC2).
    unaddressed_findings: Vec<Finding>,
    /// Reads 3+4 were skipped (per-session no_external OR the repo declared
    /// `required_bots: []`). Recorded in loop_check events so the skip is
    /// observable, not silently absent (AC3-UI).
    review_skipped: bool,
    /// Configured `config.review.reviewers` with no head-pinned attestation.
    /// The sole failing term whenever the login gate is vacuous, and the reason
    /// the block message can name real local work instead of an absent bot.
    unattested_reviewers: Vec<UnattestedReviewer>,
    /// Unparseable events.jsonl lines that carry the literal
    /// `review_attestation`. Named in the reason so a corrupt attestation is
    /// not silently dropped. Not exhaustive by construction: a write torn
    /// before that token cannot be recognized at all.
    malformed_attestations: usize,
    /// Review coverage: did anyone actually review, distinct from `reviewed`
    /// (did anyone object). Computed at read time from observed evidence
    /// across two producer axes (github_app review objects; local_attestation
    /// head-pinned passes). Terminal selection consumes this: a run that would
    /// report `DonePRGreen` at coverage 0/Unknown reports `DoneUnreviewed`
    /// instead. Never cached, never inferred from `reviewed`.
    coverage: CoverageReport,
    /// The resolved review-posture verdict , computed alongside
    /// coverage when the caller supplied a resolved `review.posture`. None on
    /// the no-PR early return and on callers without settings context.
    posture: Option<PostureVerdict>,
    /// The attestation-chain range tiling computed for this read, carried so
    /// the standalone verb's stdout payload equals the row read_pr_info
    /// emitted, field for field (payload parity). Default on every early
    /// return and test fixture: no chain, no rescue.
    range_tiling: RangeTiling,
}

pub use crate::review_freshness::{
    freshness_rank, review_freshness, CodeDiffIdentity, Freshness, FreshnessFacts,
    FreshnessResolver,
};

// Child modules named by their question (the file budget's remedy): the
// coverage row's state deriver, the receipt line, and attestation authorship
// live there, not here.
mod async_wait;
mod attestation_journal;
mod authorship;
mod awaiting_merge;
mod coverage_receipt;
mod holds;
mod king_decide;
mod range_tiling;
mod session_binding;
pub use range_tiling::{compute_range_tiling, RangeTiling};
mod review_count;
mod review_state;
use attestation_journal::missing_global_attestations;
pub use attestation_journal::unattested_reviewers_scan_text;
mod args;
mod block_reason;
mod bot_nudge;
mod bot_verdict;
mod bounded_run;
mod budget;
mod ci_checks;
mod coverage;
mod coverage_classify;
mod coverage_status;
mod findings;
mod gh_read;
mod intent;
mod local_attestation;
mod plan_fidelity;
mod posture;
mod review_coverage_verb;
mod review_inputs;
mod self_review_floor;
mod settings;
mod watch_lease;
pub(crate) use args::{parse_args, try_flag_value, LoopCheckArgs};
use async_wait::{arm_watch_hint, async_wait_class, conflicting_reason, merge_slot_reason};
use authorship::carry_author_session_forward;
pub use authorship::AttestationOrigin;
use authorship::{classify_attestation_origin, default_attestation_origin};
pub(crate) use awaiting_merge::main_head_failing_checks;
use block_reason::{build_block_reason, sized_self_review_hint};
use bot_nudge::{
    classify_bot_nudge, logins_correspond, nudge_class_idlable, nudge_config_for,
    nudge_giveup_message, post_nudge_comment, profile_by_author, resolved_nudge_configs,
    unresponsive_bot, BotNudge, NudgeClass, NudgeConfig, BOT_PROFILES, MAX_NUDGE_CEILING,
    MAX_NUDGE_WAIT_MINUTES,
};
pub(crate) use bot_verdict::bot_verdict;
use bot_verdict::{clean_pass_review, usage_limit_comment_by};
#[cfg(test)]
use bounded_run::BOUNDED_STDERR_TAIL_CAP;
pub(crate) use bounded_run::{
    bounded_read, bounded_read_diagnostic, git_bounded, log_bounded_read_error, BoundedOutput,
    GhReadError,
};
use bounded_run::{probe_gh_bin, run_bounded, BoundedRun, GhProbeOutcome, ReadErrorKind};
use budget::{check_budget, BudgetTrip};
pub(crate) use ci_checks::classify_checks_payload;
use ci_checks::{awaiting_review_only, local_recovery_from_refusal};
#[cfg(test)]
use ci_checks::{
    ci_has_pending_checks, compute_ci_conclusion, failing_check_names, without_coverage_statuses,
};
#[cfg(test)]
use coverage::review_activity_ts;
use coverage::{compute_review_info, human_approval_counts};
pub use coverage::{
    AttestationScope, Coverage, CoverageProducer, CoverageReport, CoverageVerdict, ReviewState,
    ReviewerVerdict,
};
use coverage_classify::coverage_event_data_full;
pub use coverage_classify::{
    classify_coverage, classify_coverage_tiled, coverage_event_data, coverage_event_data_tiled,
};
pub use coverage_receipt::coverage_receipt_line;
pub use coverage_status::operator_waiver;
#[cfg(test)]
use coverage_status::{coverage_instrument_status, uncovered_status_description};
use coverage_status::{
    coverage_unavailable_description, publish_coverage_status, COVERAGE_STATUS_CONTEXT,
    COVERAGE_UNAVAILABLE_STATUS_CONTEXT,
};
use findings::{blocking_severity, compute_unaddressed_findings, max_ts, ts_after, Finding};
#[cfg(test)]
use gh_read::is_no_pr_stderr;
use gh_read::{
    attestation_in_scope, git_head_branch, git_head_sha, graphql_exhausted_reason, head_is_shipped,
    internal_gh_adapter, pr_head_oid, probe_graphql_quota, read_pr_head_oid, read_pr_view,
    refusal_is_secondary, stderr_smells_rate_limit, stderr_tail, GraphqlQuota,
};
pub(crate) use gh_read::{coverage_adapter, is_graphql_read};
pub(crate) use intent::parse_xml_attr;
#[cfg(test)]
use intent::INTENT_LOOKBACK_ENTRIES;
use intent::{detect_intent, extract_last_assistant_message, Intent};
use local_attestation::{
    author_is_bot, author_is_known_bot, in_scope_chain, line_carries_keyed_findings,
    local_attestation_verdict, local_latest_attestations, local_refused_verdicts,
    zero_evidence_attestation, LocalPass,
};
pub use local_attestation::{disposition_blockers, mark_owed_verdicts, rounds_since_last_pass};
#[cfg(test)]
use plan_fidelity::classify_plan_fidelity;
use plan_fidelity::{evaluate_plan_fidelity, FidelityGate, FIDELITY_TIMEOUT};
#[cfg(test)]
use posture::resolved_required_bots;
use posture::{
    carry_interdiff_lines_resolved, is_bot_reviewer, login_equals, login_matches_bot,
    posture_components, posture_verdict, resolve_posture_config,
    resolved_local_peer_reviewers_for_author, resolved_optional_bots,
    resolved_required_bots_for_author, PostureConfig, PostureVerdict, LOCAL_PEER_REVIEWER,
    SAME_MODEL_LOCAL_PEER_SENTINEL, SAME_MODEL_PEER_SENTINEL,
};
pub use review_coverage_verb::{run_review_coverage, run_review_coverage_capture};
pub(crate) use review_inputs::resolve_review_inputs;
pub(crate) use self_review_floor::is_documentation_path;
use self_review_floor::{
    classify_payload_for_floor, floor_self_review, reviewer_invocation_for,
    self_review_floor_applies, REVIEW_ORDER,
};
use settings::{
    fail_closed_settings, normalize_reviewer, parse_manifest, parse_settings_result,
    session_cost_from_ledger, Manifest, PeerEntry,
};
#[cfg(test)]
pub(crate) use settings::{parse_settings, value_as_probe_list};
#[cfg(test)]
use settings::{scalar_as_singleton, MALFORMED_REVIEWERS_SENTINEL, UNPARSEABLE_SETTINGS_SENTINEL};
pub(crate) use settings::{scan_manifest_field, Settings};
use watch_lease::{harness_can_idle, watch_window_ms};

/// A configured local reviewer with no head-pinned `pass` attestation.
#[derive(Debug, Clone, PartialEq)]
pub struct UnattestedReviewer {
    name: String,
    /// A head this reviewer DID attest at, which is no longer HEAD. Always a
    /// PASS and never empty - normalized at construction so `Some` means
    /// "there is a real prior pass to name", not "check is_empty() first".
    /// Without it the block message reads as "you never ran sigma" to a session
    /// that ran sigma and then pushed a commit, losing turns twice.
    superseded_head: Option<String>,
    /// This reviewer DID attest at the current head, and the verdict was not
    /// `pass`. "No attestation exists" would be a lie to a session that ran the
    /// reviewer and was told no.
    failed_at_head: bool,
}

/// The committed event lines for one journal family: the store's rows in
/// commit order, pre-cutover bytes imported first (hash-dedupe free).
pub(crate) fn event_lines(journal: &Path) -> Result<Vec<String>, String> {
    crate::event_store::import_all(journal)?;
    let q = crate::event_store::EventQuery {
        include_rejected: true,
        ..Default::default()
    };
    let rows = crate::event_store::query_events(journal, &q)?;
    Ok(rows.into_iter().map(|r| r.line).collect())
}

/// The `config.review.reviewers` entries NOT satisfied by a head-pinned
/// `review_attestation` event. A
/// reviewer is satisfied when events.jsonl carries a line with
/// `type == "review_attestation"`, `data.reviewer` matching (leading '/'
/// stripped on both sides), the line in scope for this PR
/// (`attestation_in_scope`: `data.branch == head_branch`, or the exact head
/// sha whatever branch it emitted on - a legacy line with no branch counts
/// only on that exact-head arm), and `data.verdict == "pass"`.
///
/// The gate reads `.is_empty()` and the block message reads the names, so the
/// decision and the explanation come from ONE scan. When they came from two,
/// the message told sessions to wait on a bot that was never required.
///
/// Fail closed everywhere: an empty/unreadable events file, a stale head_sha
/// (attestation for a prior commit), or a `fail` verdict leaves the reviewer
/// UNSATISFIED - except 's ONE softening directly below. An empty
/// reviewer list is vacuously satisfied (no reviewers gate).
///: the one softening of the `fail` arm - a `fail` whose own chain
/// raised keyed findings that are all terminally dispositioned ANSWERS this
/// head, so it satisfies the reviewer exactly like a pass ("answered at this
/// head", never "clean at this head"). A findings-free fail and a RETRACTION
/// never satisfy (the bystander and revoked-pass shapes stay unsatisfied).
/// Authorship is unknowable inside this scan, so the disposition read is
/// fail-open on origin; the `reviewed` conjunction re-runs the
/// disposition scan WITH authorship downstream and a solo author's decline
/// still withholds there.
/// `unattested_reviewers` plus the count of unparseable lines that LOOK like
/// attestations. A torn write leaves a corrupt `review_attestation` in the file
/// and the gate then reports "no head-pinned review_attestation", which is the
/// same class of lie this node exists to delete - so the count is surfaced in
/// the reason. Mirrors `open_review_findings`, which already does this for
/// `review_finding`.
pub fn unattested_reviewers_scan(
    events_path: &Path,
    reviewers: &[String],
    freshness: &dyn Fn(&str) -> Freshness,
    head_branch: &str,
    head_sha: &str,
    rounds_exhausted: bool,
) -> (Vec<UnattestedReviewer>, usize) {
    // no committed evidence -> gate unmet (fail closed); an unreadable store
    // is the same shape, never an empty-but-satisfied read
    let content = match event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => {
            let unsatisfied = reviewers
                .iter()
                .map(|r| UnattestedReviewer {
                    name: r.trim_start_matches('/').to_string(),
                    superseded_head: None,
                    failed_at_head: false,
                })
                .collect();
            return (unsatisfied, 0);
        }
    };
    unattested_reviewers_scan_text(
        &content,
        reviewers,
        freshness,
        head_branch,
        head_sha,
        rounds_exhausted,
    )
}

/// An operator review finding still open: a `review_finding` event for
/// the node with no later `review_finding_resolved` for the same id.
#[derive(Debug, Clone)]
struct OpenFinding {
    id: String,
    first_line: String,
}

/// Scan events.jsonl for OPEN operator review findings scoped to `node`.
///
/// Returns `(open findings sorted by id, malformed-line count)`. A finding is
/// open until an explicit `review_finding_resolved` clears it - node-scoped and
/// NOT head-pinned, so a new commit never auto-clears an operator's comment
/// (Locked Decision 2). Malformed finding lines notice-not-block (AC3-FR): a
/// line that is unparseable JSON but carries the literal `review_finding`, or a
/// parsed `review_finding` missing its id, is our own writer's corrupted output;
/// it is counted for the deny/audit notice but NEVER holds the gate. Any read
/// failure yields no findings (the gate is only ADDED by evidence, never
/// invented from an unreadable file).
fn open_review_findings(events_path: &Path, node: &str) -> (Vec<OpenFinding>, usize) {
    // Any read failure yields no findings (the gate is only ADDED by
    // evidence, never invented from an unreadable store).
    let content = match event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => return (Vec::new(), 0),
    };
    // Preserve first-seen order via a Vec of (id, first_line); a later duplicate
    // id (shouldn't happen - ids are minted) just refreshes the first_line.
    let mut findings: Vec<(String, String)> = Vec::new();
    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut malformed = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            // Only OUR corrupted output counts toward the notice; unrelated
            // corruption from another writer is not a finding concern.
            if line.contains("review_finding") {
                malformed += 1;
            }
            continue;
        };
        match val.get("type").and_then(|v| v.as_str()) {
            Some("review_finding") => {
                if val.pointer("/data/node").and_then(|v| v.as_str()) != Some(node) {
                    continue;
                }
                match val.pointer("/data/finding_id").and_then(|v| v.as_str()) {
                    Some(id) => {
                        let first = val
                            .pointer("/data/text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        if let Some(slot) = findings.iter_mut().find(|(fid, _)| fid == id) {
                            slot.1 = first;
                        } else {
                            findings.push((id.to_string(), first));
                        }
                    }
                    None => malformed += 1, // review_finding without an id
                }
            }
            Some("review_finding_resolved") => {
                if let Some(id) = val.pointer("/data/finding_id").and_then(|v| v.as_str()) {
                    resolved.insert(id.to_string());
                }
            }
            _ => {}
        }
    }
    let mut open: Vec<OpenFinding> = findings
        .into_iter()
        .filter(|(id, _)| !resolved.contains(id))
        .map(|(id, first_line)| OpenFinding { id, first_line })
        .collect();
    open.sort_by(|a, b| a.id.cmp(&b.id)); // deterministic deny reason
    (open, malformed)
}

/// Deny reason for an open-finding gate: quote the first finding (id + first
/// line) + the resolve remedy, plus a `[+N more]` count and any malformed-line
/// notice so nothing vanishes silently.
fn build_findings_block_reason(open: &[OpenFinding], malformed: usize) -> String {
    let f = &open[0];
    let more = if open.len() > 1 {
        format!(" [+{} more]", open.len() - 1)
    } else {
        String::new()
    };
    let notice = if malformed > 0 {
        format!(" ({malformed} malformed finding line(s) ignored)")
    } else {
        String::new()
    };
    format!(
        "open review finding {}: {} - address it, then `fno backlog annotate resolve {}`{}{}",
        f.id, f.first_line, f.id, more, notice
    )
}

/// A `Covered(0)` that rests on a commit the object store could not measure
/// is not a known zero - it is an unread. Demote it to `Unknown` so the row
/// publishes pending and the gate recomputes it on its next read (the
/// recompute fetches the commit through the resolver). A `Covered(n > 0)` is
/// real reviews counted and is never demoted, which also keeps the
/// spent-budget discharge (always `n >= 1`) intact.
fn demote_unmeasured_coverage(coverage: &mut Coverage, resolver: &FreshnessResolver) {
    if matches!(coverage, Coverage::Covered(0)) && !resolver.unmeasured().is_empty() {
        *coverage = Coverage::Unknown;
    }
}

/// The local attestation axis: every project-log rotation PLUS the global
/// journal's slug-scoped attestations. A review fork emits into its own
/// checkout's project log and mirrors to the global journal, and when the
/// fork's checkout dies the mirror alone survives (measured on PR 2137:
/// three attestations for one head, zero copies in any surviving project
/// log). Mirrors of rows the project log still holds are deduped, so a round
/// is never counted twice. An unreadable journal degrades to project-only.
fn review_journal_text(events_path: &Path, global_events_path: &Path, repo_slug: &str) -> String {
    let project_text = crate::events_store::review_text(events_path);
    let global_text = crate::event_store::review_text(global_events_path);
    let extra_global = missing_global_attestations(&global_text, &project_text, repo_slug);
    if extra_global.is_empty() {
        project_text
    } else {
        format!("{project_text}\n{extra_global}")
    }
}

/// Run done() reads. Returns Ok(PrInfo) or Err((read_name, stderr_tail)) on gh failure.
#[allow(clippy::too_many_arguments)]
fn read_pr_info(
    gh_bin: &str,
    git_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    optional_bots: &[String],
    optional_lane_configured: bool,
    external_reviewers: &[String],
    reviewers: &[String],
    nudge_configs: &[NudgeConfig],
    head_sha: &str,
    events_path: &Path,
    global_events_path: &Path,
    repo_slug: &str,
    author_session: Option<&str>,
    pr_selector: Option<&str>,
    prefetched_pr_json: Option<Value>,
    github_approval_satisfies: bool,
    max_rounds: i64,
    // The resolved `review.carry_interdiff_lines` (law d-608344c1): how many
    // interdiff lines a rebase or fix may add and still carry an attestation.
    // `0` disables the arm. Resolved by the caller from settings, default 100.
    carry_interdiff_lines: usize,
    // The resolved `review.posture` , computed by the caller from the
    // parsed settings. None on callers that have no settings context.
    posture: Option<&PostureConfig>,
    // The local reviewer names allowed to satisfy the peer posture component
    // (the cross-model-resolved set; the same-model sentinel never matches).
    peer_reviewers: &[String],
) -> Result<PrInfo, GhReadError> {
    let rest_adapter = internal_gh_adapter(gh_bin);
    let checks_read = if rest_adapter {
        "pr_status_rest"
    } else {
        "pr_checks"
    };
    let checks_parse = if rest_adapter {
        "pr_status_rest_parse"
    } else {
        "pr_checks_parse"
    };
    // An explicit PR selector for the branch-resolved gh calls:
    // Some(n) inserts the number (`gh pr view <n>`, `gh pr checks <n>`) so the
    // standalone review-coverage verb can evaluate a PR from a checkout that is
    // NOT on its branch (`fno do pr merge <n>` from canonical); None keeps the
    // argv byte-identical to the stop hook's branch-resolved form. The one
    // number-based call (`gh api .../pulls/<n>/comments`) already carries the
    // number the first read returned.
    let sel: Vec<&str> = pr_selector.into_iter().collect();
    // Read 1: PR state + number + head OID + mergeability. Reuse the caller's
    // read when it already resolved this exact selector (e.g. review-coverage
    // pinning --pr N via read_pr_head_oid) instead of asking gh again.
    let Some(pr_json) = (match prefetched_pr_json {
        Some(json) => Some(json),
        None => read_pr_view(gh_bin, cwd, pr_selector)?,
    }) else {
        // No PR yet: world-state, not an error. done() is simply false, and
        // the backstop can resolve a stuck no-PR session as NoProgress. Every
        // omitted field is PrInfo's no-information default.
        return Ok(PrInfo {
            mergeable: "UNKNOWN".to_string(),
            ..PrInfo::default()
        });
    };

    let state = PrState::from_gh_str(
        pr_json
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("none"),
    );
    let number = pr_json.get("number").and_then(|v| v.as_i64()).unwrap_or(0);
    let head_oid = pr_head_oid(&pr_json).unwrap_or_default();
    // The PR's head branch, same `gh pr view` round trip as headRefOid. The
    // scope predicate needs it: attestations are keyed to the branch they
    // reviewed, and an empty read must pass "" so the predicate fails closed
    // onto exact head equality.
    let head_branch = pr_json
        .get("headRefName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // The PR author's login, for the human-approval counting rule: an
    // approval counts only when its login provably is not this one. An
    // absent/unreadable author is None, which the rule reads fail-closed
    // (exclude the approval from the count, never include it on a guess).
    let pr_author = pr_json
        .pointer("/author/login")
        .and_then(|v| v.as_str())
        .filter(|a| !a.is_empty())
        .map(str::to_string);
    // GitHub's mergeable state: "MERGEABLE" | "CONFLICTING" | "UNKNOWN" (still
    // computing). Only "CONFLICTING" is a definitive no; UNKNOWN must not hold
    // the terminal (it clears on its own). Missing field -> "UNKNOWN".
    let mergeable = pr_json
        .get("mergeable")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    // BEHIND on either payload spelling (GraphQL `mergeStateStatus`, REST
    // `mergeable_state`): the base moved past this head, so a rebase is work
    // to do now. Absent on both reads as false (fail-open to idlable).
    let base_behind = ["mergeStateStatus", "mergeable_state"].iter().any(|k| {
        pr_json
            .get(*k)
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("behind"))
            .unwrap_or(false)
    });

    // One freshness resolver for every reviewer on this PR.
    // Both producers and both presence scans read it, so there is one rule
    // rather than the two divergent ones this replaces. Memoized per reviewed
    // sha, and the HEAD identity is computed lazily, so a PR whose reviewers
    // are all at HEAD (the common case) pays no git at all.
    let base_ref = pr_json
        .get("baseRefName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resolver = FreshnessResolver::new(git_bin, cwd, base_ref, head_sha, carry_interdiff_lines);
    // Fetch a head the store cannot read before anything judges it: a
    // server-side rebase publishes the new head on GitHub before the next
    // local fetch, and an absent head reads `None`, then stale, then a stored
    // uncovered row that never recomputes. Memoized, so at most one fetch.
    resolver.ensure_local(head_sha);
    let freshness = |sha: &str| resolver.freshness(sha);

    // The range-tiling answer for this PR's attestation chain, computed ONCE
    // and shared by every consumer below (the classify_coverage local axis,
    // the emitted review_coverage row). The local attestation axis reads the
    // project rotations plus the global journal's slug-scoped mirrors; any
    // git failure answers not-tiled and today's rule stands alone.
    let events_text = review_journal_text(events_path, global_events_path, repo_slug);
    let mut tiling = compute_range_tiling(
        git_bin,
        cwd,
        base_ref,
        &events_text,
        &head_branch,
        head_sha,
        max_rounds,
        // The same resolver the per-verdict axis built above, same base_ref
        // and head_sha: the carry costs no git call the per-verdict axis was
        // not already making.
        Some(&resolver),
    );

    // (E): a MERGED PR is terminal. A PR merged out-of-band (GitHub
    // web/mobile, or `gh pr merge`) is done regardless of whether the required
    // bot ever reviewed it or whether CI is still green post-merge - the merge
    // IS the authority. Short-circuit the now-irrelevant CI + review polls
    // (which also avoids a transient gh blip on those reads re-blocking a
    // finished session). The single merge signal is `state` from the same
    // `gh pr view` call that `reconcile`/`fno do pr verify` read - one signal, not
    // two independently-polled sources. done()'s `head_shipped` guard still
    // applies downstream: an unpushed commit on top of a merged PR stays
    // unshipped work.
    if state == PrState::Merged {
        return Ok(PrInfo {
            state,
            number,
            head_oid,
            ci_conclusion: CiConclusion::Skipped,
            mergeable,
            reviewed: true,
            review_skipped: true,
            ..PrInfo::default()
        });
    }

    // Read 2: CI checks. Compute the conclusion, the full failing-check-name set,
    // AND whether any check is still pending from the same payload (the set feeds
    // the DoneAwaitingMerge subset rule; the pending flag gates that terminal so
    // it never fires on partial CI).
    let no_hosted_ci =
        crate::verify_evidence::hosted_ci_not_configured(ci_declared_none, cwd, head_sha);
    let (ci_conclusion, failing_checks, ci_has_pending) = if no_hosted_ci {
        (CiConclusion::Skipped, Vec::new(), false)
    } else {
        let mut checks_args = vec!["pr", "checks"];
        checks_args.extend(sel.iter().copied());
        checks_args.extend(["--json", "name,state,bucket,startedAt,workflow"]);
        let checks_out = bounded_read(
            gh_bin.as_ref(),
            &checks_args,
            cwd,
            checks_read,
            stopgate_read_timeout(),
        )?;

        if !checks_out.status.success() {
            return Err(GhReadError::failed(
                checks_read,
                stderr_tail(&checks_out.stderr_tail),
            ));
        }

        let checks: Value = serde_json::from_slice(&checks_out.stdout)
            .map_err(|_| GhReadError::parse_failed(checks_parse))?;
        // One truth table for the payload (classify_checks_payload): the
        // conclusion, the failing-name set, and the pending flag can never
        // answer off different rollups.
        classify_checks_payload(&checks).map_err(|e| GhReadError::parse_failed(&e))?
    };

    // Reads 3+4: reviews + inline findings. Skipped when the session declares
    // no_external OR the repo declares `required_bots: []` (the no-review-gate
    // path, US3 - mirrors ci.declared_none; PR + CI carry the gate). The two
    // skips are orthogonal: one is per-session, the other repo config.
    // Skip the review reads only when there is NOTHING to honor: no required
    // login AND no optional login. An optional-only gate still reads (to catch
    // an optional blocking finding), but its presence is never required.
    //: the gate is a strict conjunction over the union of GitHub-login
    // evidence (github_apps/peers via optional_bots+required_bots) AND the
    // local-attestation `reviewers`. Each satisfied by its own evidence source,
    // so the two skips are INDEPENDENT: `no_external` (and an empty login set)
    // skips only the EXTERNAL GitHub-login reads - it is scoped to external
    // review (control-plane-loop.md step 2), NOT the local attestation gate. A
    // repo that pins `reviewers: [sigma]` still requires that local pass even
    // when a session runs `--no-external` to skip usage-wedged App bots
    // (fixes a fail-open the sigma review caught). `reviewers` is empty for
    // every pre- config, so `reviewers_all_attested` is vacuously true
    // there and this changes nothing for them.
    let login_gate_active = !required_bots.is_empty() || optional_lane_configured;
    let login_skipped = no_external || !login_gate_active;
    // One scan feeds both the gate and its explanation, so the two cannot
    // disagree the way the decision and the message did on PR #618.
    let (unattested, malformed_attestations) = unattested_reviewers_scan_text(
        &events_text,
        reviewers,
        &freshness,
        &head_branch,
        head_sha,
        tiling.rounds_exhausted,
    );
    let reviewers_ok = unattested.is_empty();
    // Coverage reads the same merged journal text as the attestation scan
    // (its local axis) plus the GitHub review arrays (its github_app axis).
    // Authorship carry-forward: when this process resolved no manifest
    // session, the previous coverage row's recorded author FOR THIS PR (the
    // scan filters on the number; the events file is project-wide) stands
    // in, so a re-read from a manifest-less cwd classifies against the
    // historical author instead of landing every local verdict Unmeasured.
    let carried_author = author_session
        .map(str::to_string)
        .or_else(|| carry_author_session_forward(&events_text, number));
    let author_session = carried_author.as_deref();
    let (
        latest_review_ts,
        reviewed,
        missing_bots,
        bot_nudges,
        stale_bots,
        unaddressed_findings,
        coverage,
        hard_finding_present,
    ) = if login_skipped {
        // No GitHub logins to poll (nothing configured, or no_external): skip
        // the gh review reads entirely (fewer calls + no spurious gh-error
        // block). The local attestation gate still applies - reviewers_ok is
        // true when unconfigured, so a login-only or no-gate config is
        // unaffected. Coverage's github axis is empty here (no logins read),
        // so coverage is the local axis alone - which is exactly how a
        // worker-run /code-review counts even on a no-required-bots config.
        // The GitHub axis was intentionally not queried. That is a known
        // zero ONLY when nothing was configured to read: the skip is the
        // inactive gate, with or without no_external. A `no_external` session
        // on a repo with an ACTIVE login gate suppressed reads the config
        // demanded, so the honest answer for that axis is Unknown with its
        // retry remedy - reporting a healthy read of zero bots fabricated
        // "uncovered" and an instrument-health receipt for reviews that were
        // never queried. A fresh local pass still rescues it inside
        // classify_coverage (positive evidence).
        // One classification pass over a tiling, as (coverage, blockers): the
        // round-budget refresh below re-runs it so every conjunct downstream
        // reads the SAME budget, never a mix.
        let classify_with = |tiling: &RangeTiling| {
            let mut coverage = classify_coverage_tiled(
                &[],
                &[],
                &events_text,
                &[],
                !(no_external && login_gate_active),
                author_session,
                &freshness,
                &head_branch,
                head_sha,
                Some(tiling),
                pr_author.as_deref(),
                github_approval_satisfies,
            );
            demote_unmeasured_coverage(&mut coverage.coverage, &resolver);
            // Locked Decision 1: the pass condition is disposition-complete.
            // Non-terminal blocking findings withhold `reviewed` here exactly as
            // the Python merge gate refuses on them - below the cap only.
            let blockers = disposition_blockers(&events_text, &head_branch, head_sha);
            (coverage, blockers)
        };
        let (mut coverage, mut blockers) = classify_with(&tiling);
        // The reviewer scan above read the pre-refresh budget too; a local
        // mut so the arm's tuple below answers from whatever budget this arm
        // ends on.
        let mut reviewers_ok = reviewers_ok;
        //: the round budget counts the GitHub reviews axis on THIS arm
        // too. A stock install (no required bots, no optional lane) never
        // reached the external arm's refresh, so rounds the connector posted
        // read 0 here and the cap could not fire on exactly the lane that
        // spun (PR #1225: five real rounds, counter 0/2). The read mirrors
        // the Python merge gate's own gating (_coverage_gate._pr_reviews):
        // pay it only where it can change the answer - the row is uncovered,
        // or findings are in play - never on a healthy covered PR. A
        // no_external session reads nothing, whatever the gate says. A
        // failed read keeps the events-only answer rather than guessing: a
        // cap that fires on a broken read spends a budget that may be
        // unspent.
        if !no_external && (!coverage.coverage.is_covered() || !blockers.is_empty()) {
            let mut reviews_args = vec!["pr", "view"];
            reviews_args.extend(sel.iter().copied());
            reviews_args.extend(["--json", "reviews"]);
            match bounded_read(
                gh_bin.as_ref(),
                &reviews_args,
                cwd,
                "pr_reviews",
                stopgate_read_timeout(),
            ) {
                Ok(out) if out.status.success() => {
                    let parsed = serde_json::from_slice::<Value>(&out.stdout)
                        .map_err(|_| GhReadError::parse_failed("pr_reviews_parse"));
                    match parsed {
                        Ok(reviews_json) => {
                            let reviews_arr =
                                reviews_json.get("reviews").and_then(|v| v.as_array());
                            if let Some(reviews_arr) = reviews_arr {
                                tiling.rounds_used = rounds_since_last_pass(
                                    &events_text,
                                    &head_branch,
                                    head_sha,
                                    Some(reviews_arr),
                                );
                                tiling.rounds_exhausted = tiling.rounds_used >= max_rounds.max(1);
                                tiling.rounds_max = max_rounds;
                                // The classification and the reviewer scan
                                // above judged the pre-refresh budget; re-run
                                // both so the in-classify spent-budget
                                // discharge, the withhold conjunct, and the
                                // unattested reviewer's cap-yield all read the
                                // refreshed one.
                                let (re_coverage, re_blockers) = classify_with(&tiling);
                                coverage = re_coverage;
                                blockers = re_blockers;
                                let (re_unattested, _re_malformed) = unattested_reviewers_scan_text(
                                    &events_text,
                                    reviewers,
                                    &freshness,
                                    &head_branch,
                                    head_sha,
                                    tiling.rounds_exhausted,
                                );
                                reviewers_ok = re_unattested.is_empty();
                            }
                        }
                        Err(e) => log_bounded_read_error("round-budget reviews read", &e),
                    }
                }
                Ok(out) => log_bounded_read_error(
                    "round-budget reviews read",
                    &GhReadError::failed("pr_reviews", stderr_tail(&out.stderr_tail)),
                ),
                Err(e) => log_bounded_read_error("round-budget reviews read", &e),
            }
        }
        (
            "none".to_string(),
            reviewers_ok && !blockers_withhold(&blockers, tiling.rounds_exhausted),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            coverage,
            blockers.iter().any(|b| b.hard),
        )
    } else {
        // Read 3: top-level reviews + issue comments
        let mut reviews_args = vec!["pr", "view"];
        reviews_args.extend(sel.iter().copied());
        reviews_args.extend(["--json", "reviews,comments"]);
        let reviews_out = bounded_read(
            gh_bin.as_ref(),
            &reviews_args,
            cwd,
            "pr_reviews",
            stopgate_read_timeout(),
        )?;

        if !reviews_out.status.success() {
            return Err(GhReadError::failed(
                "pr_reviews",
                stderr_tail(&reviews_out.stderr_tail),
            ));
        }

        let reviews_json: Value = serde_json::from_slice(&reviews_out.stdout)
            .map_err(|_| GhReadError::parse_failed("pr_reviews_parse"))?;

        // PRESENCE is required-only: an optional login's absence must never
        // create a missing_bot (never wait for it), and its STALENESS must
        // never reach stale_bots either - an optional bot reading an older
        // commit is not a property any reviewer owes, so it may neither block
        // nor disqualify local-review recovery. FINDINGS honor the union: an
        // optional login's blocking P1 still holds the gate ("honor if
        // present"). A dedup keeps a login that is in both lists counted once.
        let info = compute_review_info(&reviews_json, required_bots, &freshness);
        // Per-outstanding-bot nudge classification, computed AFTER the
        // usage-limit retain (which happened inside compute_review_info) so
        // the two give-up paths never compose (AC6): a usage_limited bot is
        // already out of missing_bots and is never classified here. A STALE
        // bot is classified alongside a missing one: its remedy is
        // the same trigger comment, and without classification the session
        // could neither tell whether it had already asked nor idle while
        // waiting for the re-read. Derived from the same issue-comment list,
        // fresh every fire.
        let now = Utc::now();
        let review_comments = reviews_json
            .get("comments")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let classify = |bot: &String| {
            classify_bot_nudge(
                bot,
                review_comments,
                nudge_config_for(nudge_configs, bot),
                now,
            )
        };
        let bot_nudges: Vec<BotNudge> = info
            .missing_bots
            .iter()
            .chain(info.stale_bots.iter().map(|(bot, _)| bot))
            .map(classify)
            .collect();
        // The "empty bot_nudges = not classified = status quo" contract that
        // async_wait_class and build_block_reason rely on holds only because
        // this is an all-or-nothing map: bot_nudges is either empty or 1:1
        // with missing_bots + stale_bots. A future partial classification
        // would silently mis-idle, so pin the invariant here rather than let
        // it drift.
        debug_assert_eq!(
            bot_nudges.len(),
            info.missing_bots.len() + info.stale_bots.len()
        );
        let mut findings_bots: Vec<String> = required_bots.to_vec();
        for b in optional_bots {
            if !findings_bots.iter().any(|x| x == b) {
                findings_bots.push(b.clone());
            }
        }

        // Read 4: inline review comments (NEW in step 2). Codex's P1s land on
        // the /pulls/N/comments REST endpoint, which `gh pr view --json
        // comments` does NOT return (verified on PR #447). --paginate may
        // emit CONCATENATED JSON arrays (one per page), so parse as a stream.
        let pulls_target = format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments");
        let comments_out = bounded_read(
            gh_bin.as_ref(),
            &["api", &pulls_target, "--paginate"],
            cwd,
            "pulls_comments",
            stopgate_read_timeout(),
        )?;

        if !comments_out.status.success() {
            return Err(GhReadError::failed(
                "pulls_comments",
                stderr_tail(&comments_out.stderr_tail),
            ));
        }

        let mut inline_comments: Vec<Value> = Vec::new();
        for page in serde_json::Deserializer::from_slice(&comments_out.stdout).into_iter::<Value>()
        {
            let page = page.map_err(|_| GhReadError::parse_failed("pulls_comments_parse"))?;
            match page.as_array() {
                Some(arr) => inline_comments.extend(arr.iter().cloned()),
                None => return Err(GhReadError::parse_failed("pulls_comments_parse")),
            }
        }

        // Commit timestamps feed the commit-after arm of "addressed". Only
        // fetched when a blocking candidate could exist (cheap pre-scan).
        let has_blocking_candidate = inline_comments.iter().any(|c| {
            c.get("in_reply_to_id").and_then(|v| v.as_i64()).is_none()
                && blocking_severity(c.get("body").and_then(|v| v.as_str()).unwrap_or("")).is_some()
        });
        let commit_dates: Vec<String> = if has_blocking_candidate {
            let mut commits_args = vec!["pr", "view"];
            commits_args.extend(sel.iter().copied());
            commits_args.extend(["--json", "commits"]);
            let commits_out = bounded_read(
                gh_bin.as_ref(),
                &commits_args,
                cwd,
                "pr_commits",
                stopgate_read_timeout(),
            )?;
            if !commits_out.status.success() {
                return Err(GhReadError::failed(
                    "pr_commits",
                    stderr_tail(&commits_out.stderr_tail),
                ));
            }
            let commits_json: Value = serde_json::from_slice(&commits_out.stdout)
                .map_err(|_| GhReadError::parse_failed("pr_commits_parse"))?;
            commits_json
                .get("commits")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            c.get("committedDate")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let (inline_ts, unaddressed) = compute_unaddressed_findings(
            &inline_comments,
            &commit_dates,
            &findings_bots,
            external_reviewers,
        );

        // Read 4's newest comment timestamp joins the activity timestamp so
        // inline-only review traffic advances the fingerprint (closes the
        // false-NoProgress hole).
        let activity_ts = max_ts(&info.latest_ts, &inline_ts);
        //: the login gate AND the local-attestation reviewers gate must
        // both clear. reviewers is usually empty (vacuously true) so this is a
        // no-op for login-only configs.
        // (a) Record the rate-limit drop so a post-hoc audit sees why the gate
        // proceeded without a required bot (AC1-UI). append_loop_event, not
        // Branch-B emit: these are target-stream events (see the doc comment on
        // append_loop_event), deliberately unregistered in KNOWN_EVENT_KINDS.
        let usage_limited: Vec<String> = info
            .reviewer_refused
            .iter()
            .filter(|bot| {
                review_comments
                    .iter()
                    .any(|comment| usage_limit_comment_by(bot, comment))
            })
            .cloned()
            .collect();
        if !usage_limited.is_empty() {
            append_loop_event(
                events_path,
                "review_gate_bot_usage_limited",
                serde_json::json!({"pr": number, "bots": usage_limited}),
            );
        }
        // Coverage's github_app axis: configured required + optional logins
        // (external_reviewers are local-attestation peers, not github
        // posters). Dedup so a login in both lists is one verdict.
        let reviews_arr: &[Value] = reviews_json
            .get("reviews")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        // The reviews are in hand, so the round budget now counts BOTH
        // evidence axes: the attestations above and the review objects
        // here. A GitHub-App reviewer's rounds leave no attestation row
        // anywhere, so without this refresh a connector-driven loop reads
        // zero rounds and the cap cannot fire. The refreshed values feed
        // every consumer below in this arm (the coverage classify, the
        // withhold/impossible conjuncts, the emitted row). The no-external
        // arm above pays for the same axis only where it can change the
        // answer (uncovered row, findings in play) - never on a healthy
        // covered PR.
        tiling.rounds_used =
            rounds_since_last_pass(&events_text, &head_branch, head_sha, Some(reviews_arr));
        tiling.rounds_exhausted = tiling.rounds_used >= max_rounds.max(1);
        tiling.rounds_max = max_rounds;
        let comments_arr: &[Value] = reviews_json
            .get("comments")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut gh_logins: Vec<String> = required_bots.to_vec();
        for b in optional_bots {
            if !gh_logins.iter().any(|x| x == b) {
                gh_logins.push(b.clone());
            }
        }
        // github_read_ok is true here: a failed gh read returned Err above.
        let mut coverage = classify_coverage_tiled(
            reviews_arr,
            comments_arr,
            &events_text,
            &gh_logins,
            true,
            author_session,
            &freshness,
            &head_branch,
            head_sha,
            Some(&tiling),
            pr_author.as_deref(),
            github_approval_satisfies,
        );
        demote_unmeasured_coverage(&mut coverage.coverage, &resolver);
        mark_owed_verdicts(&mut coverage, required_bots);
        let local_recovery = local_recovery_from_refusal(
            &info.reviewer_refused,
            &info.missing_bots,
            &info.stale_bots,
            &coverage,
        );
        // Locked Decision 1, same conjunct as the solo lane arm: the pass
        // condition is disposition-complete, withheld below the cap only.
        let blockers = disposition_blockers(&events_text, &head_branch, head_sha);
        let reviewed = (info.all_required_passed() || local_recovery || tiling.rounds_exhausted)
            && unaddressed.is_empty()
            && reviewers_ok
            && !blockers_withhold(&blockers, tiling.rounds_exhausted);
        (
            activity_ts,
            reviewed,
            info.missing_bots,
            bot_nudges,
            info.stale_bots,
            unaddressed,
            coverage,
            blockers.iter().any(|b| b.hard),
        )
    };
    // The hard axis rides the row: the standing operator-law waiver's
    // condition is hard findings alone, independent of the budget, and it
    // only ever consults this below the cap - at the cap the configured
    // rounds discharge every open finding.
    let mut tiling = tiling;
    tiling.hard_blocker = hard_finding_present;

    // Emit coverage every gate eval so the Python readers (the merge primitive
    // and the polling command) and audit see one coherent, fresh number rather
    // than recomputing it (the Ownership rule: loopcheck computes, Python
    // reads). Skipped for the no-PR early returns above.
    //
    // BOTH logs, like every other loop-check event. This one used to write only
    // the project log, and since the stop hook runs wherever the session runs,
    // that put the attestation in `<worktree>/.fno/events.jsonl` while a merge
    // run from canonical read `<canonical>/.fno/events.jsonl` - a satisfied
    // gate reading as an unsatisfiable one, silently, with a refusal that
    // named a count and not a location. The global log is the one file both
    // stand in; `repo` in the payload keeps it scoped.
    // The posture verdict rides the same emit: coverage computed the verdicts,
    // so satisfaction against the resolved rung is one predicate here rather
    // than a reclassification on the Python side (AC6-HP).
    let posture_v = posture.map(|pc| posture_verdict(pc, &coverage, peer_reviewers));
    if number > 0 {
        emit_to_both(
            events_path,
            global_events_path,
            "review_coverage",
            coverage_event_data_full(
                number,
                &coverage,
                head_sha,
                repo_slug,
                author_session,
                Some(&tiling),
                posture_v.as_ref(),
            ),
        );
    }

    // The merge-slot hold: a local claims read keyed to this PR's
    // base ref, so idling on a slot held by another PR costs no GitHub spend.
    // A slot this PR holds itself is not a hold on THIS session. The read is
    // gated on the classifier's cheap preconditions (green CI, review gate
    // satisfied): every consumer of this field sits behind both, so a red or
    // unreviewed PR pays no `git worktree list` subprocess for a value it
    // never reads.
    let merge_slot_holder = if ci_conclusion.is_ok() && reviewed {
        crate::authorized_merge::merge_slot_holder(cwd, base_ref)
            .filter(|m| *m != number.unsigned_abs())
    } else {
        None
    };
    Ok(PrInfo {
        range_tiling: tiling,
        state,
        number,
        head_oid,
        ci_conclusion,
        failing_checks,
        ci_has_pending,
        mergeable,
        merge_slot_holder,
        base_behind,
        latest_review_ts,
        reviewed,
        missing_bots,
        bot_nudges,
        stale_bots,
        unaddressed_findings,
        // Telemetry only (no decision reads this): "no review gate of any kind
        // applied" = the login reads were skipped AND no local reviewers gate.
        // A reviewers-only config did gate, so it is NOT review_skipped.
        review_skipped: login_skipped && reviewers.is_empty(),
        unattested_reviewers: unattested,
        malformed_attestations,
        posture: posture_v,
        coverage,
    })
}

/// The fno binary every loop-check surface shells, resolved through the same
/// env seam the hint and fidelity probes use (`FNO_LOOPCHECK_FNO_BIN`,
/// default `fno`). One resolver so a stubbed test and a live gate cannot
/// disagree about which binary answered.
pub(crate) fn loopcheck_fno_bin() -> String {
    std::env::var("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|_| "fno".to_string())
}

/// `$HOME/.fno/events.jsonl`, the global-log fallback every direct-dispatch
/// verb reaches for when no `--global-events` override is given. Hand-built
/// (no Rust resolver for events.jsonl exists yet, debt this file
/// already carries) rather than a new duplicate of the same literal in
/// every caller.
pub(crate) fn default_global_events_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(&home).join(".fno/events.jsonl")
}

/// Best-effort `fno inbox notify TITLE BODY`. Spawned detached and never waited on;
/// any failure (missing binary, non-zero exit) is non-fatal - the terminal
/// completes on the durable event row alone (AC2-FR). Suppressed under
/// `FNO_LOOPCHECK_NO_NOTIFY=1` so unit tests never spawn a real notifier.
fn best_effort_notify(title: &str, body: &str) {
    if std::env::var("FNO_LOOPCHECK_NO_NOTIFY").as_deref() == Ok("1") {
        return;
    }
    // var_os avoids a lossy UTF-8 conversion on a path/binary env value and
    // hands the raw OsString straight to the spawn (gemini review).
    let fno_bin = std::env::var_os("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|| "fno".into());
    crate::operator_notice::notify_operator_with(&fno_bin, title, body, None);
}

// ── review coverage ──────────────────────────────────────────────────
//
// The old gate's `reviewed` boolean (loopcheck.rs `let reviewed =
// all_required_passed() && unaddressed.is_empty() && reviewers_ok`) was a claim
// about reviews computed entirely from what did NOT happen: nobody is still
// owed, no finding is outstanding, no reviewer is unattested. A quota refusal is
// dropped from `missing_bots` (PR #214) and reads as a pass; on a config with no
// required bots, nothing can object, so `reviewed` is true on zero reviews.
//
// Coverage is the missing predicate: did anyone actually review? It is a
// first-class value reported everywhere, never folded back into the objection
// boolean (collapsing it back undoes this node).
//
// Producer axis, not producer string. Two review producers share the display
// name "codex": the `chatgpt-codex-connector` GitHub App (posts review objects,
// can refuse on quota) and the local `codex` CLI (posts none, never rate-limited
// by the App's quota). They are told apart by `CoverageProducer`, never by the
// reviewer string. A third local lane,
// claude `/code-review`, shares the `LocalAttestation` axis.

// ── inline findings (Read 4, step 2 / US2) ────────────────────────────────────

// ── fingerprint + fire history ────────────────────────────────────────────────

fn make_fingerprint(
    head_sha: &str,
    pr_state: &str,
    ci_conclusion: &str,
    latest_ts: &str,
) -> String {
    // An absent latest-review time renders "none", the pre-read's form; two shapes reset the streak.
    let latest_ts = if latest_ts.is_empty() {
        "none"
    } else {
        latest_ts
    };
    format!("{head_sha}|{pr_state}|{ci_conclusion}|{latest_ts}")
}

/// Default debounce window: an unchanged fingerprint seen again inside this many
/// seconds is the SAME observation, not a new one. The streak counts independent
/// observations of an unchanged world, not stop-hook fires -- a session taking
/// short turns used to burn a 5-fire backstop in 109 seconds while its CI run
/// still had 7 minutes to go, which no external wait can outrun. The effective
/// floor becomes `(backstop_n - 1) * gap`: 10 minutes unattended, 20 attended.
/// Override with `FNO_LOOPCHECK_MIN_FIRE_GAP_SECS` (0 restores fire counting).
const MIN_FIRE_GAP_SECS: i64 = 300;

/// Resolve the debounce window from the env seam, falling back to the default.
/// Mirrors the `FNO_LOOPCHECK_GH_BIN` / `_NO_NOTIFY` / `_NO_COMMENT` seams.
fn min_fire_gap_secs() -> i64 {
    std::env::var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(MIN_FIRE_GAP_SECS)
}

/// Count prior loop_check events for this session_id in the project events file.
/// Returns (total_fires, consecutive_unchanged_count, last_fingerprint_in_log,
/// streak_window_secs).
///
/// `current_fp` is the fingerprint computed this fire (used for streak matching).
/// `last_fp` is the most recent fingerprint recorded in the events log for this
/// session -- used for carry-forward when the gh pre-read fails this fire.
/// `streak_window_secs` is the span from the oldest COUNTED fire to `now`; it is
/// what makes a streak count falsifiable from the events log.
///
/// The streak is debounced by `min_gap_secs`: walking backwards from `now`, a
/// matching fire closer than the gap to the last counted one is skipped
/// TRANSPARENTLY and does not advance the cursor, so a burst collapses to a
/// single observation. The asymmetry is deliberate and load-bearing: a CHANGED
/// fingerprint breaks the streak at any spacing, because real progress is real
/// progress at any speed -- only the *absence* of change needs time to be
/// credible.
fn read_prior_fires(
    events_path: &Path,
    session_id: &str,
    current_fp: Option<&str>,
    now: DateTime<Utc>,
    min_gap_secs: i64,
) -> (u64, u64, Option<String>, i64) {
    let content = match event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => return (0, 0, None, 0),
    };

    let mut total: u64 = 0;

    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        total += 1;
    }

    // Calculate consecutive streak from the end (how many recent fires share current_fp)
    // and capture the most recent fp recorded. `next_ts` is the cursor: it starts
    // at `now` and only moves to a fire that was COUNTED, which is what collapses
    // a rapid burst into one observation.
    let mut consecutive: u64 = 0;
    let mut last_fp: Option<String> = None;
    let mut next_ts = now;
    let mut oldest_counted_ts: Option<DateTime<Utc>> = None;
    for line in content.lines().rev() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        // US4: gh-errored fires are TRANSPARENT to the streak - they neither
        // advance nor reset the consecutive count (their recorded fp is just
        // a carry-forward, not an observation). After an outage clears, the
        // streak resumes from its pre-outage value (AC4-FR).
        if val
            .pointer("/data/fp_read_failed")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            continue;
        }
        let fp = val
            .pointer("/data/fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Capture the most recent fp (first match in reverse order)
        if last_fp.is_none() && !fp.is_empty() {
            last_fp = Some(fp.to_string());
        }
        // With no explicit reference, the streak counts against the NEWEST
        // recorded fingerprint: the journal IS the observation history when
        // this fire reads no PR state .
        let reference = match current_fp {
            Some(fp) => fp,
            None => last_fp.as_deref().unwrap_or(""),
        };
        // A CHANGED fingerprint breaks the streak at ANY spacing - progress is
        // never debounced. This check precedes the gap check on purpose.
        if fp != reference {
            break;
        }
        // Debounce. A fire we cannot place in time is skipped transparently
        // rather than counted: giving up on a parse error must fail AWAY from
        // an irreversible NoProgress, matching classify_bot_nudge's precedent.
        let Some(ts) = val
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        else {
            continue;
        };
        let gap = (next_ts - ts).num_seconds();
        // gap < 0 means clock skew (a fire stamped after `now`); count it rather
        // than invent a debounce from a bad clock - status quo, no crash.
        if gap < 0 || gap >= min_gap_secs {
            consecutive += 1;
            next_ts = ts;
            oldest_counted_ts = Some(ts);
        }
        // else: same observation seen twice; skip WITHOUT advancing next_ts.
    }

    let streak_window_secs = oldest_counted_ts
        .map(|t| (now - t).num_seconds().max(0))
        .unwrap_or(0);

    (total, consecutive, last_fp, streak_window_secs)
}

/// The newest recorded loop_check row's `pr_state`/`ci` components for this
/// session: the journal's copy of the last observed world, so a fire that
/// reads no PR state can still record comparable row fields .
fn read_last_row_fields(events_path: &Path, session_id: &str) -> (String, String) {
    let content = match event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => return ("none".to_string(), "none".to_string()),
    };
    for line in content.lines().rev() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        if val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        return (
            val.pointer("/data/pr_state")
                .and_then(|v| v.as_str())
                .unwrap_or("none")
                .to_string(),
            val.pointer("/data/ci")
                .and_then(|v| v.as_str())
                .unwrap_or("none")
                .to_string(),
        );
    }
    ("none".to_string(), "none".to_string())
}

// ── event emission ────────────────────────────────────────────────────────────

/// Envelope struct for target-stream events. Field order ts,type,source,data is
/// preserved because serde_json serializes struct fields in declaration order.
/// Method is named `append_loop_event` (NOT .emit / .emit_fields) so the
/// production-emit scanner test in lib.rs does not capture it and force
/// registration in KNOWN_EVENT_KINDS (which is the Branch B / fno-agents
/// daemon stream, not the target stream that these events belong to).
#[derive(Debug, Serialize)]
struct LoopEventEnvelope<'a> {
    ts: String,
    #[serde(rename = "type")]
    event_type: &'a str,
    source: &'static str,
    data: serde_json::Value,
}

// pub(crate): the `finalize` verb (step 6, ) reuses this so its
// `session_finalized` events carry the identical RFC3339 timestamp shape.
pub(crate) fn now_rfc3339_utc() -> String {
    // Millisecond precision prevents distinct same-second events from sharing
    // the content-derived id that makes a true byte-identical retry idempotent.
    let now = chrono::Utc::now();
    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Append a target-stream event through the shared Branch-A mkdir mutex.
/// Failure is loud on stderr but never fatal to the decision.
fn append_loop_event(path: &Path, event_type: &str, data: serde_json::Value) {
    let env = LoopEventEnvelope {
        ts: now_rfc3339_utc(),
        event_type,
        source: "hook",
        data,
    };
    let Ok(event) = serde_json::to_value(&env) else {
        eprintln!("loop-check: failed to serialize event {event_type}");
        return;
    };
    // One store commit is the acknowledgement: the journal lock-timeout and
    // maintenance retry legs retired with the mutex they served.
    if let Err(error) =
        crate::claims::append_event_line(path, &event, std::time::Duration::from_secs(2))
    {
        eprintln!(
            "loop-check: failed to write event {event_type} to {}: {error}",
            path.display()
        );
    }
}

/// Append to both project and global event logs; `finalize` ships its
/// session events through the same writer, so all envelopes stay identical.
pub(crate) fn emit_to_both(
    project_events: &Path,
    global_events: &Path,
    event_type: &str,
    data: serde_json::Value,
) {
    append_loop_event(project_events, event_type, data.clone());
    if project_events != global_events {
        append_loop_event(global_events, event_type, data);
    }
}

pub(crate) fn observe_shadow_transition(
    run_log: &Path,
    session_id: &str,
    event: crate::run_state::RunEvent,
    project_events: &Path,
    global_events: &Path,
) -> bool {
    if !is_full_run_id(session_id) {
        emit_transition_rejection(
            session_id,
            event,
            "invalid_run_id",
            "manifest carries no valid full run id".to_string(),
            None,
            run_log,
            project_events,
            global_events,
        );
        return false;
    }

    let Err(error) = crate::run_state::append_transition(run_log, session_id, event) else {
        return true;
    };
    let (kind, from) = match &error {
        crate::run_state::RunStateError::InvalidTransition(invalid) => (
            "invalid_transition",
            Some(serde_json::to_value(invalid.from).unwrap_or(serde_json::Value::Null)),
        ),
        _ => ("observer_io", None),
    };
    emit_transition_rejection(
        session_id,
        event,
        kind,
        error.to_string(),
        from,
        run_log,
        project_events,
        global_events,
    );
    false
}

fn emit_transition_rejection(
    session_id: &str,
    event: crate::run_state::RunEvent,
    kind: &str,
    error: String,
    from: Option<serde_json::Value>,
    run_log: &Path,
    project_events: &Path,
    global_events: &Path,
) {
    let event_name = serde_json::to_value(event)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let mut data = serde_json::json!({
        "session_id": session_id,
        "kind": kind,
        "event": event_name,
        "error": error,
        "run_log": run_log.display().to_string(),
    });
    if let Some(from) = from {
        data["from"] = from;
    }
    emit_to_both(project_events, global_events, "transition_rejected", data);
}

pub(crate) fn is_full_run_id(value: &str) -> bool {
    static SESSION_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    SESSION_ID
        .get_or_init(|| {
            regex::Regex::new(
                r"^(?:\d{8}T\d{6}Z-[a-z]{0,2}\d+-[0-9a-f]{6}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$",
            )
            .expect("full run id regex is valid")
        })
        .is_match(value)
}

// ── budget check ──────────────────────────────────────────────────────────────

// ── main decision function ────────────────────────────────────────────────────

/// Core decision logic. Returns (exit_code, json_output).
/// Exit 0 always for allow/block; non-zero only for internal/CLI errors.
fn decide_inner(args: &[String]) -> (i32, String) {
    // Missing required flags are CLI misuse: exit 2 with the same JSON error
    // shape the pre-refactor inline checks emitted (AC5-ERR).
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            let out = serde_json::json!({ "error": e });
            return (2, out.to_string());
        }
    };
    // (A): the shim feeds the full Stop-hook JSON via stdin so
    // the stopping turn's final text (`last_assistant_message`, recomputed
    // per fire) is readable without racing the transcript flush. Read or
    // parse failures degrade to None (transcript fallback), never an error -
    // but a genuine I/O error is named on stderr so a sustained stdin failure
    // is separable from an ordinary transcript-channel fire.
    let hook_input: Option<String> = if parsed.hook_input_stdin {
        match std::io::read_to_string(std::io::stdin()) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "loop-check: failed to read hook input from stdin: {e}; falling back to transcript scan"
                );
                None
            }
        }
    } else {
        None
    };
    decide_with_payload(&parsed, hook_input.as_deref())
}

/// The decision core with the Stop payload as a parameter :
/// the native `hook stop` handler calls this in process with the payload it
/// already read, and tests replay recorded payloads, so the stdin handoff is
/// no longer the only way in.
pub(crate) fn decide_with_payload(
    parsed: &LoopCheckArgs,
    hook_input: Option<&str>,
) -> (i32, String) {
    // Publish the fire bound and the king drain reserve before any read.
    let reserve_ms = if parsed.driver == "king" {
        stopgate_drain_reserve_ms()
    } else {
        0
    };
    stopgate_stamp_fire(
        parsed.read_timeout_ms.unwrap_or(0),
        std::time::Instant::now() + STOPGATE_FIRE_BUDGET,
        reserve_ms,
    );
    if let Some(message) = crate::loops_pause::pause_message(&parsed.cwd) {
        return (0, paused_output(&parsed.driver, &message));
    }
    // The king uses a separate manifest and decision path.
    if parsed.driver == "king" {
        return king_decide::king_decide(&parsed);
    }

    // Session binding: when the caller names the harness session that asked,
    // the registry answers who may drive this target before any progress
    // logic runs. Body, refusal and crown routing: loopcheck/session_binding.rs.
    if let Some(out) = session_binding::gate_output(&parsed) {
        return out;
    }

    let state_path = parsed.state_path.clone();
    let transcript_path = parsed.transcript_path.clone();
    let cwd = parsed.cwd.clone();

    // (A): the payload text arrives as a parameter (see
    // decide_with_payload); the message read degrades to the transcript scan.
    let last_assistant_message: Option<String> =
        hook_input.and_then(extract_last_assistant_message);

    // Parse manifest
    let manifest_content = match std::fs::read_to_string(&state_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "loop-check: cannot read state file {}: {e}",
                state_path.display()
            );
            return (
                0,
                allow_output(
                    "allow",
                    None,
                    "corrupt/missing manifest; allowing exit",
                    0,
                    None,
                ),
            );
        }
    };

    let manifest = match parse_manifest(&manifest_content) {
        Some(m) => m,
        None => {
            eprintln!("loop-check: corrupt manifest (no frontmatter)");
            let out = allow_output(
                "allow",
                None,
                "corrupt manifest (no frontmatter); allowing exit",
                0,
                None,
            );
            return (0, out);
        }
    };

    // Lease renewal: keep this session's node claim fresh on every
    // stop, so a worker whose supervisor pid died mid-run (and now runs under a
    // new pid) never loses its claim to TTL expiry. Best-effort and non-fatal:
    // renew only bumps expires_at when the on-disk holder still matches, so it
    // can never steal, and any failure is a warning that just shortens the lease
    // (the loop never blocks on it). The claim key/holder/ttl are APPENDED after
    // the frontmatter by `fno do target init`, so scan the whole manifest for them
    // (parse_manifest is frontmatter-bounded and would miss them). Root=None
    // routes node:<id> to the global claims root inside renew.
    if let (Some(key), Some(holder)) = (
        scan_manifest_field(&manifest_content, "target_claim_key"),
        scan_manifest_field(&manifest_content, "target_claim_holder"),
    ) {
        // Renew for the SAME window the claim was acquired with (default 2h,
        // matching init's `_CLAIM_TTL`), so the deadline never grows.
        let ttl_ms = scan_manifest_field(&manifest_content, "target_claim_ttl")
            .and_then(|s| crate::claims::parse_ttl_ms(&s))
            .unwrap_or(7_200_000);
        match crate::claims::renew(&key, &holder, ttl_ms, None) {
            Ok(_) => {}
            Err(e) => eprintln!("loop-check: lease renewal for {key} failed (non-fatal): {e}"),
        }
    }

    // Resolve paths + settings + reviewer sets through the ONE shared resolver
    //: the standalone review-coverage verb resolves exactly these,
    // from the same overlay, so there is no second precedence implementation.
    let inputs = resolve_review_inputs(
        &cwd,
        parsed.events_path.as_deref(),
        parsed.global_events_path.as_deref(),
        parsed.settings_path.as_deref(),
        parsed.global_settings_path.as_deref(),
        parsed.author_harness_override.as_deref(),
    );
    let project_events = inputs.project_events;
    let global_events = inputs.global_events;
    let repo_slug = inputs.repo_slug;
    let settings = inputs.settings;
    let author_harness = inputs.author_harness;
    let required_bots = inputs.required_bots;
    let mut required_reviewers = inputs.required_reviewers;
    let optional_bots = inputs.optional_bots;
    let nudge_configs = inputs.nudge_configs;

    let ledger_path = parsed
        .ledger_path
        .clone()
        .unwrap_or_else(|| crate::paths::ledger_path(&cwd));

    // Now timestamp
    let now: DateTime<Utc> = if let Some(ref s) = parsed.now_override {
        s.parse().unwrap_or_else(|_| Utc::now())
    } else {
        Utc::now()
    };

    let session_id = manifest
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let emit = |event_type: &str, data: serde_json::Value| {
        emit_to_both(&project_events, &global_events, event_type, data);
    };

    // <help> distress: parsed from the same stopping message the
    // intent read uses, ahead of every branch below (including the advisory
    // no-gh mode), because it is a side channel that must fire once per stop
    // regardless of how the stop itself is decided. The node id is resolved
    // here once; the review-findings scan below reuses the same binding.
    let node_id = scan_manifest_field(&manifest_content, "graph_node_id").or_else(|| {
        scan_manifest_field(&manifest_content, "target_claim_key")
            .and_then(|k| k.strip_prefix("node:").map(|s| s.to_string()))
    });
    let harness = scan_manifest_field(&manifest_content, "harness");
    crate::distress::scan_and_emit(
        &project_events,
        &global_events,
        &cwd,
        &session_id,
        node_id.as_deref(),
        harness.as_deref(),
        &transcript_path,
        last_assistant_message.as_deref(),
    );

    // ── Step 1: cancel sentinel ───────────────────────────────────────────────
    if let Some(hit) = check_cancel_sentinel(&cwd, &state_path, &manifest.created_at, "target") {
        emit("termination", hit.termination_data(&session_id));
        // One-shot: once a sentinel has terminated this run it has done its
        // job. Consuming it is what stops a cancel from re-terminating every
        // later stop of a session that recovers and keeps working.
        if hit.kind == crate::cancel_sentinel::CancelKind::TargetSentinel {
            let _ = std::fs::remove_file(&hit.path);
        }
        return (
            0,
            allow_output(
                "allow",
                Some(TerminationReason::Interrupted),
                &hit.termination_message(),
                0,
                None,
            ),
        );
    }

    // ── Step 2: legacy terminal status ───────────────────────────────────────
    if let Some(ref status) = manifest.legacy_status {
        emit(
            "loop_check_legacy_manifest",
            serde_json::json!({
                "session_id": session_id,
                "status": status
            }),
        );
        return (
            0,
            allow_output(
                "allow",
                None,
                &format!("legacy manifest status={status}; allowing exit"),
                0,
                None,
            ),
        );
    }

    // ── Step 3: budget check ──────────────────────────────────────────────────
    if let Some(trip) = check_budget(&manifest, &settings, &now, &ledger_path) {
        let axis = match &trip {
            BudgetTrip::WallClock => "wall_clock",
            BudgetTrip::Cost => "cost",
        };
        emit(
            "termination",
            serde_json::json!({
                "session_id": session_id,
                "reason": "Budget",
                "axis": axis,
                "message": format!("budget exceeded (axis={axis})")
            }),
        );
        return (
            0,
            allow_output(
                "allow",
                Some(TerminationReason::Budget),
                &format!("budget exceeded (axis={axis})"),
                0,
                None,
            ),
        );
    }

    let generic = crate::delivery_completion::evaluate_manifest(
        &cwd,
        manifest.plan_path.as_deref(),
        &project_events,
    );
    // ── Steps 3b/3c: the question gates (decided-but-unrecorded; held-on-open)
    // Both folds live in `holds` beside the scans they drive; the journal
    // union, the emit rows, and the order (unrecorded first, then held) are
    // theirs.
    if session_id != "unknown" {
        match holds::question_gates(
            &project_events,
            &global_events,
            &cwd,
            &session_id,
            node_id.as_deref().unwrap_or(""),
            &emit,
        ) {
            holds::QuestionGateStop::None => {}
            holds::QuestionGateStop::Block { reason } => {
                return (0, allow_output("block", None, &reason, 0, None));
            }
            holds::QuestionGateStop::Terminate { reason, message } => {
                return (0, allow_output("allow", Some(reason), &message, 0, None));
            }
        }
    }
    // ── Check gh binary availability ──────────────────────────────────────────
    // Only a NotFound spawn reads as absence. Every other spawn failure is
    // SpawnTrouble: gh exists but could not be spawned right now, which is
    // not a fact about the world and must not degrade the session. The probe
    // outcome is emitted as an event row so what it concluded is observable.
    let gh_bin = &parsed.gh_bin;
    let gh_probe = probe_gh_bin(gh_bin.as_ref(), &cwd);
    // Type is deliberately NOT "loop_check": read_prior_fires treats every
    // loop_check row for this session as a fire observation, and a probe row
    // with no fingerprint would break the no-progress streak on each fire.
    // The probe is its own observable, not a fire decision.
    emit(
        "gh_probe",
        serde_json::json!({
            "session_id": session_id,
            "outcome": gh_probe.outcome_str(),
            "detail": gh_probe.detail_str(),
        }),
    );
    let gh_available = !matches!(gh_probe, GhProbeOutcome::Absent);

    if !gh_available
        && matches!(
            generic,
            crate::delivery_completion::DeliveryCompletion::Inactive
        )
    {
        if !manifest.attended && !manifest.advisory {
            // Unattended + no advisory + no gh -> Interrupted
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "Interrupted",
                    "message": "gh binary not found; unattended sessions require gh"
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::Interrupted),
                    "gh binary not found; unattended sessions require gh",
                    0,
                    None,
                ),
            );
        }
        // Attended or declared advisory -> advisory mode (promise + budget only).
        // Budget was already checked above; honor intent here so a promise can
        // terminate an advisory session (AC5-ERR) - gh reads are impossible, so
        // the promise alone is the completion signal.
        emit(
            "loop_advisory_mode",
            serde_json::json!({
                "session_id": session_id,
                "attended": manifest.attended
            }),
        );
        let (advisory_intent, _advisory_intent_source) =
            detect_intent(last_assistant_message.as_deref(), &transcript_path);
        if let Intent::Aborted { ref reason } = advisory_intent {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "Aborted",
                    "message": reason
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::Aborted),
                    "aborted tag detected (advisory mode)",
                    0,
                    None,
                ),
            );
        }
        if advisory_intent == Intent::Promise {
            emit(
                "termination",
                serde_json::json!({
                    "session_id": session_id,
                    "reason": "DoneAdvisory",
                    "message": "promise accepted in advisory mode (gh unavailable)"
                }),
            );
            return (
                0,
                allow_output(
                    "allow",
                    Some(TerminationReason::DoneAdvisory),
                    "promise accepted in advisory mode (gh unavailable)",
                    0,
                    None,
                ),
            );
        }
        return (
            0,
            allow_output(
                "block",
                None,
                "gh binary not found; running in advisory mode (promise + budget only)",
                0,
                None,
            ),
        );
    }

    // ── Step 4: intent + backstop ─────────────────────────────────────────────
    let (intent, intent_source) =
        detect_intent(last_assistant_message.as_deref(), &transcript_path);
    let git_bin = &parsed.git_bin;
    let head_sha = git_head_sha(git_bin, &cwd);
    let gh_bin = &parsed.gh_bin;
    // The fleet's ONE GitHub request budget ledger: the stand-down below and
    // the failed-read refusal recording both key off it.
    let budget_ledger = parsed
        .gh_budget_ledger
        .clone()
        .unwrap_or_else(crate::gh_budget::ledger_path);

    // Fire history is JOURNAL truth: fires, the trailing shared fingerprint,
    // and its pr_state/ci come from one local read; no PR read routes. A
    // generic-delivery fire OBSERVED its world, so its streak counts against
    // the observed revision; every other fire reads the journal's newest fp.
    let backstop_n: u64 = if manifest.attended { 5 } else { 3 };
    let min_fire_gap = min_fire_gap_secs();
    let generic_observed = generic.is_active();
    let no_pr_fp =
        || generic.delivery_fingerprint(make_fingerprint(&head_sha, "none", "none", "none"));
    let observed_fp = no_pr_fp();
    let (prior_fires, journal_streak, last_recorded_fp, streak_window) = read_prior_fires(
        &project_events,
        &session_id,
        if generic_observed {
            Some(&observed_fp)
        } else {
            None
        },
        now,
        min_fire_gap,
    );
    let (last_pr_state, last_ci) = read_last_row_fields(&project_events, &session_id);
    // A fire that does not run done() inherits the last recorded fingerprint,
    // so its row stays comparable with its neighbors; only done() can move it.
    let fingerprint = if generic_observed {
        observed_fp
    } else {
        last_recorded_fp.clone().unwrap_or_else(no_pr_fp)
    };
    let this_fire = prior_fires + 1;
    // consecutive_unchanged counts prior identical fires; adding this fire.
    let consecutive_after = journal_streak + 1;
    let backstop_tripped = consecutive_after >= backstop_n;
    let terminal = |dec: &str, r, m: &str| {
        (
            0,
            allow_output(dec, r, m, this_fire, Some(fingerprint.clone())),
        )
    };

    // The harness caps consecutive Stop-hook blocks at
    // CLAUDE_CODE_STOP_HOOK_BLOCK_CAP (Claude Code default 9) and force-ends the
    // turn once it binds. Record the resolved cap on the first fire of
    // a session so a run ended by the harness override (last events are blocks
    // whose running consecutive count meets the cap, then silence) is
    // distinguishable from one ended by budget (a terminal budget decision).
    let (block_cap, block_cap_source) = match std::env::var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP") {
        Ok(v) => (v.trim().parse::<u64>().unwrap_or(9), "env"),
        Err(_) => (9, "default"),
    };
    if prior_fires == 0 {
        emit(
            "loop_check_config",
            serde_json::json!({
                "session_id": session_id,
                "block_cap": block_cap,
                "block_cap_source": block_cap_source,
            }),
        );
    }

    // D: probe done() after MUTE_PROBE_N unchanged mute fires
    // instead of waiting out the full backstop streak. A done-but-mute
    // session (all reads pass, no promise as final text) now resolves as a
    // late DonePRGreen in ~2 fires instead of 5/3 - the post-wedge events
    // audit counted 337 backstop fires, i.e. ~1000 no-op confirmation laps.
    // NoProgress still requires the full backstop_n streak (unchanged below),
    // so the grilled-9 backstop semantics are intact; a probed fire whose
    // done() fails simply blocks with the named reason.
    const MUTE_PROBE_N: u64 = 2;

    // ── Watching: the lease-only idle runs ahead of every read. A
    // <watching> tag on a harness that can self-wake idles on the tag plus a
    // renewed claim lease: this fire reads NO PR state - the watcher's exit
    // re-evaluates with fresh evidence. A harness that cannot idle, or a
    // lease that will not renew, falls through with the named refusal riding
    // the ordinary done() block, so the agent still sees the actionable
    // blocker behind its own dead watch - never a dead watch, never a blind
    // one.
    let mut watching_fell_through = false;
    if let Intent::Watching {
        ref reason,
        ref timeout,
        ref pr,
    } = intent
    {
        let is_loop_run_child = std::env::var("FNO_DRIVER_LIB").is_ok();
        let can_idle = harness_can_idle(author_harness.as_deref(), is_loop_run_child);
        let window_ms = watch_window_ms(timeout.as_deref());
        let claim = watch_lease::claim_pair(&manifest_content);
        let renew_outcome = claim
            .as_ref()
            .map(|(key, holder)| crate::claims::renew(key, holder, window_ms, None));
        let renewed = matches!(renew_outcome.as_ref(), Some(Ok(true)));
        if can_idle && renewed {
            // The tag's own `pr=`/`reason=` attributes are the only source
            // here (the idle verifies nothing), so `blocker` is only as
            // trustworthy as the agent's tag: pass the declared reason
            // through when it is one of the two real classes, else the
            // honest "unknown".
            let blocker = match reason.as_str() {
                "ci" => "ci",
                "review" => "review",
                "merge_slot" => "merge_slot",
                _ => "unknown",
            };
            emit(
                "loop_check_watch_idle",
                serde_json::json!({
                    "session_id": session_id,
                    "pr": pr.as_deref().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                    "blocker": blocker,
                    "declared_timeout": timeout.clone().unwrap_or_default(),
                    "reason": reason,
                    "lease_ms": window_ms
                }),
            );
            emit(
                "loop_check",
                serde_json::json!({
                    "session_id": session_id,
                    "fingerprint": fingerprint,
                    "fires": this_fire,
                    "consecutive_unchanged": consecutive_after,
                    "streak_window_secs": streak_window,
                    "decision": "allow",
                    "intent": "watching",
                    "intent_source": intent_source,
                    "pr_state": last_pr_state,
                    "ci": last_ci,
                    "reviewed": false,
                    "fp_read_failed": false
                }),
            );
            return terminal(
                "allow",
                None,
                "watching: idling until the watcher fires; this fire read no PR state",
            );
        }
        // Not idlable, or the lease declined: the refusal is composed after
        // done() has named the real blocker, so the block keeps the
        // actionable reason the agent needs alongside the refusal itself.
        watching_fell_through = true;
    }

    // node_id is resolved once above, beside the <help> distress emit.
    let (open_findings, malformed_findings) = match node_id.as_deref() {
        Some(n) => open_review_findings(&project_events, n),
        None => (Vec::new(), 0),
    };
    if malformed_findings > 0 {
        emit(
            "loop_check_malformed_finding",
            serde_json::json!({
                "session_id": session_id,
                "node": node_id,
                "malformed_lines": malformed_findings
            }),
        );
    }

    // The two telemetry rows every path records, bound once: the fire's
    // loop_check row carries the same nine base fields everywhere, merged
    // with the fields the deciding arm adds.
    let term_row = |reason: &str, message: &str| {
        emit(
            "termination",
            serde_json::json!({"session_id": session_id, "reason": reason, "message": message}),
        );
    };
    let fire_row = |dec: &str, name: &str, fp_bad: bool, extra: serde_json::Value| {
        let mut row = serde_json::json!({
            "session_id": session_id, "fingerprint": fingerprint,
            "fires": this_fire, "consecutive_unchanged": consecutive_after,
            "streak_window_secs": streak_window, "decision": dec,
            "intent": name, "intent_source": intent_source,
            "fp_read_failed": fp_bad
        });
        if let (Some(row), serde_json::Value::Object(extra)) = (row.as_object_mut(), extra) {
            row.extend(extra);
        }
        emit("loop_check", row);
    };

    // Run done() on active generic delivery, intent, backstop, or mute-probe; malformed findings cannot block.
    if generic.is_active()
        || intent != Intent::None
        || backstop_tripped
        || consecutive_after >= MUTE_PROBE_N
        || watching_fell_through
    {
        // Handle aborted first
        if let Intent::Aborted { ref reason } = intent {
            term_row("Aborted", reason);
            fire_row(
                "allow",
                "aborted",
                false,
                serde_json::json!({
                    "pr_state": last_pr_state,
                    "ci": last_ci,
                    "reviewed": false
                }),
            );
            return terminal(
                "allow",
                Some(TerminationReason::Aborted),
                "aborted tag detected",
            );
        }

        // ── gh availability + advisory mode, for fires that would read gh ─────
        // Moved inside the done gate  a no-intent fire beneath
        // the mute probe blocks locally and execs no gh. Only a NotFound
        // spawn reads as absence; every other spawn failure is SpawnTrouble
        // and must not degrade the session. The probe outcome rides its own
        // event row, never a loop_check row (a probe row with no fingerprint
        // would break the no-progress streak read above).
        let gh_probe = probe_gh_bin(gh_bin.as_ref(), &cwd);
        emit(
            "gh_probe",
            serde_json::json!({
                "session_id": session_id,
                "outcome": gh_probe.outcome_str(),
                "detail": gh_probe.detail_str(),
            }),
        );
        let gh_available = !matches!(gh_probe, GhProbeOutcome::Absent);
        if !gh_available
            && matches!(
                generic,
                crate::delivery_completion::DeliveryCompletion::Inactive
            )
        {
            if !manifest.attended && !manifest.advisory {
                // Unattended + no advisory + no gh -> Interrupted
                term_row(
                    "Interrupted",
                    "gh binary not found; unattended sessions require gh",
                );
                return terminal(
                    "allow",
                    Some(TerminationReason::Interrupted),
                    "gh binary not found; unattended sessions require gh",
                );
            }
            // Attended or declared advisory -> advisory mode (promise + budget
            // only). Budget was checked above; honor intent here so a promise
            // can terminate an advisory session (AC5-ERR) - gh reads are
            // impossible, so the promise alone is the completion signal.
            emit(
                "loop_advisory_mode",
                serde_json::json!({
                    "session_id": session_id,
                    "attended": manifest.attended
                }),
            );
            if intent == Intent::Promise {
                term_row(
                    "DoneAdvisory",
                    "promise accepted in advisory mode (gh unavailable)",
                );
                return terminal(
                    "allow",
                    Some(TerminationReason::DoneAdvisory),
                    "promise accepted in advisory mode (gh unavailable)",
                );
            }
            return terminal(
                "block",
                None,
                "gh binary not found; running in advisory mode (promise + budget only)",
            );
        }

        // Operator review-finding gate: an open
        // review_finding for this node HOLDS every success terminal-allow
        // (DonePlanned / DoneAdvisory / DoneDelivery / DoneBatched / DonePRGreen) until an
        // explicit resolve - a promise cannot self-authorize past an operator's
        // open comment. Placed AFTER the Aborted arm and gated on
        // `!backstop_tripped` so the anti-wedge safety valves still win: an
        // Aborted tag exits, and once the NoProgress backstop streak is reached
        // the session gives up rather than looping forever on an unresolved
        // finding. Fires on a promise OR a mute-probe (the paths that would
        // otherwise terminate-allow), never on an ordinary working fire.
        if !open_findings.is_empty()
            && !backstop_tripped
            && (intent == Intent::Promise || consecutive_after >= MUTE_PROBE_N)
        {
            let reason = build_findings_block_reason(&open_findings, malformed_findings);
            fire_row(
                "block",
                if intent == Intent::Promise {
                    "promise"
                } else {
                    "backstop"
                },
                false,
                serde_json::json!({
                    "pr_state": last_pr_state,
                    "ci": last_ci,
                    "reviewed": false,
                    "open_findings": open_findings.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
                    "malformed_findings": malformed_findings
                }),
            );
            return (
                0,
                allow_output("block", None, &reason, this_fire, Some(fingerprint)),
            );
        }

        if let Some(output) = crate::delivery_completion::gate_output(
            &generic,
            intent == Intent::Promise,
            &project_events,
            &global_events,
            &session_id,
            manifest.session_id.as_deref(),
            node_id.as_deref(),
            intent_source,
            &fingerprint,
            this_fire,
            backstop_tripped,
            consecutive_after,
            streak_window,
            &last_pr_state,
            &last_ci,
        ) {
            return (0, output);
        }

        // Plan-only unit: a plan-only thread reached the plan boundary. Checked
        // BEFORE the advisory unit because DoneAdvisory is a ship reason (it
        // graduates the plan) and a plan-only thread must not graduate its own
        // plan. DonePlanned is benign: not a ship reason, not a postmortem.
        if manifest.planned && intent == Intent::Promise {
            term_row("DonePlanned", "promise in plan-only unit");
            fire_row(
                "allow",
                "promise",
                false,
                serde_json::json!({
                    "pr_state": last_pr_state,
                    "ci": last_ci,
                    "reviewed": true
                }),
            );
            return terminal(
                "allow",
                Some(TerminationReason::DonePlanned),
                "promise + plan-only unit; done",
            );
        }

        // Advisory unit (no_ship or manifest advisory)
        if (manifest.no_ship || manifest.advisory) && intent == Intent::Promise {
            term_row("DoneAdvisory", "promise in advisory/no_ship unit");
            fire_row(
                "allow",
                "promise",
                false,
                serde_json::json!({
                    "pr_state": last_pr_state,
                    "ci": last_ci,
                    "reviewed": true
                }),
            );
            return terminal(
                "allow",
                Some(TerminationReason::DoneAdvisory),
                "promise + advisory unit; done",
            );
        }

        // Batched unit (batch-lane Wave 2/3): the node's commits live on a
        // shared batch branch and ship via the batch PR, not its own, so
        // run_done() below would block forever waiting for a per-node PR that
        // never comes. The daemon set `batched: true` at dispatch; a promise
        // here means the member finished committing to the shared branch.
        // Terminal as DoneBatched - deliberately NOT a ship reason, so finalize
        // records the ledger entry but does NOT stamp/graduate the plan (the
        // batch's own `/pr create` graduates it once, for all members). Comes
        // AFTER the advisory arm (a batched unit is not advisory: it sets
        // neither no_ship nor advisory) and BEFORE run_done so no PR is polled.
        if manifest.batched && intent == Intent::Promise {
            term_row(
                "DoneBatched",
                "promise in batched unit; commit landed on shared branch",
            );
            fire_row(
                "allow",
                "promise",
                false,
                serde_json::json!({
                    "pr_state": last_pr_state,
                    "ci": last_ci,
                    "reviewed": true
                }),
            );
            return terminal(
                "allow",
                Some(TerminationReason::DoneBatched),
                "promise + batched unit; commit on shared branch, batch PR ships it",
            );
        }

        // A code payload carries its own review obligation on a stock install:
        // when no lane is configured, the harness-resolved self-review reviewer is
        // floored onto `required_reviewers` so the existing unattested_reviewers_scan
        // holds the session for a head-pinned attestation instead of the run asking
        // an epic leader. Opt out with config.review.self_review_required = false.
        // classify_payload fails CLOSED, so an unreadable diff floors the reviewer
        // rather than waving the obligation away. The floor is additive: an
        // already-configured lane (reviewers, bots, peers) keeps meaning exactly
        // what it meant today, and a lane that already names code-review is a no-op.
        // It sits BELOW the stand-down gates on purpose: the payload consult can
        // reach gh (the PR's files), and a fire the quota floor or a recent
        // secondary refusal stands down must spend no gh read at all. Nothing
        // between those gates and run_done reads the floored set, so the late
        // placement changes only which fires pay for the consult.
        let lane_configured = !required_bots.is_empty()
            || inputs.optional_lane_configured
            || !required_reviewers.is_empty();
        let self_review_required = settings.self_review_required.unwrap_or(true);
        // The floor applies where a session can satisfy it: a harness with a
        // self-review verb (claude /code-review, codex /review, opencode
        // /review-changes), or a harness that could not be attributed at all -
        // ambiguity is not permission. A KNOWN verbless harness
        // (gemini/agy) stays unfloored: the floor would demand an attestation no
        // verb there produces, wedging the loop; route 3 (a spawned reviewer) is
        // those harnesses' path and is deferred.
        let floor_applies =
            self_review_floor_applies(author_harness.as_deref(), inputs.author_harness_pinned_none);
        let self_review_floor = if !lane_configured && self_review_required && floor_applies {
            let payload = classify_payload_for_floor(&parsed.gh_bin, &parsed.git_bin, &cwd, None);
            floor_self_review(&required_reviewers, false, payload.0, true)
        } else {
            None
        };
        if let Some(floored) = self_review_floor.clone() {
            required_reviewers.push(floored);
        }
        //: DoneUnreviewed applies only when review is required. A stock
        // install that opts out (self_review_required=false AND no lane, or a
        // harness with no self-review verb) has zero coverage as its configured
        // state, not a defect - those green PRs still reach DonePRGreen.
        let review_required = lane_configured || self_review_floor.is_some();

        // Run done() for code units
        let done_gh_bin = if intent == Intent::Promise {
            coverage_adapter(gh_bin)
        } else {
            gh_bin.to_string()
        };
        let done_result = run_done(
            &done_gh_bin,
            git_bin,
            &cwd,
            settings.ci_declared_none,
            manifest.no_external,
            &required_bots,
            &optional_bots,
            settings
                .optional_apps
                .as_ref()
                .is_some_and(|v| !v.is_empty()),
            &settings.external_reviewers,
            &required_reviewers,
            &nudge_configs,
            &head_sha,
            &project_events,
            &global_events,
            &repo_slug,
            manifest.harness_session_id.as_deref(),
            settings.github_approval_satisfies.unwrap_or(true),
            settings.max_rounds.unwrap_or(2).max(1),
            carry_interdiff_lines_resolved(&settings),
            Some(&resolve_posture_config(&settings)),
            &resolved_local_peer_reviewers_for_author(&settings, author_harness.as_deref()),
        );

        match done_result {
            Ok(mut pr_info) => {
                // Read 4's newest activity timestamp folds into the
                // fingerprint's 4th component: a late inline finding advances
                // the fingerprint (re-block, not NoProgress - the codex
                // findings-minutes-after-summary shape). The PR fields are
                // this fire's done() RESULT : the separate
                // fingerprint pre-read is gone, so a changed world shows up
                // here first, and the journal streak counted against the last
                // recorded fingerprint needs no recount when the result
                // matches it.
                let done_fp = make_fingerprint(
                    &head_sha,
                    pr_info.state.as_str(),
                    &pr_info.ci_conclusion.render(),
                    &pr_info.latest_review_ts,
                );
                let (fingerprint, consecutive_after, streak_window) = if done_fp != fingerprint {
                    (done_fp, 1, 0)
                } else {
                    (fingerprint, consecutive_after, streak_window)
                };
                let backstop_tripped = consecutive_after >= backstop_n;

                // The arm-scoped row builders: the fingerprint trio rebound
                // above and this PR read's five fields ride in the base.
                let term_row = |reason: &str, message: &str| {
                    emit(
                        "termination",
                        serde_json::json!({
                            "session_id": session_id, "reason": reason, "message": message
                        }),
                    );
                };
                let fire_row = |dec: &str, name: &str, fp_bad: bool, extra: Value| {
                    let mut row = serde_json::json!({
                        "session_id": session_id, "fingerprint": fingerprint,
                        "fires": this_fire, "consecutive_unchanged": consecutive_after,
                        "streak_window_secs": streak_window, "decision": dec,
                        "intent": name, "intent_source": intent_source,
                        "fp_read_failed": fp_bad,
                        "pr_state": pr_info.state.as_str(),
                        "ci": pr_info.ci_conclusion.render(),
                        "reviewed": pr_info.reviewed,
                        "review_skipped": pr_info.review_skipped,
                        "unaddressed_blocking": pr_info.unaddressed_findings.len()
                    });
                    if let (Some(row), Value::Object(extra)) = (row.as_object_mut(), extra) {
                        row.extend(extra);
                    }
                    emit("loop_check", row);
                };

                let terminal = |dec: &str, r, m: &str| {
                    (
                        0,
                        allow_output(dec, r, m, this_fire, Some(fingerprint.clone())),
                    )
                };

                // section 5: post the trigger for any NeedsNudge bot ONCE,
                // then treat it as Awaiting for this fire's messaging + idle read.
                // A NeedsNudge state means !reviewed, so no terminal below can
                // fire (they require reviewed=true); posting here is safe. A
                // failed post keeps NeedsNudge so the block message tells the
                // agent to post by hand (AC11) and the count is unchanged - a
                // failed post is never counted as a nudge.
                let nudge_pr_number = pr_info.number;
                for n in pr_info.bot_nudges.iter_mut() {
                    if n.class != NudgeClass::NeedsNudge {
                        continue;
                    }
                    if post_nudge_comment(gh_bin, &cwd, nudge_pr_number, &n.review_handle) {
                        emit(
                            "loop_check_nudge_posted",
                            serde_json::json!({
                                "session_id": session_id,
                                "pr": nudge_pr_number,
                                "bot": n.login,
                                "handle": n.review_handle,
                                "nudge": n.nudges + 1,
                                "ceiling": n.ceiling
                            }),
                        );
                        n.nudges += 1;
                        n.newest_age_min = 0;
                        n.class = NudgeClass::Awaiting;
                    } else {
                        emit(
                            "loop_check_nudge_post_failed",
                            serde_json::json!({
                                "session_id": session_id,
                                "pr": nudge_pr_number,
                                "bot": n.login,
                                "handle": n.review_handle
                            }),
                        );
                    }
                }

                let ci_ok = pr_info.ci_conclusion.is_ok();
                let pr_open = pr_info.state.is_open_or_merged();
                // codex P1 on #447: a green PR must also contain the local
                // HEAD - otherwise unpushed work terminates as DonePRGreen
                // without ever shipping. MERGED PRs are exempt only when the
                // local HEAD matches too; an unpushed commit on top of a
                // merged PR is still unshipped work.
                //
                // CONTAIN, not EQUAL. This read was `head_oid == head_sha` for
                // a year, and equality is a proxy for containment that is
                // wrong in one direction: a DESCENDANT of the merge has
                // nothing left to ship, and equality called it unshipped
                // anyway. That wedged two finished sessions past their own
                // merges (PR #671 in July, PR #1071 in August) with an
                // unactionable "push the latest commits". `head_is_shipped`
                // keeps the #447 guard intact: a real extra commit is not an
                // ancestor of the base, so it still blocks.
                let head_shipped = head_is_shipped(&pr_info, &head_sha, git_bin, &cwd);

                // done_probes: the FINAL DonePRGreen conjunct. Gated on
                // every other conjunct already holding, so a plan with no probes
                // spawns no subprocess and a red/unreviewed PR never pays for one.
                let (mut probe_block, mut probe_results) = (None, Value::Null);
                if pr_open && ci_ok && pr_info.reviewed && head_shipped {
                    match evaluate_done_probes(
                        manifest.plan_path.as_deref(),
                        settings.done_probes.as_ref(),
                        &cwd,
                        &project_events,
                        &session_id,
                        PROBE_TIMEOUT,
                    ) {
                        ProbeGate::Absent => {}
                        ProbeGate::Pass(results) => probe_results = results,
                        ProbeGate::Fail { reason, results } => {
                            probe_block = Some(reason);
                            probe_results = results;
                        }
                    }
                }

                // plan fidelity: the stop-gate half of AC5. A plan whose
                // declared deliverables did not all ship blocks DonePRGreen until
                // each shortfall carries a carveout - the agent files one and the
                // next eval passes. Gated on the same conjuncts as done_probes and
                // fail-open on a stale/missing fno (the merge gate is the backstop).
                let mut fidelity_block: Option<String> = None;
                if pr_open && ci_ok && pr_info.reviewed && head_shipped {
                    let fno_bin =
                        std::env::var_os("FNO_LOOPCHECK_FNO_BIN").unwrap_or_else(|| "fno".into());
                    match evaluate_plan_fidelity(
                        manifest.plan_path.as_deref(),
                        &fno_bin,
                        &cwd,
                        // The 60s ceiling, clamped to the fire budget: a
                        // fidelity probe is a stop-gate read like any other,
                        // and one read must not spend more than the fire
                        // still has before the harness kills the hook.
                        clamp_to_fire_deadline(FIDELITY_TIMEOUT),
                    ) {
                        FidelityGate::Refused { reason } => fidelity_block = Some(reason),
                        // Degraded fails OPEN on the stop decision (same as Absent - a
                        // hung probe must not wedge the gate that lets a finished
                        // session stop), but is emitted here so it is never a SILENT
                        // pass: a probe that keeps timing out stays visible in the
                        // event log even though it never blocks.
                        FidelityGate::Degraded { reason } => emit(
                            "loop_check_fidelity_degraded",
                            serde_json::json!({
                                "session_id": session_id,
                                "plan_path": manifest.plan_path,
                                "reason": reason
                            }),
                        ),
                        _ => {}
                    }
                }

                let (reviewed, probes_passed) = (
                    pr_info.reviewed,
                    probe_block.is_none() && fidelity_block.is_none(),
                );
                if pr_open
                    && ci_ok
                    && head_shipped
                    && probes_passed
                    && awaiting_review_only(&pr_info)
                {
                    let reviewers = pr_info.coverage.refused_reviewers().join(", ");
                    let msg = format!(
                        "PR #{} is green and shipped, but reviewer(s) {} refused to review; the review gate cannot be auto-satisfied. Run a local review at HEAD, wait for reviewer recovery, or merge manually after a real review.",
                        pr_info.number, reviewers
                    );
                    term_row("DoneAwaitingReview", &msg);
                    fire_row(
                        "allow",
                        if intent == Intent::Promise {
                            "promise"
                        } else {
                            "backstop"
                        },
                        false,
                        serde_json::json!({
                            "review_state": "reviewer_refused"
                        }),
                    );
                    best_effort_notify(
                        &format!("PR #{} blocked - reviewer refused", pr_info.number),
                        &msg,
                    );
                    return terminal("allow", Some(TerminationReason::DoneAwaitingReview), &msg);
                }
                if pr_passes(pr_open, ci_ok, reviewed, head_shipped, probes_passed) {
                    // Coverage gate: the three pr_passes conjuncts all ask
                    // "did anyone object"; coverage asks "did anyone review". A
                    // passing PR nothing reviewed terminates DoneUnreviewed, not
                    // DonePRGreen - terminal on first eval (no PR #214 wedge),
                    // never a ship reason (never arms auto-merge). The
                    // discriminator is coverage, NOT the `attended` manifest field
                    //. A MERGED PR
                    // is exempt: the merge (human out-of-band, or an earlier
                    // autonomous arm) is the terminal authority, and
                    // loop-check must not re-litigate review on an already-merged
                    // PR - the coverage fix prevents the autonomous MERGE (arming),
                    // not the post-merge terminal.
                    let mut waived_green_description: Option<String> = None;
                    if review_required
                        && pr_info.state != PrState::Merged
                        && !pr_info.coverage.coverage.is_covered()
                    {
                        // The operator-law overlay, the same resolver the merge
                        // gate and the publisher apply: a waiver turns this
                        // head covered, so the loop terminates DonePRGreen with
                        // the waiver NAMED instead of wedging green PRs behind
                        // an unreviewed terminal no review can clear. Unknown
                        // authority keeps DoneUnreviewed - fail closed on a
                        // store that could not answer.
                        let (waiver, _authority_unknown) = operator_waiver(
                            &parsed.fno_bin,
                            &cwd,
                            &repo_slug,
                            pr_info.number,
                            &pr_info.head_oid,
                            pr_info.range_tiling.hard_blocker,
                        );
                        // The hint renders the exact sized invocation (Python
                        // single source) so an unreviewed-green termination
                        // names what to run, not just that something must be.
                        // Past the round cap that hint becomes the loop: the
                        // receipt then carries the spent budget and names the
                        // terminal act instead. The max here is the same
                        // resolve read_pr_info judged the tiling against.
                        if waiver.is_none() {
                            let mut cov_line = coverage_receipt_line(
                                &pr_info.coverage,
                                sized_self_review_hint(
                                    &parsed.fno_bin,
                                    &cwd,
                                    author_harness.as_deref(),
                                )
                                .as_deref(),
                                pr_info.range_tiling.rounds_exhausted.then(|| {
                                    (
                                        pr_info.range_tiling.rounds_used,
                                        settings.max_rounds.unwrap_or(2).max(1),
                                    )
                                }),
                            );
                            if _authority_unknown {
                                cov_line = format!(
                                    "{cov_line}; operator waiver authority unknown (decision probe)"
                                );
                            }
                            let done_msg = format!(
                                "PR #{} is green but UNREVIEWED - {}. Not mergeable by the autonomous path (DoneUnreviewed); merge by hand or after a review.",
                                pr_info.number, cov_line
                            );
                            term_row("DoneUnreviewed", &done_msg);
                            fire_row(
                                "allow",
                                if intent == Intent::Promise {
                                    "promise"
                                } else {
                                    "backstop"
                                },
                                false,
                                serde_json::json!({
                                    "coverage": coverage_event_data(pr_info.number, &pr_info.coverage, &head_sha, &repo_slug, manifest.harness_session_id.as_deref()),
                                    "done_probes": probe_results
                                }),
                            );
                            return terminal(
                                "allow",
                                Some(TerminationReason::DoneUnreviewed),
                                &done_msg,
                            );
                        }
                        waived_green_description = waiver;
                    }
                    // A rate-limited bot now fails the gate closed, so a
                    // green+reviewed DonePRGreen can never carry one: reaching
                    // here means every required bot has a real completed pass.
                    // The not-required arm must never say "reviewed": no
                    // coverage check ran, so the word would assert the exact
                    // opposite of the uncovered row this same fire emits
                    // (: PR 1294's terminal said "green and reviewed"
                    // while its coverage event said uncovered, and auto-merge
                    // acted on the terminal).
                    let done_msg = match waived_green_description {
                        Some(ref description) => format!(
                            "PR #{} is green; review coverage waived ({})",
                            pr_info.number, description
                        ),
                        None if !review_required => format!(
                            "PR #{} is green; review not required (no review lane and the self-review floor does not apply)",
                            pr_info.number
                        ),
                        None => format!("PR #{} is green and reviewed", pr_info.number),
                    };
                    term_row("DonePRGreen", &done_msg);
                    fire_row(
                        "allow",
                        if intent == Intent::Promise {
                            "promise"
                        } else {
                            "backstop"
                        },
                        false,
                        serde_json::json!({
                            "done_probes": probe_results
                        }),
                    );
                    return terminal("allow", Some(TerminationReason::DonePRGreen), &done_msg);
                }

                // DoneAwaitingMerge (ruling hold): a crown's dispatch_hold on
                // this session's node is proof on its own, so this gate does
                // NOT require `reviewed` (the review read flaps true/false on
                // alternate fires while a PR sits held; the ruling outranks
                // it). Graph read fires only once every other condition
                // already holds, so a normal fire pays nothing for it.
                if pr_open
                    && head_shipped
                    && !ci_ok
                    && pr_info.unaddressed_findings.is_empty()
                    && pr_info.mergeable != "CONFLICTING"
                {
                    if let Some(guard_reason) =
                        node_id.as_deref().and_then(awaiting_merge::ruling_hold)
                    {
                        let msg = format!(
                            "PR #{} complete; merge held by {guard_reason}; see fno do pr hold-check {}",
                            pr_info.number, pr_info.number
                        );
                        if !awaiting_merge::already_emitted_awaiting_merge(
                            &project_events,
                            &session_id,
                        ) {
                            term_row("DoneAwaitingMerge", &msg);
                            fire_row(
                                "allow",
                                if intent == Intent::Promise {
                                    "promise"
                                } else {
                                    "backstop"
                                },
                                false,
                                serde_json::json!({}),
                            );
                            best_effort_notify(
                                &format!("PR #{} ready - merge held by ruling", pr_info.number),
                                &msg,
                            );
                        }
                        return terminal("allow", Some(TerminationReason::DoneAwaitingMerge), &msg);
                    }
                }

                // DoneAwaitingMerge: done() failed SOLELY on CI-green
                // (PR open, reviewed, HEAD shipped, but CI red). Reached only
                // when !ci_ok because the DonePRGreen arm above returned - so
                // DonePRGreen precedence holds, and a merge that flipped the PR
                // green would have been caught by the fresh run_done this fire
                // (AC1-FR). If current main HEAD is red on the SAME checks
                // (strict subset, check-name granularity), a bg agent cannot
                // merge past it: terminate clean with a one-shot notify instead
                // of burning to NoProgress. Any PR-unique red or any gh
                // uncertainty falls through to the hold below (fail closed).
                //
                // `!pr_info.ci_has_pending` is load-bearing: ci_conclusion
                // reports Failure as soon as ONE check fails while others still
                // run, so without this guard the terminal could fire on a
                // partial-CI fire where the session's OWN new job is still
                // pending and about to turn red. The terminal must see fully
                // settled-red CI, never partial.
                //
                // `mergeable != "CONFLICTING"` guards a reviewed PR whose branch
                // conflicts with main: the human cannot merge past main-red until
                // it is rebased, so terminating here would drop the node from
                // retry circulation while it is un-mergeable. UNKNOWN (still
                // computing) is allowed - it clears on its own.
                if pr_open
                    && pr_info.reviewed
                    && head_shipped
                    && !ci_ok
                    && !pr_info.ci_has_pending
                    && pr_info.mergeable != "CONFLICTING"
                {
                    if let Some(main_failing) = main_head_failing_checks(gh_bin, &cwd) {
                        if awaiting_merge::is_pre_existing_main_red(
                            &pr_info.failing_checks,
                            &main_failing,
                        ) {
                            let proof = format!(
                                "same checks red on main's latest verdict per workflow: {}",
                                pr_info.failing_checks.join(", ")
                            );
                            let msg = format!(
                                "PR #{} complete and reviewed; awaiting merge past pre-existing main-red ({proof})",
                                pr_info.number
                            );
                            // Idempotency (Concurrency AC): emit + notify at most
                            // once per session; a re-eval or the two consumers
                            // racing still returns the terminal but does not
                            // double-notify.
                            if !awaiting_merge::already_emitted_awaiting_merge(
                                &project_events,
                                &session_id,
                            ) {
                                term_row("DoneAwaitingMerge", &msg);
                                fire_row(
                                    "allow",
                                    if intent == Intent::Promise {
                                        "promise"
                                    } else {
                                        "backstop"
                                    },
                                    false,
                                    serde_json::json!({}),
                                );
                                best_effort_notify(
                                    &format!(
                                        "PR #{} ready - merge past pre-existing main-red",
                                        pr_info.number
                                    ),
                                    &msg,
                                );
                            }
                            return terminal(
                                "allow",
                                Some(TerminationReason::DoneAwaitingMerge),
                                &msg,
                            );
                        }
                    }
                }

                // ── Watching idle-allow ─────────────────────────────
                // A verified async wait (CI pending or awaiting a bot review,
                // head pushed, zero unaddressed findings) plus an agent-armed
                // <watching> tag idles NON-terminally: the harness re-invokes the
                // model when the agent's watcher task exits, so re-blocking every
                // ~90s tick until then is pure no-op overhead. done() and every
                // terminal above already ran (a terminal always beats an idle),
                // and this sits BEFORE the NoProgress backstop so a long watched
                // wait degrades to budget/claim-expiry, never a spurious kill.
                //
                // The observation is hoisted out of the intent arm on purpose
                //: async_wait_class reads external truth (PR open,
                // head shipped, no findings, the wait class) and the backstop
                // below needs the same truth. The idle-allow only rescues a
                // session whose lease renewal succeeds and whose harness can
                // idle; a fall-through there must not reach a terminal built
                // from absence.
                let observed_async_wait =
                    async_wait_class(&pr_info, open_findings.is_empty(), head_shipped);

                //: a freshly-posted nudge sits in Awaiting until
                // wait_minutes elapses. On a harness that cannot idle on a
                // `<watching>` tag (a loop-run child, codex/gemini, or a failed
                // lease renewal) the fingerprint is stable, so without this guard
                // the generic backstop reaps the wait after backstop_n fires -
                // before the nudge cycle reaches its ceiling, terminating with a
                // generic NoProgress instead of the named give-up. Suppress the
                // backstop ONLY when the sole unmet condition is a live Awaiting
                // nudge: it is self-limiting (Awaiting -> Unresponsive after
                // wait_minutes, when this guard clears and the backstop reaps it
                // naming the bot), and the narrow scope keeps CI red, a finding,
                // an unattested reviewer, or a failed probe tripping it as before.
                let sole_blocker_is_awaiting = pr_open
                    && ci_ok
                    && probe_block.is_none()
                    && !pr_info.reviewed
                    && pr_info.unattested_reviewers.is_empty()
                    && pr_info.unaddressed_findings.is_empty()
                    && pr_info
                        .bot_nudges
                        .iter()
                        .any(|n| n.class == NudgeClass::Awaiting);
                //: the observation guard for BOTH async-wait classes. CI
                // still pending, or an outstanding bot in an idlable nudge state,
                // is a runtime-OBSERVED wait (PR open, head shipped, no
                // findings) - external truth that work is in flight. NoProgress
                // asserts the opposite, so the backstop must not fire on it,
                // regardless of whether the idle-allow engaged: its
                // preconditions (lease renewal, harness idling) fail for reasons
                // unrelated to liveness, and the fall-through is what wrote a
                // terminal for a live CI-waiting session. The
                // same-model-peer sentinel is the deliberate exception: nothing
                // can EVER satisfy that wait (the configured peer is the
                // author's own model), so NoProgress is then true rather than a
                // lie, and the reviewers-gate arm keeps the backstop reaping it.
                // probe_block.is_none() keeps the never-passing-probe escape
                // above intact; a suppressed backstop keeps blocking and
                // degrades to budget/claim-expiry.
                let sole_blocker_is_observed_wait = pr_open
                    && probe_block.is_none()
                    && observed_async_wait.is_some()
                    && !pr_info
                        .missing_bots
                        .iter()
                        .any(|b| b == SAME_MODEL_PEER_SENTINEL);
                // `probe_block.is_some()` keeps a probe that can never pass in
                // this environment on the NoProgress escape rather than looping
                // to the budget ceiling: PR+CI+review all hold, so without it
                // none of the other disjuncts can ever fire.
                if backstop_tripped
                    && (!pr_open || !ci_ok || !pr_info.reviewed || probe_block.is_some())
                    && !sole_blocker_is_awaiting
                    && !sole_blocker_is_observed_wait
                {
                    // Backstop tripped + done() false -> NoProgress. AC13:
                    // when a nudged bot never answered, the operator's question is
                    // "is this going to finish, and must I do something" - so name
                    // the bot + nudge count + elapsed instead of a bare fingerprint
                    // streak, and reach the operator (who is not watching the pane)
                    // with exactly one notification.
                    let nudge_giveup = unresponsive_bot(&pr_info);
                    let noprogress_msg = match nudge_giveup {
                        Some(n) => nudge_giveup_message(n),
                        None => format!(
                            "fingerprint unchanged for {} consecutive fires over {}m; PR not done",
                            consecutive_after,
                            streak_window / 60
                        ),
                    };
                    if let Some(n) = nudge_giveup {
                        best_effort_notify(
                            "target: bot review gave up",
                            &format!(
                                "PR #{}: {} did not review after {} nudges over {}m",
                                pr_info.number, n.login, n.nudges, n.span_min
                            ),
                        );
                    }
                    // Backstop tripped + done() false -> NoProgress
                    term_row("NoProgress", &noprogress_msg);
                    fire_row(
                        "allow",
                        "backstop",
                        false,
                        serde_json::json!({
                            "done_probes": probe_results
                        }),
                    );
                    let return_msg = match nudge_giveup {
                        Some(_) => noprogress_msg.clone(),
                        None => format!(
                            "fingerprint unchanged for {} fires over {}m; HEAD={}, PR={}, CI={}, reviewed={}",
                            consecutive_after,
                            streak_window / 60,
                            short_sha(&head_sha),
                            pr_info.state.as_str(),
                            pr_info.ci_conclusion.render(),
                            pr_info.reviewed
                        ),
                    };
                    return terminal("allow", Some(TerminationReason::NoProgress), &return_msg);
                }

                // A refused watch composes its refusal here, where the real
                // blocker and finding count exist: the message keeps both the
                // refusal and the actionable reason (never a blind block).
                let watching_refusal = if watching_fell_through {
                    let is_loop_run_child = std::env::var("FNO_DRIVER_LIB").is_ok();
                    let can_idle = harness_can_idle(author_harness.as_deref(), is_loop_run_child);
                    let blocker = if can_idle { observed_async_wait } else { None };
                    let claim = watch_lease::claim_pair(&manifest_content);
                    let mut lease_cause: Option<watch_lease::RenewCause> = None;
                    if can_idle && blocker.is_some() {
                        let tag_timeout = match &intent {
                            Intent::Watching { timeout, .. } => timeout.clone(),
                            _ => None,
                        };
                        let window_ms = watch_window_ms(tag_timeout.as_deref());
                        let renew_outcome = claim.as_ref().map(|(key, holder)| {
                            crate::claims::renew(key, holder, window_ms, None)
                        });
                        if !matches!(renew_outcome.as_ref(), Some(Ok(true))) {
                            lease_cause =
                                watch_lease::declined_cause(claim.as_ref(), renew_outcome.as_ref());
                        }
                    }
                    let r = watch_lease::idle_refusal(
                        can_idle,
                        author_harness.as_deref(),
                        is_loop_run_child,
                        blocker.is_none(),
                        pr_info.unaddressed_findings.len(),
                        claim.is_some(),
                        lease_cause.as_ref(),
                    );
                    Some((r.reason, r.kind))
                } else {
                    None
                };
                // done() false on promise -> block with named reason. P2
                //: enrich with a loop-boundary inbox nudge.
                // A failed probe OR a fidelity refusal IS the blocker when
                // everything else is green; build_block_reason would otherwise
                // report a healthy PR.
                let block_reason = probe_block
                    .clone()
                    .or(fidelity_block.clone())
                    .unwrap_or_else(|| {
                        build_block_reason(
                            &pr_info,
                            &head_sha,
                            open_findings.is_empty(),
                            head_shipped,
                        )
                    });
                let block_reason = match &watching_refusal {
                    // A permanent refusal already said no watcher can help, so
                    // the arm hint the classifier appended would contradict it
                    // inside one message. A harness that cannot self-wake is
                    // permanent the same way: its hint can never be honored.
                    // Cut the hint, keep the blocker.
                    Some((text, kind))
                        if watch_lease::refusal_is_permanent(text) || *kind == "harness" =>
                    {
                        format!("{text}; {}", watch_lease::without_arm_hint(&block_reason))
                    }
                    Some((text, _)) => format!("{text}; {block_reason}"),
                    None => block_reason,
                };
                let reason = crate::nudge::append_inbox_nudge(&block_reason, &cwd, &session_id);
                let mut watch_extra = serde_json::json!({
                    "done_probes": probe_results
                });
                watch_lease::attach_watch_refusal(
                    &mut watch_extra,
                    watching_refusal.as_ref().map(|(_, kind)| *kind),
                );
                fire_row(
                    "block",
                    if intent == Intent::Promise {
                        "promise"
                    } else {
                        "none"
                    },
                    false,
                    watch_extra,
                );
                return (
                    0,
                    allow_output("block", None, &reason, this_fire, Some(fingerprint)),
                );
            }
            Err(read_err) => {
                let failed_read = read_err.read.clone();
                let failed_stderr = read_err.stderr_tail.clone();
                // US4 (locked decision 6, REVERSES the wedge's behavior): a
                // gh-errored done() read NEVER terminates NoProgress, even
                // with the backstop tripped - a healthy session must not be
                // killed because GitHub blipped. The fire blocks-and-retries
                // and is recorded fp_read_failed=true, keeping it transparent
                // to the streak. Budget is NOT the sole ceiling during a
                // sustained outage: on Claude Code the harness itself caps
                // consecutive Stop-hook blocks (CLAUDE_CODE_STOP_HOOK_BLOCK_CAP,
                // default 9) and force-ends the turn once it binds - which on
                // the unraised harness happens long before budget, exactly the
                // truncation. fno raises the cap for spawned workers
                // (see _mesh_env_wrapper / the bg spawn_env), so its own
                // NoProgress/budget terminals bind first in normal operation;
                // but during a pure gh-read outage the (raised, finite) cap is
                // still the binding ceiling, not budget. AC4-EDGE holds only in
                // the sense that budget is checked before any gh read, so a gh
                // outage alone never makes a session immortal from fno's side.
                // Name the real reason when the quota is the reason.
                // "retrying next fire" is the right advice for a blip and
                // the worst possible advice for an exhausted quota: it burns a
                // fire every tick for the whole reset window on a call that
                // cannot succeed. The probe is REST and primary-exempt, so it
                // still answers while GraphQL is at 0; a failed probe keeps
                // the transient wording rather than guessing.
                // Reuse the fire-start probe; re-probe only if it failed, so a
                // blip at the top still gets its one retry without a second
                // `gh api rate_limit` on every error fire (request-rate cost).
                //
                // pulls_comments(_parse) and pr_commits share this error arm but
                // are not all GraphQL: pulls_comments is a REST endpoint
                // (`gh api .../pulls/N/comments`). A zero-remaining probe
                // must not blame GraphQL for a REST read's own failure - that
                // reads as "stop retrying gh pr view" advice for a call that was
                // never gh pr view and might succeed on the very next fire.
                let is_graphql_read = is_graphql_read(&failed_read);
                // The quota read stays on the failed-read path only :
                // a healthy fire pays no `gh api rate_limit`.
                let quota = probe_graphql_quota(gh_bin, &cwd);
                // Classified against the LIVE exempt bucket, never wording
                // (see `refusal_is_secondary`), and the verdict rides the
                // event as a FIELD: journal readers match the field, so a
                // GitHub reword cannot misread a stored refusal.
                let secondary =
                    refusal_is_secondary(&failed_stderr, quota.as_ref(), is_graphql_read);
                // The ledger is the fleet's refusal memory now (the journal
                // scan that read the row below is gone), so a refusal the
                // GATE itself observes must open the same backoff a Python
                // REST read opens - or every later fire re-probes and
                // re-attempts the refused read. The gate obeys the same
                // writer rule as _quota.record_refusal: only stderr carrying
                // HTTP 403/429 records. The weaker wording-only verdict still
                // fails this fire TOWARD backoff, but it never opens a
                // machine-wide wall on evidence a mock, a proxy, or a reword
                // could manufacture. A no-op while a backoff is already live.
                if secondary
                    && (failed_stderr.contains("HTTP 403") || failed_stderr.contains("HTTP 429"))
                {
                    let _ =
                        crate::gh_budget::record_refusal(&budget_ledger, now.timestamp_millis());
                }
                emit(
                    "loop_check_gh_error",
                    serde_json::json!({
                        "session_id": session_id,
                        "read": failed_read,
                        "outcome": read_err.outcome(),
                        "stderr_tail": failed_stderr,
                        "elapsed_s": read_err.elapsed.map(|d| d.as_secs_f64()),
                        "graphql_remaining": quota.as_ref().map(|q| q.remaining),
                        "graphql_reset": quota.as_ref().map(|q| q.reset_epoch),
                        "rate_limit_class": secondary.then_some("secondary")
                    }),
                );
                fire_row(
                    "block",
                    if intent == Intent::Promise {
                        "promise"
                    } else {
                        "none"
                    },
                    true,
                    serde_json::json!({
                        "pr_state": "unknown",
                        "ci": "unknown",
                        "reviewed": false
                    }),
                );
                // Checked BEFORE the primary-quota branch and independent of
                // it: a secondary (burst/concurrency) refusal can fire with
                // the primary quota reading thousands remaining (measured
                // live: core 4922/5000, graphql 1392/5000, refused anyway).
                // `secondary` above already classified it against the LIVE
                // exempt bucket - wording alone missed GitHub's measured body
                // - and naming it as primary exhaustion sends the caller to
                // wait for a reset that can be 40 minutes away for a limit
                // that actually clears in seconds - the exact "assert a
                // positive marker, never an absence" trap this whole
                // diagnosis exists to avoid.
                // A killed timeout renders its own sentence and never falls
                // through the ordinary failed-read wording: the two demand
                // opposite operator responses, and the quota probe below must
                // not label a killed child as a quota problem either.
                let reason = if read_err.kind == ReadErrorKind::TimedOut {
                    read_err.render()
                } else if secondary {
                    format!(
                        "gh read '{failed_read}' hit GitHub's SECONDARY rate limit (burst/\
                         concurrency, not the hourly quota - `gh api rate_limit` can read \
                         thousands remaining here and still be wrong about this). Back off \
                         briefly and retry; do not wait for a primary-quota reset, it can \
                         clear in seconds. {failed_stderr}"
                    )
                } else {
                    match &quota {
                        Some(q) if q.remaining == 0 && is_graphql_read => {
                            graphql_exhausted_reason(q)
                        }
                        _ => format!(
                            "gh read '{failed_read}' failed; retrying next fire. {failed_stderr}"
                        ),
                    }
                };
                return (
                    0,
                    allow_output("block", None, &reason, this_fire, Some(fingerprint)),
                );
            }
        }
    }

    // ── Step 5: no intent, no backstop -> block, record fingerprint ───────────
    fire_row(
        "block",
        "none",
        false,
        serde_json::json!({
            "pr_state": last_pr_state,
            "ci": last_ci,
            "reviewed": false
        }),
    );

    // P2: the dominant loop-yield boundary. Enrich the continue
    // message with a one-line inbox nudge so an autonomous loop surfaces mail.
    let continue_msg = crate::nudge::append_inbox_nudge(
        "continue working; no completion signal. If you are only waiting on an async check (CI/review) with nothing to do, arm a harness-tracked watcher with a hard timeout (e.g. background Bash `fno do pr wait <N> --until settled --timeout=30m` - REST through the coalescing cache, 60s interval, never `gh pr checks --watch`, which spends the shared GraphQL quota; a review wait is `--until review`) and end your turn with `<watching reason=\"ci|review\" pr=\"<N>\" timeout=\"30m\">` - the session idles until the watcher exits instead of re-waking every tick.",
        &cwd,
        &session_id,
    );
    (
        0,
        allow_output("block", None, &continue_msg, this_fire, Some(fingerprint)),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_done(
    gh_bin: &str,
    git_bin: &str,
    cwd: &Path,
    ci_declared_none: bool,
    no_external: bool,
    required_bots: &[String],
    optional_bots: &[String],
    optional_lane_configured: bool,
    external_reviewers: &[String],
    reviewers: &[String],
    nudge_configs: &[NudgeConfig],
    head_sha: &str,
    events_path: &Path,
    global_events_path: &Path,
    repo_slug: &str,
    author_session: Option<&str>,
    github_approval_satisfies: bool,
    max_rounds: i64,
    carry_interdiff_lines: usize,
    posture: Option<&PostureConfig>,
    peer_reviewers: &[String],
) -> Result<PrInfo, GhReadError> {
    let info = read_pr_info(
        gh_bin,
        git_bin,
        cwd,
        ci_declared_none,
        no_external,
        required_bots,
        optional_bots,
        optional_lane_configured,
        external_reviewers,
        reviewers,
        nudge_configs,
        head_sha,
        events_path,
        global_events_path,
        repo_slug,
        author_session,
        None,
        None,
        github_approval_satisfies,
        max_rounds,
        carry_interdiff_lines,
        posture,
        peer_reviewers,
    )?;
    // read_pr_info stays read-only against GitHub; the CALLER owns
    // the write. A row was just emitted for this PR, so the publisher has the
    // same evidence the merge gate will read, and the status targets the live
    // PR head the row is compared against - not merely the local HEAD. Only
    // an OPEN PR: read_pr_info short-circuits a MERGED PR to the
    // Covered(0) sentinel, and publishing that as failure would flip the
    // latest status on a merged head red after the merge passed the gate.
    if matches!(info.state, PrState::Open) {
        publish_coverage_status(
            gh_bin,
            &loopcheck_fno_bin(),
            cwd,
            repo_slug,
            info.number,
            &info.head_oid,
            head_sha,
            &info.coverage,
            required_bots,
            optional_bots,
            optional_lane_configured,
            reviewers,
            info.range_tiling.rounds_exhausted,
            info.range_tiling.hard_blocker,
        );
    }
    Ok(info)
}

/// First 8 chars of a sha, never bytes. `&s[..8]` panics when byte offset 8
/// lands inside a multibyte character, and one of these strings comes from a
/// user-writable events.jsonl - a panic there takes the whole stop gate down.
fn short_sha(s: &str) -> String {
    s.chars().take(8).collect()
}

mod read_bounds;

pub(crate) use read_bounds::{
    clamp_to_fire_deadline, drain_reserve_half_spent, stopgate_drain_reserve_ms,
    stopgate_drain_timeout, stopgate_fire_remaining_ms, stopgate_read_timeout, stopgate_stamp_fire,
    STOPGATE_BOUND_FLOOR, STOPGATE_FIRE_BUDGET,
};

// ── king driver arm ───────────────────────────────────────────────────────────
//
// A target driver asks whether its one deliverable shipped: PR, CI, review,
// probes. A king has no PR, so pointing the target driver at one can never
// reach a clean terminal state; it burns to NoProgress or Budget while looking
// like it is working. This arm asks the king's question instead, which is
// whether the board is clean, and reads that answer from `fno inbox board`
// rather than deciding anything itself.
//
// It is deliberately self-contained. The target arm below is untouched, which
// is also what the plan's engine_edit kill criterion exists to enforce.

pub(crate) use crate::king_termination::{parse_king_manifest, KingManifest};

// ── public entry points ───────────────────────────────────────────────────────

fn observe_decision(args: &[String], output: &str) {
    let Ok(parsed) = parse_args(args) else {
        return;
    };
    if parsed.driver != "target" {
        return;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(output) else {
        return;
    };
    let transition = if let Some(reason) = payload
        .get("termination_reason")
        .and_then(serde_json::Value::as_str)
    {
        let Ok(record) = crate::run_outcome::classify_legacy(reason) else {
            return;
        };
        let projection = record.projection();
        if projection.cancelled {
            crate::run_state::RunEvent::Cancel
        } else if projection.stuck {
            crate::run_state::RunEvent::Abort
        } else {
            crate::run_state::RunEvent::TerminalDecided
        }
    } else if payload.get("decision").and_then(serde_json::Value::as_str) == Some("block") {
        crate::run_state::RunEvent::DispatchClassified
    } else {
        return;
    };
    let Ok(manifest) = std::fs::read_to_string(&parsed.state_path) else {
        return;
    };
    let Some(session_id) = parse_manifest(&manifest).and_then(|value| value.session_id) else {
        return;
    };
    let project_events = parsed
        .events_path
        .unwrap_or_else(|| crate::paths::events_path(&parsed.cwd));
    let global_events = parsed.global_events_path.unwrap_or_else(|| {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(".fno/events.jsonl")
    });
    let run_log = crate::paths::worktree_space_dir(&parsed.cwd).join("run-log.jsonl");
    if transition == crate::run_state::RunEvent::TerminalDecided
        && matches!(
            crate::run_state::fold_run_state(&run_log, &session_id),
            Ok(crate::run_state::RunState::Open)
        )
    {
        // A plan-only/advisory/batched run can terminate on its first fire.
        // Seed the observer's dispatch arm so the legacy terminal remains
        // unchanged while the shadow journal records a legal path.
        observe_shadow_transition(
            &run_log,
            &session_id,
            crate::run_state::RunEvent::DispatchClassified,
            &project_events,
            &global_events,
        );
    }
    observe_shadow_transition(
        &run_log,
        &session_id,
        transition,
        &project_events,
        &global_events,
    );
}

pub fn decide(args: &[String]) -> (i32, String) {
    let result = session_binding::render_continuation_for_harness(args, decide_inner(args));
    if result.0 == 0 {
        observe_decision(args, &result.1);
    }
    result
}

/// Entry point called from `bin/client.rs` direct dispatch.
/// Prints JSON to stdout, returns exit code.
pub fn run_loop_check(args: &[String]) -> i32 {
    let (code, json) = decide(args);
    println!("{json}");
    code
}

/// Test-friendly variant that returns (exit_code, json_string) without printing.
/// Used by integration tests in tests/loop_check.rs.
pub fn run_loop_check_capture(args: &[String]) -> (i32, String) {
    decide(args)
}

// ── unit tests ───────────────────────

/// The production source of this module and every child module, test
/// modules cut, for the guards that scan source text. Read at run time so a
/// new child module cannot escape a scan that would have to list it.
#[cfg(test)]
pub(crate) fn production_source() -> String {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let cut = |path: std::path::PathBuf| {
        let text = std::fs::read_to_string(&path).unwrap();
        text.split("\nmod tests {").next().unwrap_or("").to_string()
    };
    let mut children: Vec<_> = std::fs::read_dir(src.join("loopcheck"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    children.sort();
    let mut out = cut(src.join("loopcheck.rs"));
    for child in children {
        out.push('\n');
        out.push_str(&cut(child));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::read_bounds::clamp_to_fire_budget;
    use super::*;

    // ── plan fidelity stop gate ──────────────────────────────────────
    //
    // Hermetic: classify canned JSON without spawning, and exercise missing
    // process handling separately. Mirrors the merge-gate half (tested in
    // Python); the two readers are independent by design.

    // ── bounded run transport (the single subprocess boundary) ─────────────
    //
    // Hermetic: every child is a stub script in a tempdir, so no test touches
    // a real `gh` or `fno`, and every timing case asserts wall-clock bounds
    // wide enough to survive parallel test scheduling.

    #[test]
    fn fire_budget_clamp_stays_under_remaining_and_above_the_floor() {
        let s = std::time::Duration::from_secs(30);
        // Budget-rich: the configured ceiling stands.
        assert_eq!(
            clamp_to_fire_budget(s, s + std::time::Duration::from_secs(1)),
            s
        );
        // Budget-poor: what remains stands.
        assert_eq!(
            clamp_to_fire_budget(s, std::time::Duration::from_millis(500)),
            std::time::Duration::from_millis(500)
        );
        // Budget spent: every read still gets a positive, killable bound.
        assert_eq!(
            clamp_to_fire_budget(s, std::time::Duration::ZERO),
            STOPGATE_BOUND_FLOOR
        );
    }

    // ── review freshness: the one predicate (/) ───────────────

    fn ident_of(lines: &[&str]) -> CodeDiffIdentity {
        // Any injective stand-in for the blake3 hash keeps equality tests
        // honest: equal line sets -> equal identity, different -> different.
        CodeDiffIdentity {
            hash: lines.join("\n"),
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn facts(reviewed: Option<&str>, head: Option<&str>, tree: Option<&[&str]>) -> FreshnessFacts {
        FreshnessFacts {
            reviewed_identity: reviewed.map(|h| ident_of(&[h])),
            head_identity: head.map(|h| ident_of(&[h])),
            tree_paths: tree.map(|p| p.iter().map(|s| s.to_string()).collect()),
            // Default facts carry no interdiff and a cap of 0, which disables
            // the arm: these tests pin the pre-existing arms exactly.
            ..Default::default()
        }
    }

    fn facts_lines(reviewed: &[&str], head: &[&str], tree: Option<&[&str]>) -> FreshnessFacts {
        FreshnessFacts {
            reviewed_identity: Some(ident_of(reviewed)),
            head_identity: Some(ident_of(head)),
            tree_paths: tree.map(|p| p.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn freshness_same_sha_is_fresh() {
        // No git facts needed at all: the reviewer read this exact commit.
        assert_eq!(
            review_freshness("abc123", "abc123", &FreshnessFacts::default()),
            Freshness::Fresh
        );
    }

    #[test]
    fn freshness_shrunk_diff_carries_as_subset() {
        // The specimen shape: one doc sentence reworded, one test-file hunk
        // gone because main absorbed it. Every raw line still shipping was
        // read, so the shrunk diff carries instead of costing a re-review.
        let f = facts_lines(
            &[
                ":100644 100644 aaa bbb M\tcli/src/a.py",
                ":100644 100644 ccc ddd M\tcli/tests/test_a.py",
            ],
            &[":100644 100644 aaa bbb M\tcli/src/a.py"],
            Some(&["docs/x.md"]),
        );
        let verdict = review_freshness("r1", "h1", &f);
        assert_eq!(verdict, Freshness::CarriedSubset);
        assert!(verdict.counts());
    }

    #[test]
    fn freshness_subset_does_not_swallow_equal_or_added_lines() {
        // Equal sets are not a strict subset (they hash equal and take the
        // tree-paths branch instead), and a HEAD line the reviewer never saw
        // is new unreviewed code: Stale.
        assert_eq!(
            review_freshness(
                "r",
                "h",
                &facts_lines(&["l1", "l2"], &["l1", "l2"], Some(&[]))
            ),
            Freshness::CarriedBaseSync
        );
        assert_eq!(
            review_freshness("r", "h", &facts_lines(&["l1"], &["l1", "l2"], None)),
            Freshness::Stale
        );
    }

    #[test]
    fn freshness_absent_identity_stays_stale() {
        // An identity that failed to compute (None, which is also what an
        // empty code diff yields) can never carry - absence is not evidence.
        assert_eq!(
            review_freshness("r", "h", &facts(None, Some("i"), None)),
            Freshness::Stale
        );
        assert_eq!(
            review_freshness("r", "h", &facts(Some("i"), None, None)),
            Freshness::Stale
        );
    }

    // ── one rounds number across axes (law d-608344c1 /) ─────────────

    fn attest_line(head: &str, branch: &str) -> String {
        format!(
            "{{\"type\":\"review_attestation\",\"data\":{{\"head_sha\":\"{head}\",\"branch\":\"{branch}\",\"reviewer\":\"code-review\",\"verdict\":\"pass\"}}}}"
        )
    }

    // ── both producers go through the one predicate (/) ───────

    /// PR #826's real payload shape: codex submitted at `8e557ccd` while the
    /// gate evaluated against head `89bc0b91`, two commits later.
    pub(super) fn pr826_reviews() -> Vec<Value> {
        vec![serde_json::json!({
            "author": {"login": "chatgpt-codex-connector"},
            "state": "COMMENTED",
            "submittedAt": "2026-08-12T17:51:48Z",
            "commit": {"oid": "8e557ccdecec07abc7e409ad8d888318016612c1"}
        })]
    }

    fn attestation_line(reviewer: &str, head: &str, verdict: &str) -> String {
        serde_json::json!({
            "ts": "2026-01-01T00:00:00Z", "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": reviewer, "head_sha": head, "verdict": verdict,
                     "attester_session_id": "sess-author"}
        })
        .to_string()
    }

    /// Like `attestation_line` but naming the branch the attestation is about -
    /// the scoping field every post-change event carries. A legacy line (no
    /// branch) is admitted only on exact head equality, so tests exercising a
    /// MOVED head must scope by branch or their fixture silently vanishes.
    fn attestation_line_on_branch(
        reviewer: &str,
        head: &str,
        verdict: &str,
        branch: &str,
    ) -> String {
        serde_json::json!({
            "ts": "2026-01-01T00:00:00Z", "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": reviewer, "head_sha": head, "verdict": verdict,
                     "attester_session_id": "sess-author", "branch": branch}
        })
        .to_string()
    }

    #[test]
    fn a_zero_file_pass_does_not_satisfy_the_reviewers_gate() {
        // The reviewers gate (config.review.reviewers, the self-review floor)
        // reads a separate scan; a review of nothing must not satisfy it
        // there either, only on the coverage axis.
        let zero = serde_json::json!({
            "ts": "2026-01-01T00:00:00Z", "source": "test",
            "type": "review_attestation",
            "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass",
                     "attester_session_id": "sess-author",
                     "reviewed_line_count": 0, "reviewed_file_count": 0}
        })
        .to_string();
        let dir = std::env::temp_dir().join(format!(
            "fno-zero-file-scan-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        std::fs::write(&events, zero).unwrap();
        let reviewers = vec!["code-review".to_string()];
        let (unattested, _malformed) =
            unattested_reviewers_scan(&events, &reviewers, &|_| Freshness::Fresh, "", "h", false);
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            !unattested.is_empty(),
            "a zero-file pass must leave the reviewer unattested"
        );
        assert_eq!(unattested[0].name, "code-review");
    }

    #[test]
    fn optional_staleness_never_disqualifies_local_recovery() {
        // The PR-1151 shape: a REQUIRED bot refused, a fresh local attestation
        // at HEAD, and an OPTIONAL bot whose verdict read an older commit.
        // Recovery must hold - an optional bot going stale is not a property
        // any reviewer owes. The stale optional verdict rides in COVERAGE (the
        // union axis), never in the required-only bot sets.
        let events = attestation_line_on_branch("code-review", "h", "pass", "feature/x");
        let rep = classify_coverage(
            &[],
            &[],
            &events,
            &["chatgpt-codex-connector".to_string()],
            true,
            Some("sess-author"),
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        assert_eq!(rep.coverage, Coverage::Covered(1));
        let refused = vec!["some-required-bot".to_string()];
        assert!(local_recovery_from_refusal(&refused, &[], &[], &rep));
    }

    #[test]
    fn required_staleness_still_blocks_local_recovery() {
        // Unchanged, pinned: a stale REQUIRED verdict is one re-read from
        // counting, so recovery must not skip past it.
        let events = attestation_line_on_branch("code-review", "h", "pass", "feature/x");
        let rep = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            Some("sess-author"),
            &|_| Freshness::Fresh,
            "feature/x",
            "h",
        );
        let refused = vec!["some-required-bot".to_string()];
        let stale = vec![("some-required-bot".to_string(), "oldsha".to_string())];
        assert!(!local_recovery_from_refusal(&refused, &[], &stale, &rep));
        // And no refusal means no recovery at all: absence is a wait.
        assert!(!local_recovery_from_refusal(&[], &[], &[], &rep));
    }

    #[test]
    fn required_reviewer_gate_honors_the_same_carry() {
        // The N-reachable-paths check. `config.review.reviewers` is satisfied
        // by a DIFFERENT scan than the coverage count, so a carry granted to
        // one and refused by the other leaves the gate exactly as tight as
        // before and the softening purely decorative.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.jsonl");
        std::fs::write(
            &p,
            attestation_line_on_branch("code-review", "oldhead", "pass", "feature/x") + "\n",
        )
        .unwrap();
        let reviewers = vec!["code-review".to_string()];

        let carried = unattested_reviewers_scan(
            &p,
            &reviewers,
            &|_| Freshness::CarriedBaseSync,
            "feature/x",
            "currenthead",
            false,
        )
        .0;
        assert!(
            carried.is_empty(),
            "a carried attestation must satisfy the gate"
        );

        let stale = unattested_reviewers_scan(
            &p,
            &reviewers,
            &|_| Freshness::Stale,
            "feature/x",
            "currenthead",
            false,
        )
        .0;
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].superseded_head.as_deref(), Some("oldhead"));
    }

    // ── review_coverage reaches BOTH logs, scoped by repo ───────────

    /// The bare sha equality the predicate replaced, as a freshness resolver.
    /// The pre-change tests below run against it unchanged: with no carry ever
    /// granted, the new code must reproduce the old behavior exactly, and any
    /// test that moves is a regression rather than the intended softening.
    fn sha_equality_freshness(head: &str) -> impl Fn(&str) -> Freshness + '_ {
        move |sha: &str| {
            if !sha.is_empty() && sha == head {
                Freshness::Fresh
            } else {
                Freshness::Stale
            }
        }
    }

    #[test]
    fn target_stream_emit_lands_beside_legacy_lock_dirs() {
        // The store commit owns serialization now; a legacy lock or
        // maintenance marker beside the journal neither blocks nor drops a
        // hook emission.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("events.jsonl");
        let global = dir.path().join("global-events.jsonl");
        std::fs::create_dir(dir.path().join("events.jsonl.lock.d")).unwrap();
        std::fs::create_dir(dir.path().join("events.jsonl.gc.d")).unwrap();

        emit_to_both(&project, &global, "mutex_probe", serde_json::json!({}));

        for path in [&project, &global] {
            let text = crate::events::committed_journal_text(path);
            assert!(text.contains("mutex_probe"), "missing in {path:?}");
        }
    }

    #[test]
    fn shadow_transition_accepts_without_emitting_a_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let run_log = dir.path().join("run-log.jsonl");
        let events = dir.path().join("events.jsonl");
        let run_id = "20260823T060900Z-cx73523-e04109";

        observe_shadow_transition(
            &run_log,
            run_id,
            crate::run_state::RunEvent::DispatchClassified,
            &events,
            &events,
        );

        assert_eq!(
            crate::run_state::fold_run_state(&run_log, run_id).unwrap(),
            crate::run_state::RunState::Working
        );
        assert!(!events.exists());
    }

    #[test]
    fn shadow_transition_rejection_changes_no_legacy_decision() {
        let dir = tempfile::tempdir().unwrap();
        let run_log = dir.path().join("run-log.jsonl");
        let events = dir.path().join("events.jsonl");
        let run_id = "20260823T060900Z-cx73523-e04109";
        crate::run_state::append_transition(
            &run_log,
            run_id,
            crate::run_state::RunEvent::DispatchClassified,
        )
        .unwrap();
        crate::run_state::append_transition(
            &run_log,
            run_id,
            crate::run_state::RunEvent::PrepareHandoff,
        )
        .unwrap();
        crate::run_state::append_transition(
            &run_log,
            run_id,
            crate::run_state::RunEvent::SuccessorProven,
        )
        .unwrap();
        let legacy = allow_output("block", None, "keep working", 2, None);

        observe_shadow_transition(
            &run_log,
            run_id,
            crate::run_state::RunEvent::DispatchClassified,
            &events,
            &events,
        );

        assert_eq!(legacy, allow_output("block", None, "keep working", 2, None));
        let telemetry = crate::events::committed_journal_text(&events);
        assert!(telemetry.contains("\"type\":\"transition_rejected\""));
        assert!(telemetry.contains("invalid transition Closed + DispatchClassified"));
    }

    // The spaces-move chokepoint tests live in their own file: this module
    // is shrink-only, and the tests were the code this change touched.
    mod args_tests;
    mod bot_nudge_tests;
    mod bot_verdict_tests;
    mod bounded_run_tests;
    mod ci_checks_tests;
    mod coverage_classify_tests;
    mod coverage_status_tests;
    mod coverage_tests;
    mod findings_tests;
    mod gh_read_tests;
    mod intent_tests;
    mod local_attestation_tests;
    mod posture_tests;
    mod review_coverage_verb_tests;
    mod self_review_floor_tests;
    mod settings_tests;
    mod space_chokepoint_tests;
    use gh_read_tests::shipped_pr;
    #[test]
    fn shadow_observer_rejects_short_run_ids() {
        let dir = tempfile::tempdir().unwrap();
        let run_log = dir.path().join("run-log.jsonl");
        let events = dir.path().join("events.jsonl");

        observe_shadow_transition(
            &run_log,
            "short-run",
            crate::run_state::RunEvent::DispatchClassified,
            &events,
            &events,
        );

        assert!(!run_log.exists());
        assert!(crate::events::committed_journal_text(&events)
            .contains("manifest carries no valid full run id"));
    }

    #[test]
    fn target_stream_emit_lands_during_legacy_maintenance_markers() {
        // The store commit is the acknowledgement; a maintenance marker
        // beside the journal retires no emission.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("events.jsonl");
        std::fs::create_dir(dir.path().join("events.jsonl.lock.d")).unwrap();
        std::fs::create_dir(dir.path().join("events.jsonl.gc.d")).unwrap();

        append_loop_event(&project, "review_coverage", serde_json::json!({}));

        assert!(
            crate::events::committed_journal_text(&project).contains("review_coverage"),
            "review coverage was dropped during expected maintenance"
        );
    }

    #[test]
    fn target_stream_emit_lands_when_legacy_markers_clear_mid_flight() {
        // Markers created and removed around the emission: the store commit
        // is the acknowledgement boundary, so the row lands regardless.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("events.jsonl");
        let lock = dir.path().join("events.jsonl.lock.d");
        let maintenance = dir.path().join("events.jsonl.gc.d");
        std::fs::create_dir(&lock).unwrap();
        std::fs::create_dir(&maintenance).unwrap();

        append_loop_event(&project, "maintenance_handoff_probe", serde_json::json!({}));

        std::fs::remove_dir_all(maintenance).unwrap();
        std::fs::remove_dir_all(lock).unwrap();
        assert!(
            crate::events::committed_journal_text(&project).contains("maintenance_handoff_probe"),
            "the probe row was dropped"
        );
    }

    /// The list half of the scan. Production reads the count too, so this
    /// wrapper lives here rather than as an unused function in the binary.
    /// `head_branch` "" models a legacy fixture (no branch field, admitted on
    /// exact head equality); a multi-head fixture passes the branch it named.
    fn unattested_reviewers(
        events_path: &Path,
        reviewers: &[String],
        head_sha: &str,
        head_branch: &str,
    ) -> Vec<UnattestedReviewer> {
        unattested_reviewers_scan(
            events_path,
            reviewers,
            &sha_equality_freshness(head_sha),
            head_branch,
            head_sha,
            false,
        )
        .0
    }

    /// The gate's boolean view of `unattested_reviewers`, exactly as
    /// `read_pr_info` derives it. The pre-change predicate tests below are
    /// unchanged on purpose: promoting the return value to a list must not
    /// move the gate.
    fn reviewers_all_attested(events_path: &Path, reviewers: &[String], head_sha: &str) -> bool {
        unattested_reviewers(events_path, reviewers, head_sha, "").is_empty()
    }

    // ── streak debounce ─────────────────────────────────────────────
    //
    // These drive `read_prior_fires` with an explicit `now` and gap, so they need
    // no env var and are parallel-safe -- unlike the integration suite, which
    // pins FNO_LOOPCHECK_MIN_FIRE_GAP_SECS=0 process-wide.

    const FP: &str = "FP";
    const NOW: &str = "2026-06-05T12:00:00Z";

    fn at(ts: &str) -> DateTime<Utc> {
        ts.parse().unwrap()
    }

    /// Write a loop_check events log from (ts, fingerprint) pairs, oldest first.
    fn write_fire_log(path: &Path, fires: &[(String, &str)]) {
        let mut out = String::new();
        for (ts, fp) in fires {
            out.push_str(
                &serde_json::json!({
                    "ts": ts, "type": "loop_check", "source": "hook",
                    "data": { "session_id": "sess", "fingerprint": fp },
                })
                .to_string(),
            );
            out.push('\n');
        }
        std::fs::write(path, out).unwrap();
    }

    /// Count the streak over prior fires given as SECONDS BEFORE `now`, oldest
    /// first, all sharing FP. Returns (streak, streak_window_secs).
    fn streak_ago(secs_before_now: &[i64], gap: i64) -> (u64, i64) {
        let now = at(NOW);
        let fires: Vec<(String, &str)> = secs_before_now
            .iter()
            .map(|s| {
                (
                    (now - chrono::Duration::seconds(*s))
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string(),
                    FP,
                )
            })
            .collect();
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("events.jsonl");
        write_fire_log(&p, &fires);
        let (_, streak, _, window) = read_prior_fires(&p, "sess", Some(FP), now, gap);
        (streak, window)
    }

    /// The streak rules. `consecutive_after` is streak + 1, so a streak of 4 is
    /// what trips the attended backstop of 5.
    #[test]
    fn debounce_streak_counting_rules() {
        // (case, prior fires as seconds before now (oldest first), gap, streak, window)
        #[rustfmt::skip]
        let cases: &[(&str, &[i64], i64, u64, i64)] = &[
            // AC1-HP: the triggering shape - four fires inside 60s are ONE
            // observation (the current fire), nowhere near backstop_n.
            ("rapid burst collapses to one observation", &[49, 33, 16, 0], 300, 0, 0),
            // AC2-HP: a genuinely stalled session is still reaped.
            ("fires 6 minutes apart still trip the backstop", &[1440, 1080, 720, 360], 300, 4, 1440),
            // AC3-FR: a skip must NOT advance the cursor. This fire is 330s
            // before `now` but only 270s before the burst's oldest member, so it
            // counts ONLY because the burst left the cursor parked at `now`.
            ("a skip does not advance the cursor", &[330, 60, 30, 10], 300, 1, 330),
            // AC6-FR: gap 0 is byte-identical to the old fire counting, which is
            // what lets the integration suite pin the seam and keep every
            // backstop assertion it already had.
            ("gap 0 restores fire counting exactly", &[49, 33, 16], 0, 3, 49),
            // Clock skew must not invent a debounce from a bad clock.
            ("a fire stamped after `now` counts, not crashes", &[1200, -600], 300, 2, 1200),
            // AC8-REG: the recorded sequence behind the false terminal - session
            // 20260727T203203Z, five fires in 109 seconds with CI still PENDING.
            ("the false-NoProgress incident now blocks", &[109, 93, 76, 17], 300, 0, 0),
        ];
        for (case, fires, gap, want_streak, want_window) in cases {
            let (streak, window) = streak_ago(fires, *gap);
            assert_eq!(streak, *want_streak, "streak: {case}");
            assert_eq!(window, *want_window, "window: {case}");
        }
    }

    /// AC4-CON: progress is never debounced - a CHANGED fingerprint breaks the
    /// streak however fast it arrived.
    #[test]
    fn debounce_changed_fingerprint_breaks_streak_at_any_speed() {
        let now = at(NOW);
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("events.jsonl");
        write_fire_log(
            &p,
            &[
                ("2026-06-05T11:40:00Z".to_string(), FP),
                ("2026-06-05T11:50:00Z".to_string(), FP),
                ("2026-06-05T11:59:58Z".to_string(), "DIFFERENT"),
            ],
        );
        let (_, streak, _, _) = read_prior_fires(&p, "sess", Some(FP), now, 300);
        assert_eq!(streak, 0, "a 2-second-old change still resets the streak");
    }

    /// AC5-ERR: a fire we cannot place in time is transparent - it neither counts
    /// toward nor breaks the streak, and never panics. Failing this way biases
    /// away from an irreversible NoProgress.
    #[test]
    fn debounce_untimestamped_fire_is_transparent() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("events.jsonl");
        let lines = [
            r#"{"ts":"2026-06-05T11:40:00Z","type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
            r#"{"ts":"not-a-timestamp","type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
            r#"{"type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
        ];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();

        let (_, streak, last_fp, _) = read_prior_fires(&p, "sess", Some(FP), at(NOW), 300);
        assert_eq!(
            streak, 1,
            "unplaceable fires skip; the good one still counts"
        );
        assert_eq!(
            last_fp.as_deref(),
            Some(FP),
            "carry-forward still reads the newest recorded fp"
        );
    }

    #[test]
    fn block_reason_pending_ci_is_not_red() {
        // The MUTE_PROBE_N probe runs done() while CI is often still in
        // flight; a Pending conclusion must read as "still running", never
        // as the misleading "CI red ... failed" (observed live on PR #455).
        let pr = PrInfo {
            state: PrState::Open,
            number: 455,
            head_oid: "abc".to_string(),
            ci_conclusion: CiConclusion::Pending,
            mergeable: "UNKNOWN".to_string(),
            ..PrInfo::default()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("still running"),
            "pending CI must not read as red; got: {reason}"
        );
        assert!(!reason.contains("failed"), "got: {reason}");
    }

    #[test]
    fn unknown_coverage_names_the_read_remedy_not_a_ci_failure() {
        let mut pr = watch_pr();
        pr.ci_conclusion = CiConclusion::Success;
        pr.ci_has_pending = false;
        pr.coverage = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Unknown,
            verdicts: vec![],
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("coverage read unavailable"),
            "got: {reason}"
        );
        assert!(reason.contains("retry the review verb"), "got: {reason}");
        assert!(!reason.contains("CI red"), "got: {reason}");
        assert!(!reason.contains("failed"), "got: {reason}");
        assert!(!reason.contains("Read the failing log"), "got: {reason}");
        assert!(!reason.contains("fno do pr logs"), "got: {reason}");
    }

    #[test]
    fn unwatched_async_nudge_ci_pending_teaches_arm_and_tag() {
        // AC3-HP: the CI-pending block message must instruct arming a
        // harness-tracked watcher with a timeout and emitting <watching>,
        // replacing the old "wait silently" prose.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Pending,
            ci_has_pending: true,
            ..watch_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("<watching"), "got: {reason}");
        assert!(reason.contains("timeout"), "got: {reason}");
        // The taught watcher is the sanctioned REST wait verb, never the
        // GraphQL `gh pr checks --watch` and never an inline loop (which a
        // worktree session's Bash isolation refuses as too complex).
        assert!(reason.contains("fno do pr wait"), "got: {reason}");
        assert!(!reason.contains("gh pr checks"), "got: {reason}");
        assert!(!reason.contains("while ["), "got: {reason}");
        assert!(!reason.contains("wait silently"), "got: {reason}");
    }

    #[test]
    fn no_hint_prescribes_the_timeout_binary() {
        // File-wide, so a future hint cannot reintroduce `timeout(1)` at a site
        // this test does not name. The needle is built at runtime so the test
        // does not match its own source.
        let needle = ["timeout", " "].concat();
        for tail in super::production_source().split(&needle).skip(1) {
            assert!(
                !tail.trim_start().starts_with(|c: char| c.is_ascii_digit()),
                "bare timeout invocation: ...{}",
                tail.chars().take(60).collect::<String>()
            );
        }
    }

    /// Names every direct synchronous wait on `gh`/`fno` outside the
    /// centralized runner, given loopcheck source text. Pure over the source so
    /// the ratchet test can drive it with a mutated fixture instead of trusting
    /// a zero-hit scan that never ran.
    ///
    /// A bypass is `Command::new(gh_bin|fno_bin|git_bin)` whose statement (the
    /// next 300 chars, cut at the next `fn ` boundary so one chain cannot
    /// bleed into the next function) calls `.output()`, `.wait()`, or
    /// `.status()` - the three synchronous waits. The transport's own
    /// `.spawn()` chain and the one deliberately detached notifier spawn are
    /// not waits and pass.
    fn direct_wait_bypasses(source: &str) -> Vec<String> {
        let mut hits = Vec::new();
        for marker in [
            "Command::new(gh_bin",
            "Command::new(fno_bin",
            "Command::new(git_bin",
        ] {
            for tail in source.split(marker).skip(1) {
                let stmt: String = tail
                    .chars()
                    .take(300)
                    .collect::<String>()
                    .split("fn ")
                    .next()
                    .unwrap_or("")
                    .to_string();
                let ctx = tail.chars().take(60).collect::<String>();
                for wait in [".output()", ".wait()", ".status()"] {
                    if stmt.contains(wait) {
                        hits.push(format!("{marker} ... {ctx}"));
                        break;
                    }
                }
            }
        }
        hits
    }

    #[test]
    fn the_bypass_detector_catches_an_injected_bypass() {
        // The detector itself is under test: a fixture with ONE new direct
        // wait must be named, proving a green production scan is a scan that
        // ran and matched the right symbol - not an empty haystack.
        let fixture = "fn somewhere() {
    let out = Command::new(gh_bin)
        .args([\"pr\", \"view\"])
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
}
fn run_bounded() {}
bounded_read(); bounded_read();";
        let hits = direct_wait_bypasses(fixture);
        assert_eq!(hits.len(), 1, "exactly the injected bypass: {hits:?}");
        assert!(hits[0].contains("Command::new(gh_bin"), "{hits:?}");
        // The git marker is under test too: a direct git wait is the same
        // hang shape with a different binary, and the detector must name it.
        let git_fixture = "fn g() {
    let out = Command::new(git_bin)
        .args([\"status\"])
        .output()?;
}
fn run_bounded() {}
git_bounded();";
        let git_hits = direct_wait_bypasses(git_fixture);
        assert_eq!(
            git_hits.len(),
            1,
            "exactly the injected git bypass: {git_hits:?}"
        );
        assert!(git_hits[0].contains("Command::new(git_bin"), "{git_hits:?}");
        // And the wait call is what flagged it, not the spawn shape: the same
        // fixture without the wait passes clean.
        let clean = fixture.replace(".output()", ".spawn()");
        assert!(direct_wait_bypasses(&clean).is_empty());
    }

    #[test]
    fn unwatched_async_nudge_missing_review_teaches_arm_and_tag() {
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..watch_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("chatgpt-codex-connector"), "got: {reason}");
        assert!(reason.contains("<watching"), "got: {reason}");
    }

    // ── Watching idle-allow classification ───────────────────────
    /// An open PR whose head matches local HEAD, CI still pending, no findings.
    fn watch_pr() -> PrInfo {
        PrInfo {
            state: PrState::Open,
            number: 404,
            head_oid: "abc".to_string(),
            ci_conclusion: CiConclusion::Pending,
            ci_has_pending: true,
            mergeable: "UNKNOWN".to_string(),
            ..PrInfo::default()
        }
    }

    /// the terminal fires only when the quota bounce is the SOLE unmet
    /// conjunct of `reviewed`. Each case drops one other conjunct and must
    /// block; reverting `awaiting_review_only` to the bare
    /// `!usage_limited.is_empty()` check fails them.
    #[test]
    fn awaiting_review_only_requires_every_other_conjunct() {
        let bounced = || {
            let mut pr = watch_pr();
            pr.coverage = CoverageReport {
                github_approval_satisfies: false,
                coverage: Coverage::Covered(0),
                verdicts: vec![ReviewerVerdict {
                    producer: CoverageProducer::GithubApp,
                    name: "chatgpt-codex-connector".to_string(),
                    verdict: CoverageVerdict::Refused,
                    human_approval: false,
                    author_approval: false,
                    attestation_origin: AttestationOrigin::Unknown,
                    reviewed_sha: String::new(),
                    freshness: None,
                    scope: None,
                    refusal_reason: None,
                    reviewer_context: None,
                    required: true,
                    passed: false,
                }],
            };
            pr
        };
        assert!(awaiting_review_only(&bounced()), "the terminal's own case");
        assert!(!awaiting_review_only(&watch_pr()), "no bounce to report");

        // A bot that has not reviewed YET is owed its nudge window: one bot's
        // quota state must not end the session on the others' behalf.
        let mut still_pending = bounced();
        still_pending.missing_bots = vec!["gemini-code-assist".to_string()];
        assert!(
            !awaiting_review_only(&still_pending),
            "bot still owed a wait"
        );

        // A stale bot owes a re-read: the same window a missing bot
        // gets, never a clean exit on another bot's quota bounce.
        let mut stale_pending = bounced();
        stale_pending.stale_bots = vec![("gemini-code-assist".to_string(), "00001111".to_string())];
        assert!(
            !awaiting_review_only(&stale_pending),
            "stale bot still owed a re-read"
        );

        // A standing blocking finding is work the agent must DO; parking hands
        // a human a PR carrying an unaddressed P1.
        let mut with_finding = bounced();
        with_finding.unaddressed_findings = vec![Finding {
            id: 1,
            author: "gemini-code-assist".to_string(),
            path: "src/lib.rs".to_string(),
            line: 12,
            created_at: "2026-08-06T00:00:00Z".to_string(),
            severity: "P1",
            had_reply: true,
        }];
        assert!(!awaiting_review_only(&with_finding), "unaddressed P1");

        let mut unattested = bounced();
        unattested.unattested_reviewers = vec![UnattestedReviewer {
            name: "sigma".to_string(),
            superseded_head: None,
            failed_at_head: false,
        }];
        assert!(!awaiting_review_only(&unattested), "local review never ran");
    }

    #[test]
    fn watch_idle_classifies_pending_ci() {
        assert_eq!(async_wait_class(&watch_pr(), true, true), Some("ci"));
    }

    #[test]
    fn codex_watch_harness_gate_is_claude_only() {
        // Only Claude self-wakes on a background-task exit, so only Claude idles.
        assert!(harness_can_idle(Some("claude"), false));
        // A loop-run child (FNO_DRIVER_LIB) exits on allow -> never idles.
        assert!(!harness_can_idle(Some("claude"), true));
        // codex/gemini have no self-wake; their daemon-consumer waker ships
        // separately, so until then they keep today's block behavior.
        assert!(!harness_can_idle(Some("codex"), false));
        assert!(!harness_can_idle(Some("gemini"), false));
        // Unknown harness (bare shell / daemon): conservative block.
        assert!(!harness_can_idle(None, false));
    }

    #[test]
    fn watching_refusal_names_the_disqualifying_substrate() {
        assert_eq!(
            watch_lease::watching_harness_refusal(Some("claude"), true),
            "watching ignored: loop-run child cannot idle"
        );
        assert_eq!(
            watch_lease::watching_harness_refusal(Some("codex"), false),
            "watching ignored: harness codex cannot idle"
        );
    }

    #[test]
    fn watch_idle_classifies_awaiting_review() {
        // CI green, no pending checks, and a required GitHub bot has not reviewed.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), Some("review"));
    }

    #[test]
    fn watch_idle_rejects_ci_pending_with_a_failure() {
        // gemini finding: a check has ALREADY concluded red while others run.
        // The agent should debug now, not idle out the remaining pending checks.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Failure(Some("unit".into())),
            ci_has_pending: true,
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), None);
    }

    #[test]
    fn watch_idle_rejects_local_attestation_review_gate() {
        // codex P1: reviewed=false with an EMPTY missing_bots is a local
        // attestation (sigma) or unaddressed-finding gate - no GitHub reviewer
        // will ever post to wake the session, so idling would park it forever.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec![],
            bot_nudges: vec![],
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), None);
    }

    // ── idle rule + message rendering ──────────────────────────────────

    fn bn(login: &str, class: NudgeClass, nudges: usize, newest: i64, span: i64) -> BotNudge {
        BotNudge {
            login: login.into(),
            class,
            review_handle: "@codex review".into(),
            ceiling: 3,
            nudges,
            newest_age_min: newest,
            span_min: span,
        }
    }
    fn bot_review_pr(login: &str, nudges: Vec<BotNudge>) -> PrInfo {
        PrInfo {
            range_tiling: RangeTiling::default(),
            number: 618,
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec![login.into()],
            bot_nudges: nudges,
            ..watch_pr()
        }
    }

    #[test]
    fn nudge_needs_nudge_blocks_and_names_the_command() {
        // AC1: not idlable; reason gives the exact gh command; no arm-and-tag hint.
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn(
                "chatgpt-codex-connector",
                NudgeClass::NeedsNudge,
                0,
                0,
                0,
            )],
        );
        assert_eq!(async_wait_class(&pr, true, true), None);
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("gh pr comment 618 --body \"@codex review\""),
            "{reason}"
        );
        assert!(
            !reason.contains("harness-tracked watcher"),
            "no arm hint: {reason}"
        );
    }

    #[test]
    fn nudge_awaiting_idles_with_the_arm_hint() {
        // AC2: a genuine async wait - idlable, message says nudged + awaiting,
        // and the arm-and-tag ritual is present.
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn("chatgpt-codex-connector", NudgeClass::Awaiting, 1, 3, 3)],
        );
        assert_eq!(async_wait_class(&pr, true, true), Some("review"));
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("nudged"), "{reason}");
        assert!(reason.contains("awaiting"), "{reason}");
        assert!(
            reason.contains("harness-tracked watcher"),
            "arm hint present: {reason}"
        );
    }

    #[test]
    fn nudge_unresponsive_blocks_and_names_optional_apps() {
        // AC3: not idlable; names the give-up + optional_apps; no arm-and-tag hint.
        let pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn(
                "chatgpt-codex-connector",
                NudgeClass::Unresponsive,
                3,
                20,
                47,
            )],
        );
        assert_eq!(async_wait_class(&pr, true, true), None);
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("did not review after 3 nudges over 47m"),
            "{reason}"
        );
        assert!(reason.contains("config.review.optional_apps"), "{reason}");
        assert!(reason.contains("do not arm a watcher"), "{reason}");
        assert!(
            !reason.contains("harness-tracked watcher"),
            "no arm hint: {reason}"
        );
    }

    #[test]
    fn nudge_not_nudgeable_keeps_todays_behavior() {
        // AC5: a non-nudgeable required bot keeps today's string + arm hint and
        // stays idlable, regardless of comment history.
        let pr = bot_review_pr(
            "gemini-code-assist",
            vec![bn("gemini-code-assist", NudgeClass::NotNudgeable, 0, 0, 0)],
        );
        assert_eq!(async_wait_class(&pr, true, true), Some("review"));
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("gemini-code-assist has not reviewed"),
            "{reason}"
        );
        assert!(
            reason.contains("harness-tracked watcher"),
            "arm hint present: {reason}"
        );
    }

    #[test]
    fn nudge_empty_classification_is_status_quo() {
        // A non-empty missing_bots with an EMPTY bot_nudges (not classified)
        // behaves exactly as pre-: idlable, today's string.
        let pr = bot_review_pr("chatgpt-codex-connector", vec![]);
        assert_eq!(async_wait_class(&pr, true, true), Some("review"));
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("has not reviewed"), "{reason}");
    }

    #[test]
    fn stale_bot_needs_nudge_keeps_the_trigger_command() {
        // A stale bot classified NeedsNudge needs the concrete remedy the
        // nudge branch carries (the trigger command, the counters), decorated
        // with what it read - not the generic re-read sentence.
        let mut pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn(
                "chatgpt-codex-connector",
                NudgeClass::NeedsNudge,
                0,
                0,
                0,
            )],
        );
        pr.missing_bots = vec![];
        pr.stale_bots = vec![(
            "chatgpt-codex-connector".to_string(),
            "deadbeef1234".to_string(),
        )];
        assert_eq!(async_wait_class(&pr, true, true), None);
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("gh pr comment 618"), "{reason}");
        assert!(
            reason.contains("(read deadbeef, superseded by this head)"),
            "{reason}"
        );
    }

    #[test]
    fn stale_bot_block_reason_names_the_read_sha_and_a_reread() {
        // AC1: the bot DID respond, so the message must not read as a
        // first read - it names the commit it read and asks for a re-read.
        // Still idles like any nudgeable outstanding bot.
        let mut pr = bot_review_pr(
            "chatgpt-codex-connector",
            vec![bn(
                "chatgpt-codex-connector",
                NudgeClass::NotNudgeable,
                0,
                0,
                0,
            )],
        );
        pr.missing_bots = vec![];
        pr.stale_bots = vec![(
            "chatgpt-codex-connector".to_string(),
            "0daa4d6cea7c".to_string(),
        )];
        assert_eq!(async_wait_class(&pr, true, true), Some("review"));
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("chatgpt-codex-connector (read 0daa4d6c, superseded by this head)"),
            "{reason}"
        );
        assert!(reason.contains("ask for a re-read"), "{reason}");
        assert!(!reason.contains("has not reviewed"), "{reason}");
    }

    #[test]
    fn nudge_message_for_a_stale_bot_appends_what_it_read() {
        // Mixed set: gemini never responded, codex read an older commit. The
        // Awaiting message names codex, so it must carry the read-note suffix
        // rather than read as "never responded".
        let mut pr = bot_review_pr(
            "gemini-code-assist",
            vec![
                bn("gemini-code-assist", NudgeClass::NotNudgeable, 0, 0, 0),
                bn("chatgpt-codex-connector", NudgeClass::Awaiting, 1, 3, 3),
            ],
        );
        pr.stale_bots = vec![(
            "chatgpt-codex-connector".to_string(),
            "111122223333".to_string(),
        )];
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("chatgpt-codex-connector (read 11112222, superseded by this head)"),
            "{reason}"
        );
    }

    #[test]
    fn finding_block_reason_names_the_reply_handle() {
        // AC14: an unaddressed finding by a known bot names the handle a reply
        // must address, not just "reply in-thread".
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "chatgpt-codex-connector".into(),
                path: "a.rs".into(),
                line: 10,
                created_at: "2026-07-06T01:00:00Z".into(),
                severity: "P1",
                had_reply: true,
            }],
            ..watch_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("@chatgpt-codex-connector"), "{reason}");
    }

    /// The exact state PR #618 sat in for ~15 turns: CI green, no required
    /// bots, no unaddressed findings, and a `reviewers: [sigma]` gate with no
    /// head-pinned attestation. `reviewers_ok` was the sole failing term.
    fn reviewers_gate_pr() -> PrInfo {
        PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Success,
            ci_has_pending: false,
            reviewed: false,
            review_skipped: false,
            missing_bots: vec![],
            bot_nudges: vec![],
            unaddressed_findings: vec![],
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: None,
                failed_at_head: false,
            }],
            ..watch_pr()
        }
    }

    #[test]
    fn block_reason_names_the_reviewers_gate_not_a_bot() {
        // AC2: the old string claimed a bot had not reviewed while
        // required_bots was empty and the real blocker was local. sigma is
        // retired, so the unmet reason names the lane, never a panel run.
        let reason = build_block_reason(&reviewers_gate_pr(), "abc", true, true);
        assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
        assert!(reason.contains("sigma"), "got: {reason}");
        assert!(reason.contains("/fno:review"), "got: {reason}");
        assert!(!reason.contains("/fno:review sigma"), "got: {reason}");
        assert!(!reason.contains("bot reviewer"), "got: {reason}");
    }

    #[test]
    fn block_reason_names_the_local_peer_invocation() {
        let mut pr = reviewers_gate_pr();
        pr.unattested_reviewers[0].name = LOCAL_PEER_REVIEWER.to_string();
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("/fno:review peer --attest"),
            "got: {reason}"
        );
        assert!(
            !reason.contains("wait on a GitHub reviewer"),
            "got: {reason}"
        );
    }

    #[test]
    fn block_reason_explains_same_model_local_peer_refusal() {
        let mut pr = reviewers_gate_pr();
        pr.unattested_reviewers[0].name = SAME_MODEL_LOCAL_PEER_SENTINEL.to_string();
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("configure a cross-model peer"),
            "got: {reason}"
        );
        assert!(
            !reason.contains(SAME_MODEL_LOCAL_PEER_SENTINEL),
            "got: {reason}"
        );
    }

    #[test]
    fn block_reason_reviewers_gate_emits_no_idle_ritual() {
        // AC3: async_wait_class already excluded this blocker from idling
        // (watch_idle_rejects_local_attestation_review_gate), so prescribing
        // the arm-and-tag ritual here is the code contradicting itself.
        let pr = reviewers_gate_pr();
        assert_eq!(async_wait_class(&pr, true, true), None);
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(!reason.contains("<watching"), "got: {reason}");
        assert!(
            !reason.contains("Arm a harness-tracked watcher"),
            "got: {reason}"
        );
        assert!(!reason.contains("gh pr checks"), "got: {reason}");
    }

    #[test]
    fn block_reason_names_a_superseded_attestation_head() {
        // A session that ran sigma and then pushed must not read "you never
        // ran sigma"; name the head the pass is pinned to.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: Some("0123456789abcdef".to_string()),
                failed_at_head: false,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("01234567"), "got: {reason}");
        assert!(reason.contains("superseded"), "got: {reason}");
    }

    #[test]
    fn block_reason_generic_review_fallback_has_no_idle_ritual() {
        // The fallback is only reachable with an EMPTY missing_bots, which
        // async_wait_class refuses to idle. It must not teach the ritual either.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unattested_reviewers: vec![],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(!reason.contains("<watching"), "got: {reason}");
        assert!(!reason.contains("bot reviewer"), "got: {reason}");
    }

    #[test]
    fn block_reason_missing_bot_still_teaches_the_ritual() {
        // AC7-adjacent regression: a REAL outstanding GitHub bot, and nothing
        // local outstanding, is a valid async wait and keeps today's message.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            unattested_reviewers: vec![],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("chatgpt-codex-connector"), "got: {reason}");
        assert!(reason.contains("<watching"), "got: {reason}");
    }

    #[test]
    fn block_reason_local_work_outranks_a_bot_wait() {
        // Codex review of this PR: with a bot AND a local reviewer both
        // outstanding, naming only the bot hides the half the session can act
        // on now. Worse, if the bot never posts, the local work never happens
        // and the run dies on budget with the gate still unmet - the #618 shape
        // this node exists to delete.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("reviewers gate unmet"), "got: {reason}");
        assert!(!reason.contains("<watching"), "got: {reason}");
        // Once the local half is attested, the bot wait is the sole blocker and
        // the arm-and-tag message returns.
        let after = PrInfo {
            range_tiling: RangeTiling::default(),
            unattested_reviewers: vec![],
            ..pr
        };
        assert!(build_block_reason(&after, "abc", true, true).contains("<watching"));
    }

    #[test]
    fn reviewers_gate_stays_fail_closed() {
        // AC7: promoting the predicate's return value to a list must not move
        // the gate. Missing file, missing event, stale head, and a `fail`
        // verdict all still leave the reviewer unsatisfied.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("absent.jsonl");
        let sigma = vec!["sigma".to_string()];
        assert!(!unattested_reviewers(&missing, &sigma, "h", "").is_empty());

        let stale = tmp.path().join("stale.jsonl");
        std::fs::write(&stale, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#)).unwrap();
        let out = unattested_reviewers(&stale, &sigma, "NEW", "feature/x");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].superseded_head.as_deref(), Some("OLD"));
        assert!(!out[0].failed_at_head);

        let failed = tmp.path().join("fail.jsonl");
        std::fs::write(&failed, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#)).unwrap();
        let out = unattested_reviewers(&failed, &sigma, "h", "");
        assert_eq!(out.len(), 1);
        // A head-pinned fail is not a superseded pass; do not offer a stale head.
        assert_eq!(out[0].superseded_head, None);
        // ...but it IS an attestation at this head, and the message says so
        // rather than claiming none exists. Pinned at the PARSER: the message
        // test hand-builds the struct and never exercises this derivation, so
        // `failed_at_head: false` survived the whole suite before this line.
        assert!(
            out[0].failed_at_head,
            "a fail at HEAD must be reported as such"
        );
    }

    #[test]
    fn unpinned_attestation_never_counts_as_evidence() {
        // codex P1 on this PR: defaulting a missing head_sha to "" made an
        // unpinned event MATCH a caller whose own head_sha is "", turning
        // no-evidence into a pass.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(
            &p,
            format!(
                "{}\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","verdict":"pass"}}"#
            ),
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "", "");
        assert_eq!(out.len(), 1, "unpinned evidence must not satisfy the gate");
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn a_failed_old_head_is_not_reported_as_superseded() {
        // codex P2: "attested at X, superseded" implies a prior PASS. An
        // old-head fail rendered that way invents a review that never passed.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(&p, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail","branch":"feature/x"}}"#)).unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW", "feature/x");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn a_corrupt_attestation_line_is_counted_and_named() {
        // A torn write leaves an unparseable review_attestation in the file.
        // The gate must still fail closed, but reporting "no head-pinned
        // review_attestation" over a corrupt one is the same class of lie this
        // node deletes. The sibling review_finding scanner already counts its
        // malformed lines; this one did not.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e1.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","hea"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"loop_check","data":{}}"#,
            ),
        )
        .unwrap();
        let (out, malformed) = unattested_reviewers_scan(
            &p,
            &["sigma".to_string()],
            &sha_equality_freshness("h"),
            "",
            "h",
            false,
        );
        assert_eq!(out.len(), 1, "a corrupt line never satisfies the gate");
        assert_eq!(malformed, 1, "and it is counted, not silently dropped");

        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            malformed_attestations: malformed,
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(
            reason.contains("unparseable attestation line"),
            "got: {reason}"
        );

        // A clean file adds nothing to the message.
        let p = tmp.path().join("e2.jsonl");
        std::fs::write(
            &p,
            r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"loop_check","data":{}}"#,
        )
        .unwrap();
        assert_eq!(
            unattested_reviewers_scan(
                &p,
                &["sigma".to_string()],
                &sha_equality_freshness("h"),
                "",
                "h",
                false
            )
            .1,
            0
        );
        assert!(!build_block_reason(&reviewers_gate_pr(), "abc", true, true)
            .contains("unparseable attestation line"));
    }

    #[test]
    fn a_revoked_pass_falls_back_to_an_older_passing_head() {
        // codex P2: `pass A, pass B, fail B` with HEAD C. A single "most recent
        // pass" entry overwrites A with B and then drops B, so the message
        // claims no prior pass while A is still a real one - the misleading
        // guidance this whole node exists to delete, reappearing in exactly the
        // multi-round review/fix cycle that produces this sequence.
        let tmp = tempfile::tempdir().unwrap();
        // One journal per scenario: a rewrite would leak rows across them.
        let p = tmp.path().join("e1.jsonl");
        let line = |head: &str, verdict: &str| {
            format!(
                r#"{{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{{"reviewer":"sigma","head_sha":"{head}","verdict":"{verdict}","branch":"feature/x"}}}}"#
            )
        };
        std::fs::write(
            &p,
            [
                line("AAA", "pass"),
                line("BBB", "pass"),
                line("BBB", "fail"),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC", "feature/x");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].superseded_head.as_deref(),
            Some("AAA"),
            "a still-valid older pass must survive a newer head's retraction"
        );

        // The newest STILL-PASSING head wins when several are valid.
        let p = tmp.path().join("e2.jsonl");
        std::fs::write(
            &p,
            [line("AAA", "pass"), line("BBB", "pass")].join("\n") + "\n",
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC", "feature/x");
        assert_eq!(out[0].superseded_head.as_deref(), Some("BBB"));

        // Every old head retracted -> nothing to name.
        let p = tmp.path().join("e3.jsonl");
        std::fs::write(
            &p,
            [
                line("AAA", "pass"),
                line("BBB", "pass"),
                line("BBB", "fail"),
                line("AAA", "fail"),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "CCC", "feature/x");
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn a_later_fail_revokes_the_superseded_pass_for_that_head() {
        // Append-ordered pass-then-fail on the SAME old head. The pass was
        // recorded as superseded and the fail merely skipped, so the message
        // kept claiming that head was successfully attested after its latest
        // verdict retracted exactly that (codex P2 on this PR).
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e1.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail","branch":"feature/x"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW", "feature/x");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].superseded_head, None,
            "a retracted pass is not evidence"
        );

        // A re-run pass after the fail restores it: revocation is latest-wins,
        // not a one-way latch.
        std::fs::write(
            &tmp.path().join("e2.jsonl"),
            concat!(
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#,
                "\n",
                r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"fail","branch":"feature/x"}}"#,
                "\n",
                // Distinct ts: byte-dedupe would drop an identical row.
                r#"{"ts":"2026-01-01T00:00:01Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass","branch":"feature/x"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let out = unattested_reviewers(
            &tmp.path().join("e2.jsonl"),
            &["sigma".to_string()],
            "NEW",
            "feature/x",
        );
        assert_eq!(out[0].superseded_head.as_deref(), Some("OLD"));
    }

    #[test]
    fn short_sha_never_panics_on_multibyte() {
        // codex P2: `&s[..8]` panics when byte 8 lands inside a character, and
        // superseded_head comes from a user-writable events.jsonl.
        assert_eq!(short_sha("0123456789ab"), "01234567");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
        assert_eq!(short_sha("1234567\u{e9}xyz"), "1234567\u{e9}");
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: Some("1234567\u{e9}abc".to_string()),
                failed_at_head: false,
            }],
            ..reviewers_gate_pr()
        };
        build_block_reason(&pr, "1234567\u{e9}abc", true, true);
    }

    #[test]
    fn watcher_hint_never_contradicts_the_idle_classifier() {
        // codex P1: the missing-bot branch emitted the ritual unconditionally,
        // including for states async_wait_class refuses to idle (an unaddressed
        // finding, or an open operator finding). The hint is now derived from
        // that same classifier, so the two agree by construction.
        let bot_only = PrInfo {
            range_tiling: RangeTiling::default(),
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            unattested_reviewers: vec![],
            ..reviewers_gate_pr()
        };
        for (label, pr, open_empty) in [
            // Reaches the FINDINGS branch, not the bot branch: unaddressed
            // findings render first. Kept because the invariant under test is
            // "no hint for a non-idlable state", which holds branch-wide - but
            // it is the case below that reaches `missing_bots`, so that one is
            // what would catch a reintroduced unconditional `arm_watch_hint`
            // there. A sigma round caught this test silently losing its teeth
            // when the branch order moved out from under it.
            (
                "bot + unaddressed finding (renders as the finding)",
                PrInfo {
                    range_tiling: RangeTiling::default(),
                    missing_bots: vec!["chatgpt-codex-connector".into()],
                    bot_nudges: vec![],
                    unattested_reviewers: vec![],
                    unaddressed_findings: vec![Finding {
                        id: 1,
                        author: "codex".into(),
                        path: "a.rs".into(),
                        line: 1,
                        created_at: "2026-07-27T00:00:00Z".into(),
                        severity: "P1",
                        had_reply: true,
                    }],
                    ..reviewers_gate_pr()
                },
                true,
            ),
            (
                // The one that DOES reach `missing_bots` while non-idlable.
                "bot + open operator finding",
                PrInfo {
                    range_tiling: RangeTiling::default(),
                    missing_bots: vec!["chatgpt-codex-connector".into()],
                    bot_nudges: vec![],
                    unattested_reviewers: vec![],
                    ..reviewers_gate_pr()
                },
                false,
            ),
        ] {
            let reason = build_block_reason(&pr, "abc", open_empty, true);
            assert_eq!(async_wait_class(&pr, open_empty, true), None, "{label}");
            assert!(!reason.contains("<watching"), "{label}: {reason}");
        }
        // The genuinely idlable state keeps the ritual.
        assert_eq!(async_wait_class(&bot_only, true, true), Some("review"));
        assert!(build_block_reason(&bot_only, "abc", true, true).contains("<watching"));
    }

    #[test]
    fn an_unaddressed_finding_is_named_before_the_reviewers_gate() {
        // Sigma review of this PR: addressing an inline finding MOVES HEAD,
        // which supersedes any attestation produced first. Naming the reviewer
        // first would make the session run the panel twice.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "codex".into(),
                path: "a.rs".into(),
                line: 7,
                created_at: "2026-07-27T00:00:00Z".into(),
                severity: "P1",
                had_reply: true,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("unaddressed"), "got: {reason}");
        assert!(!reason.contains("reviewers gate unmet"), "got: {reason}");
        // With the finding cleared, the reviewers gate is what is named.
        let after = PrInfo {
            range_tiling: RangeTiling::default(),
            unaddressed_findings: vec![],
            ..pr
        };
        assert!(build_block_reason(&after, "abc", true, true).contains("reviewers gate unmet"));
    }

    #[test]
    fn unaddressed_finding_with_no_reply_names_the_top_level_blind_spot() {
        // A finding answered with a top-level PR comment reads as unaddressed
        // because the gate walks in_reply_to_id chains only. The block reason
        // must name the mechanism and the exact gh command, not the ambiguous
        // "reply in-thread" a worker who posted a top-level comment reads as
        // "I did reply" (PR #447, #787 both stalled green PRs this way).
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "codex".into(),
                path: "a.rs".into(),
                line: 7,
                created_at: "2026-07-27T00:00:00Z".into(),
                severity: "P1",
                had_reply: false,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("no in-thread reply"), "got: {reason}");
        assert!(reason.contains("in_reply_to_id"), "got: {reason}");
        assert!(reason.contains("top-level PR comment"), "got: {reason}");
        assert!(reason.contains("in_reply_to=<id>"), "got: {reason}");
    }

    #[test]
    fn a_failed_attestation_at_this_head_is_not_reported_as_absent() {
        // "no head-pinned review_attestation" reads as "you never ran it" to a
        // session that ran the reviewer and was told no.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unattested_reviewers: vec![UnattestedReviewer {
                name: "sigma".to_string(),
                superseded_head: None,
                failed_at_head: true,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("verdict NOT pass"), "got: {reason}");
    }

    #[test]
    fn the_stop_gate_marks_declare_as_a_self_cert() {
        // AC5: every surface that prints `declare` says it asserts nothing.
        // The Rust block message is such a surface.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unattested_reviewers: vec![UnattestedReviewer {
                name: "declare".to_string(),
                superseded_head: None,
                failed_at_head: false,
            }],
            ..reviewers_gate_pr()
        };
        let reason = build_block_reason(&pr, "abc", true, true);
        assert!(reason.contains("self-cert"), "got: {reason}");
        assert!(
            reason.contains("asserts no review evidence"),
            "got: {reason}"
        );
        // A real reviewer carries no such mark.
        assert!(!build_block_reason(&reviewers_gate_pr(), "abc", true, true).contains("self-cert"));
    }

    #[test]
    fn an_empty_head_sha_never_becomes_a_superseded_head() {
        // Option<String> cannot say "non-empty", so normalize at construction
        // rather than leaving is_empty() as a convention every reader re-derives.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(&p, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"sigma","head_sha":"","verdict":"pass"}}"#)).unwrap();
        let out = unattested_reviewers(&p, &["sigma".to_string()], "NEW", "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].superseded_head, None);
    }

    #[test]
    fn an_outstanding_local_reviewer_is_never_an_idlable_wait() {
        // The classifier half of the same fix: with a bot AND a local reviewer
        // outstanding, a stray <watching> tag must not park the session on work
        // it could do now.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            missing_bots: vec!["chatgpt-codex-connector".into()],
            bot_nudges: vec![],
            ..reviewers_gate_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), None);
    }

    /// A degraded `gh pr view` can return an open PR with no `headRefOid`.
    /// `head_is_shipped` answers false for that, so a bare `!head_shipped`
    /// would render the push message with a blank sha and hide the real
    /// blocker. The `is_empty` guard in front of it is what prevents that.
    #[test]
    fn block_reason_never_demands_a_push_for_a_pr_with_no_recorded_head() {
        let pr = shipped_pr("", PrState::Open);
        let reason = build_block_reason(&pr, "fe407c3b", true, false);
        assert!(
            !reason.contains("push the latest commits"),
            "an empty recorded head must fall through to the real blocker: {reason}"
        );
        assert!(!reason.is_empty(), "a reason is still rendered: {reason}");
    }

    #[test]
    fn block_reason_stops_demanding_a_push_for_a_shipped_head() {
        // The wedge, end to end. Positive marker AND absence: a panicking
        // renderer would also fail to print the push message.
        let pr = shipped_pr("23480a0e", PrState::Merged);
        let reason = build_block_reason(&pr, "fe407c3b", true, true);
        assert!(
            !reason.contains("push the latest commits"),
            "shipped head must not be told to push: {reason}"
        );
        assert!(!reason.is_empty(), "a reason is still rendered: {reason}");
    }

    #[test]
    fn block_reason_still_demands_a_push_for_unshipped_work() {
        // The half that proves the fix did not just delete the guard.
        let pr = shipped_pr("23480a0e", PrState::Open);
        let reason = build_block_reason(&pr, "deadbeef", true, false);
        assert!(
            reason.contains("push the latest commits before completing"),
            "unpushed work must still block: {reason}"
        );
    }

    /// git stub whose origin/main diff is EMPTY (the any-directory case: the
    /// fire sits where the branch diff says nothing) and gh stub serving a PR.
    fn floor_payload_stubs(dir: &Path, files_json: &str) -> (PathBuf, PathBuf) {
        let git = write_exec(
            dir,
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  rev-parse*origin/main*) printf 'sha\\n' ;;\n  *origin/main*) printf '' ;;\n  *) exit 1 ;;\nesac\n",
        );
        let gh = write_exec(
            dir,
            "gh",
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'gh version 2.x' ;;\n  *view*) echo '{{\"number\":7}}' ;;\n  *files*) printf '{files_json}' ;;\n  *) exit 1 ;;\nesac\n"
            ),
        );
        (git, gh)
    }

    #[test]
    fn self_review_gate_pass_attestation_clears_code_review() {
        // AC2-HP: once a head-pinned code-review pass lands, the scan no longer
        // holds it - the floor is satisfiable by the self-serve route, not a wait.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("e.jsonl");
        std::fs::write(&p, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"code-review","head_sha":"h","verdict":"pass"}}"#)).unwrap();
        let out = unattested_reviewers(&p, &["code-review".to_string()], "h", "");
        assert!(
            out.is_empty(),
            "code-review should clear on a pass: {out:?}"
        );
    }

    fn write_exec(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    // ── gh probe: spawn trouble is never absence ─────────────────────────────

    /// One stub gh for the pr_num > 0 failure-arm tests: `pr view` fails with a
    /// rate-limit stderr (NOT the "no pull requests found" no-PR wording, which
    /// would take the Ok(PrState::None) branch), `api rate_limit` answers the
    /// given buckets. `core_remaining` Some(0) keeps the refusal explainable
    /// by the primary quota (not classified secondary) when the test wants
    /// the graphql-diagnosis arm.
    fn write_failing_pr_view_gh(
        dir: &Path,
        graphql_remaining: i64,
        reset_in_secs: i64,
        core_remaining: i64,
    ) -> String {
        let reset = Utc::now().timestamp() + reset_in_secs;
        write_exec(
            dir,
            "gh",
            &format!(
                "#!/bin/sh\n\
                 [ \"$1\" = pr ] && [ \"$2\" = view ] && \
                 echo 'GraphQL: API rate limit exceeded for user ID 1.' >&2 && exit 1\n\
                 [ \"$1\" = api ] && [ \"$2\" = rate_limit ] && \
                 echo '{{\"resources\":{{\"graphql\":{{\"remaining\":{remaining},\"reset\":{reset}}},\
                 \"core\":{{\"remaining\":{core},\"limit\":5000,\"reset\":{reset}}}}}}}' && exit 0\n\
                 exit 1\n",
                remaining = graphql_remaining,
                reset = reset,
                core = core_remaining,
            ),
        )
        .to_str()
        .unwrap()
        .to_string()
    }

    /// The review-coverage argv the quota tests share, head sha apart.
    fn review_coverage_args(tmp: &std::path::Path, head: &str, gh: &str) -> Vec<String> {
        [
            "review-coverage",
            "--cwd",
            tmp.to_str().unwrap(),
            "--pr",
            "865",
            "--head",
            head,
            "--events",
            tmp.join("ev.jsonl").to_str().unwrap(),
            "--global-events",
            tmp.join("gev.jsonl").to_str().unwrap(),
            "--settings",
            tmp.join("absent.toml").to_str().unwrap(),
            "--gh-bin",
            gh,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Verbatim as measured 2026-08-24T01:01:17Z during a live secondary
    /// refusal: GitHub's own wording contains NO "secondary" - that absence is
    /// the premise the live-bucket classifier exists for. The `...` gaps are
    /// where the live capture was truncated, not paraphrase.
    const VERBATIM_403: &str = "gh: API rate limit exceeded for user ID 4994564. If you reach \
         out to GitHub Support for help, please include the request ID \
         FAEB:283161:6EF36:99B72:6A8B97DD ... Terms of Service (...) (HTTP 403)";

    #[test]
    fn unwatched_async_nudge_review_uses_review_aware_watcher() {
        // codex P2: the review-wait watcher must poll REVIEW state, not
        // checks. It must also poll on REST - the GraphQL
        // reviews read is part of what exhausts the shared quota. The recipes
        // are the sanctioned `fno do pr wait` verb, one plain command: an
        // inline `while`/`$(...)` loop is refused by Claude Code's worktree
        // Bash isolation, so a worktree session cannot arm the watcher at all.
        let hint = arm_watch_hint(404, "review");
        assert!(
            hint.contains("fno do pr wait 404 --until review"),
            "got: {hint}"
        );
        assert!(!hint.contains("gh pr view"), "got: {hint}");
        assert!(!hint.contains("while ["), "got: {hint}");
        assert!(!hint.contains("$("), "got: {hint}");
        // The CI-wait watcher polls the REST status chokepoint for the
        // POSITIVE settled marker, never `gh pr checks --watch` (GraphQL).
        let ci_hint = arm_watch_hint(404, "ci");
        assert!(
            ci_hint.contains("fno do pr wait 404 --until settled"),
            "got: {ci_hint}"
        );
        assert!(!ci_hint.contains("gh pr checks"), "got: {ci_hint}");
        assert!(!ci_hint.contains("while ["), "got: {ci_hint}");
        assert!(!ci_hint.contains("$("), "got: {ci_hint}");
    }

    #[test]
    fn watch_idle_rejects_unshipped_work() {
        // AC2-ERR: unpushed work is never async-wait. The predicate used to be
        // `PR head != local HEAD` and is now "not shipped", which is the same
        // verdict for a genuine unpushed commit and a different one for a head
        // that merely moved past its own merge.
        assert_eq!(async_wait_class(&watch_pr(), true, false), None);
    }

    #[test]
    fn watch_idle_rejects_ci_red() {
        // AC1-ERR: settled-red CI (no pending) blocks, never idles.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            ci_conclusion: CiConclusion::Failure(Some("unit".into())),
            ci_has_pending: false,
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), None);
    }

    #[test]
    fn watch_idle_rejects_unaddressed_finding() {
        // AC2-ERR: an unaddressed blocking inline finding is not async-wait.
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            unaddressed_findings: vec![Finding {
                id: 1,
                author: "codex".into(),
                path: "a.rs".into(),
                line: 1,
                created_at: "none".into(),
                severity: "P1",
                had_reply: true,
            }],
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), None);
    }

    #[test]
    fn watch_idle_rejects_open_operator_finding() {
        // An open operator review_finding for the node also blocks idling.
        assert_eq!(async_wait_class(&watch_pr(), false, true), None);
    }

    #[test]
    fn watch_idle_rejects_non_open_pr() {
        // A merged/closed PR is not an async wait (green+merged is DonePRGreen).
        let pr = PrInfo {
            range_tiling: RangeTiling::default(),
            state: PrState::Merged,
            ..watch_pr()
        };
        assert_eq!(async_wait_class(&pr, true, true), None);
    }

    #[test]
    fn watch_idle_window_defaults_clamps_and_slacks() {
        // Default (no tag timeout): 30m + 12m slack.
        assert_eq!(
            watch_window_ms(None),
            30 * 60_000 + watch_lease::WATCH_SLACK_MS
        );
        // Honored within range.
        assert_eq!(
            watch_window_ms(Some("30m")),
            30 * 60_000 + watch_lease::WATCH_SLACK_MS
        );
        // Below the 5m floor clamps up.
        assert_eq!(
            watch_window_ms(Some("1m")),
            5 * 60_000 + watch_lease::WATCH_SLACK_MS
        );
        // Above the 2h ceiling clamps down.
        assert_eq!(
            watch_window_ms(Some("5h")),
            2 * 3_600_000 + watch_lease::WATCH_SLACK_MS
        );
        // Garbage falls back to the default.
        assert_eq!(
            watch_window_ms(Some("soon")),
            30 * 60_000 + watch_lease::WATCH_SLACK_MS
        );
    }

    #[test]
    fn fingerprint_format() {
        let fp = make_fingerprint("sha123", "OPEN", "SUCCESS", "2026-06-05T01:00:00Z");
        assert_eq!(fp, "sha123|OPEN|SUCCESS|2026-06-05T01:00:00Z");
    }

    // ── DoneAwaitingMerge classifier ───────────────────────────────────────

    // ── cancel is an absent result, not a terminal one ─────────────────────

    /// AC5-HP: enums parse known gh strings.
    #[test]
    fn pr_state_parses_known_gh_strings() {
        assert_eq!(PrState::from_gh_str("OPEN"), PrState::Open);
        assert_eq!(PrState::from_gh_str("MERGED"), PrState::Merged);
        assert_eq!(PrState::from_gh_str("CLOSED"), PrState::Closed);
        assert_eq!(PrState::from_gh_str("none"), PrState::None);
    }

    /// AC5-EDGE: an unexpected gh state string maps to PrState::None
    /// (fail-closed), never panics.
    #[test]
    fn pr_state_unknown_string_fails_closed() {
        assert_eq!(PrState::from_gh_str("DRAFT"), PrState::None);
        assert_eq!(PrState::from_gh_str(""), PrState::None);
        assert_eq!(PrState::from_gh_str("open"), PrState::None);
    }

    /// AC5-UI: as_str/render reproduce the exact legacy fingerprint vocabulary.
    #[test]
    fn enum_rendering_byte_identical_to_legacy_strings() {
        assert_eq!(PrState::Open.as_str(), "OPEN");
        assert_eq!(PrState::Merged.as_str(), "MERGED");
        assert_eq!(PrState::Closed.as_str(), "CLOSED");
        assert_eq!(PrState::None.as_str(), "none");
        assert_eq!(CiConclusion::Success.render(), "SUCCESS");
        assert_eq!(
            CiConclusion::Failure(Some("lint".into())).render(),
            "FAILURE:lint"
        );
        assert_eq!(CiConclusion::Failure(None).render(), "FAILURE");
        assert_eq!(CiConclusion::Pending.render(), "PENDING");
        assert_eq!(CiConclusion::Skipped.render(), "skipped");
        assert_eq!(CiConclusion::None.render(), "none");
    }

    #[test]
    fn watch_idle_event_is_non_terminal_allow() {
        // AC1-HP invariant: the idle branch emits allow + null termination, so
        // the stop-hook shim (which runs finalize only on a NON-null
        // termination_reason) never invokes finalize / stamps the ledger /
        // graduates a plan on an idle fire. This is the exact output shape the
        // idle branch returns.
        let json = allow_output(
            "allow",
            None,
            "watching: idling until watcher fires (PR #404, ci pending)",
            3,
            Some("sha|OPEN|PENDING|none".to_string()),
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["decision"], "allow");
        assert!(
            v["termination_reason"].is_null(),
            "idle-allow MUST be non-terminal or finalize would run"
        );
        assert!(v["message"].as_str().unwrap().contains("watching"));
    }

    #[test]
    fn termination_reason_variant_names_byte_identical() {
        // Fix 6: all TerminationReason variants must serialize to the exact strings
        // the spec names - no rename attributes applied.
        let cases = [
            (TerminationReason::DonePRGreen, "DonePRGreen"),
            (TerminationReason::DoneAdvisory, "DoneAdvisory"),
            (TerminationReason::DoneAwaitingReview, "DoneAwaitingReview"),
            (TerminationReason::NoWork, "NoWork"),
            (TerminationReason::Budget, "Budget"),
            (TerminationReason::NoProgress, "NoProgress"),
            (TerminationReason::Interrupted, "Interrupted"),
            (TerminationReason::Aborted, "Aborted"),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            // serde serializes enum unit variants as "\"VariantName\""
            assert_eq!(
                json,
                format!("\"{expected}\""),
                "variant {expected} serialized incorrectly"
            );
        }
    }

    // ── step 2: required_bots parsing + resolution (US1/US3) ────────────────

    // --- github_apps rename + required_bots alias (US3/US4) ---

    // --- optional_apps: honored-if-present, never required ---

    // --- reviewers: local-attestation gate (Phase 2) ---

    fn write_events(dir: &Path, lines: &[&str]) -> std::path::PathBuf {
        let p = dir.join("events.jsonl");
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    #[test]
    fn reviewers_all_attested_empty_is_vacuously_true() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("nonexistent.jsonl");
        assert!(reviewers_all_attested(&p, &[], "abc"));
    }

    #[test]
    fn reviewers_all_attested_head_pinned_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"abc123","verdict":"pass"}}"#,
            ],
        );
        assert!(reviewers_all_attested(&p, &["sigma".to_string()], "abc123"));
    }

    #[test]
    fn reviewers_all_attested_stale_head_is_unsatisfied() {
        // Head-pin: a pass for a PRIOR commit must not satisfy the current HEAD
        // (AC1-EDGE / AC8-HP). A new commit invalidates the old attestation.
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"OLD","verdict":"pass"}}"#,
            ],
        );
        assert!(!reviewers_all_attested(&p, &["sigma".to_string()], "NEW"));
    }

    #[test]
    fn reviewers_all_attested_fail_and_missing_are_unsatisfied() {
        let tmp = tempfile::tempdir().unwrap();
        // fail verdict -> unsatisfied
        let fail = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
            ],
        );
        assert!(!reviewers_all_attested(&fail, &["sigma".to_string()], "h"));
        // missing file -> fail closed
        let gone = tmp.path().join("gone.jsonl");
        assert!(!reviewers_all_attested(&gone, &["sigma".to_string()], "h"));
    }

    #[test]
    fn reviewers_all_attested_conjunction_and_slash_normalized() {
        // Every reviewer must be attested (strict conjunction); a '/'-prefixed
        // config entry matches an event that emits the bare name and vice-versa.
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
                r#"{"ts":"t","type":"review_attestation","source":"target","data":{"reviewer":"code-review","head_sha":"h","verdict":"pass"}}"#,
            ],
        );
        // Both present -> satisfied ('/code-review' config vs 'code-review' event).
        assert!(reviewers_all_attested(
            &p,
            &["sigma".to_string(), "/code-review".to_string()],
            "h"
        ));
        // One missing -> unsatisfied.
        assert!(!reviewers_all_attested(
            &p,
            &["sigma".to_string(), "declare".to_string()],
            "h"
        ));
    }

    #[test]
    fn reviewers_all_attested_latest_verdict_wins() {
        // events.jsonl is append-ordered: a later attestation supersedes an
        // earlier one for the same reviewer at the same head (codex peer P1).
        let tmp = tempfile::tempdir().unwrap();
        // pass THEN fail -> latest is fail -> unsatisfied.
        let pf = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
                r#"{"ts":"t2","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
            ],
        );
        assert!(
            !reviewers_all_attested(&pf, &["sigma".to_string()], "h"),
            "a fail posted after a pass must revoke it"
        );
        // fail THEN pass -> latest is pass -> satisfied (re-review cleared it).
        let fp = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"fail"}}"#,
                r#"{"ts":"t2","type":"review_attestation","source":"target","data":{"reviewer":"sigma","head_sha":"h","verdict":"pass"}}"#,
            ],
        );
        assert!(
            reviewers_all_attested(&fp, &["sigma".to_string()], "h"),
            "a pass posted after a fail must restore satisfaction"
        );
    }

    // ── operator review-finding gate ────────────────────────────────

    #[test]
    fn review_finding_open_then_resolved_clears() {
        // AC2-HP: an open review_finding gates; an explicit resolve clears it.
        let tmp = tempfile::tempdir().unwrap();
        let open = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-1","text":"off-by-one in the loop\nsecond line"}}"#,
            ],
        );
        let (findings, malformed) = open_review_findings(&open, "x-1");
        assert_eq!(malformed, 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "f1");
        assert_eq!(findings[0].first_line, "off-by-one in the loop"); // first line only

        // resolve clears it (node-scoped, only an explicit resolve).
        let resolved = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t1","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-1","text":"off-by-one"}}"#,
                r#"{"ts":"t2","type":"review_finding_resolved","source":"observer","data":{"finding_id":"f1"}}"#,
            ],
        );
        assert!(open_review_findings(&resolved, "x-1").0.is_empty());
    }

    #[test]
    fn review_finding_is_node_scoped() {
        // A finding for a different node must not gate this node.
        let tmp = tempfile::tempdir().unwrap();
        let p = write_events(
            tmp.path(),
            &[
                r#"{"ts":"t","type":"review_finding","source":"observer","data":{"finding_id":"f1","node":"x-OTHER","text":"not mine"}}"#,
            ],
        );
        assert!(open_review_findings(&p, "x-mine").0.is_empty());
        assert_eq!(open_review_findings(&p, "x-OTHER").0.len(), 1);
    }

    #[test]
    fn review_finding_malformed_notices_not_blocks() {
        // AC3-FR: a structurally-unparseable review_finding line does NOT block
        // (no open finding), but is counted for the audit notice. A review_finding
        // missing its id is likewise a malformed notice, never a gating finding.
        let tmp = tempfile::tempdir().unwrap();
        // A truncated (unparseable) line that still carries the review_finding marker.
        let truncated = r#"{"ts":"t","type":"review_finding","data":{"finding_id":"f1"#;
        let id_less = r#"{"ts":"t","type":"review_finding","source":"observer","data":{"node":"x-1","text":"no id"}}"#;
        let good = r#"{"ts":"t","type":"review_finding","source":"observer","data":{"finding_id":"good","node":"x-1","text":"real one"}}"#;
        let p = write_events(tmp.path(), &[truncated, id_less, good]);
        let (findings, malformed) = open_review_findings(&p, "x-1");
        assert_eq!(findings.len(), 1, "only the well-formed finding gates");
        assert_eq!(findings[0].id, "good");
        assert_eq!(
            malformed, 2,
            "the truncated line + the id-less line are noticed"
        );
    }

    #[test]
    fn review_finding_block_reason_quotes_first_plus_count() {
        let open = vec![
            OpenFinding {
                id: "aaa".into(),
                first_line: "the bug".into(),
            },
            OpenFinding {
                id: "bbb".into(),
                first_line: "another".into(),
            },
        ];
        let r = build_findings_block_reason(&open, 1);
        assert!(r.contains("aaa"));
        assert!(r.contains("the bug"));
        assert!(r.contains("fno backlog annotate resolve aaa"));
        assert!(r.contains("[+1 more]"));
        assert!(r.contains("1 malformed"));
    }

    // --- peers -> gate union (US4) ---

    #[test]
    fn local_peer_attestation_is_head_pinned() {
        let td = tempfile::tempdir().unwrap();
        let events = td.path().join("events.jsonl");
        std::fs::write(&events, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"peer","head_sha":"OLD","verdict":"pass"}}"#)).unwrap();
        let peer = vec![LOCAL_PEER_REVIEWER.to_string()];
        assert!(!reviewers_all_attested(&events, &peer, "NEW"));
        std::fs::write(&events, format!("{}\n", r#"{"ts":"2026-01-01T00:00:00Z","source":"test","type":"review_attestation","data":{"reviewer":"peer","head_sha":"NEW","verdict":"pass"}}"#)).unwrap();
        assert!(reviewers_all_attested(&events, &peer, "NEW"));
    }

    // ---- same-model peer guard -----------------------------------

    // ── clean-pass comments satisfy the required-bot gate too (P1) ────
    //
    // The coverage axis and the presence gate must agree about the same
    // comment. Before this, a clean-pass comment marked coverage reviewed
    // while missing_bots kept naming the bot, so the PR waited forever on a
    // reviewer that had already passed it.

    fn clean_pass_comment(head: &str) -> Value {
        serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": format!(
                "Codex Review: Didn't find any major issues. Bravo. Reviewed commit: {head}"
            ),
            "createdAt": "2026-08-17T02:00:00Z"
        })
    }

    // ── one predicate, both axes (PR 917 dual review) ────────────────────────
    //
    // bot_verdict is the single per-bot predicate behind BOTH the coverage
    // axis and the presence gate; these are the shapes where the two
    // previously answered differently about the same payload.

    fn usage_comment(at: &str) -> Value {
        serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": "You have reached your Codex usage limits for code reviews",
            "createdAt": at
        })
    }

    fn authentication_failure_comment() -> Value {
        serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "body": "Authentication failed while starting this code review",
            "createdAt": "2026-08-17T03:00:00Z"
        })
    }

    fn stale_findings_review() -> Value {
        serde_json::json!({
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "state": "COMMENTED",
            "commit": {"oid": "0000000000"},
            "submittedAt": "2026-08-17T01:00:00Z"
        })
    }

    fn fresh_at_head() -> impl Fn(&str) -> Freshness {
        |sha: &str| {
            if sha == "abc12345" {
                Freshness::Fresh
            } else {
                Freshness::Stale
            }
        }
    }

    #[test]
    fn optional_only_refusal_is_not_awaiting_review() {
        // The stop gate must not end the session DoneAwaitingReview over a
        // refusal nobody was owed: on a repo with no required bots that
        // terminal was trivially reachable, and the message told the worker
        // to wait for a reviewer that will never come. The gate blocks, so
        // the unattested local reviewer reads as the real work it is.
        let owed = |required| ReviewerVerdict {
            producer: CoverageProducer::GithubApp,
            name: "chatgpt-codex-connector".to_string(),
            verdict: CoverageVerdict::Refused,
            human_approval: false,
            author_approval: false,
            attestation_origin: AttestationOrigin::Unknown,
            reviewed_sha: String::new(),
            freshness: None,
            scope: None,
            refusal_reason: None,
            reviewer_context: None,
            required,
            passed: false,
        };
        let mut pr = watch_pr();
        pr.coverage = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Covered(0),
            verdicts: vec![owed(false)],
        };
        assert!(!awaiting_review_only(&pr));
        // The same refusal OWED is still the terminal's case - the pre-change
        // semantics, kept: a required bot declined and nothing else is unmet.
        pr.coverage.verdicts[0].required = true;
        assert!(awaiting_review_only(&pr));
    }

    // ── round two: the fix round's own review findings ───────────────────────

    // ── round three: the second fix round's own review findings ─────────────

    // ── nudge state ────────────────────────────────────────────────────

    fn nudge_now() -> DateTime<Utc> {
        "2026-07-06T02:00:00Z".parse().unwrap()
    }
    fn mention(body: &str, created: &str) -> Value {
        serde_json::json!({"body": body, "createdAt": created})
    }

    // ── step 2: inline findings + severity + addressed (US2) ────────────────

    fn finding_comment(id: i64, body: &str, created_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "in_reply_to_id": null,
            "user": {"login": "chatgpt-codex-connector[bot]"},
            "body": body,
            "path": "src/x.rs",
            "line": 42,
            "created_at": created_at
        })
    }

    fn reply_comment(id: i64, parent: i64, login: &str, body: &str, created_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "in_reply_to_id": parent,
            "user": {"login": login},
            "body": body,
            "created_at": created_at
        })
    }

    const REQ: &[&str] = &["chatgpt-codex-connector"];

    fn req_vec() -> Vec<String> {
        REQ.iter().map(|s| s.to_string()).collect()
    }

    // ── step 2: outage vs no-PR discrimination (US4) ─────────────────────────
}
#[cfg(test)]
#[path = "posture_self_lane_tests.rs"]
mod posture_self_lane_tests;
