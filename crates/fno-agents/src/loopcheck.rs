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
mod fire_history;
mod gh_read;
mod intent;
mod local_attestation;
mod plan_fidelity;
mod posture;
mod pr_read;
mod review_coverage_verb;
mod review_findings;
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
use fire_history::{
    append_loop_event, make_fingerprint, min_fire_gap_secs, read_last_row_fields, read_prior_fires,
};
pub(crate) use fire_history::{emit_to_both, now_rfc3339_utc, observe_shadow_transition};
#[cfg(test)]
use gh_read::is_no_pr_stderr;
use gh_read::{
    attestation_in_scope, git_head_branch, git_head_sha, graphql_exhausted_reason, head_is_shipped,
    internal_gh_adapter, pr_head_oid, probe_graphql_quota, read_pr_head_oid, read_pr_view,
    refusal_is_secondary, stderr_smells_rate_limit, stderr_tail, GraphqlQuota,
};
pub(crate) use gh_read::{coverage_adapter, is_graphql_read};
pub(crate) use intent::parse_xml_attr;
use intent::{detect_intent, extract_last_assistant_message, Intent};
#[cfg(test)]
use intent::{detect_intent_from_text, INTENT_LOOKBACK_ENTRIES};
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
pub(crate) use pr_read::CiConclusion;
use pr_read::{read_pr_info, PrInfo, PrState};
pub use review_coverage_verb::{run_review_coverage, run_review_coverage_capture};
pub(crate) use review_findings::event_lines;
#[cfg(test)]
use review_findings::OpenFinding;
use review_findings::{
    build_findings_block_reason, demote_unmeasured_coverage, open_findings_from_store,
    review_journal_text,
};
pub use review_findings::{unattested_reviewers_scan, UnattestedReviewer};
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
use watch_lease::{harness_can_idle, watch_target, watch_window_ms, CONTINUE_WORKING};

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
            let (blocker, pr_number) = watch_target(reason, pr.as_deref());
            emit(
                "loop_check_watch_idle",
                serde_json::json!({
                    "session_id": session_id,
                    "pr": pr_number,
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
    // Findings live in the store, not a rotating journal: the reader names a
    // read error instead of reading it as zero (AC5 - could-not-read is not
    // zero).
    let (open_findings, findings_read_error) = match node_id.as_deref() {
        Some(n) => open_findings_from_store(&crate::graph_get::default_graph_path(), n),
        None => (Vec::new(), None),
    };
    if let Some(error) = &findings_read_error {
        emit(
            "loop_check_finding_store_error",
            serde_json::json!({
                "session_id": session_id,
                "node": node_id,
                "error": error
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
        if (!open_findings.is_empty() || findings_read_error.is_some())
            && !backstop_tripped
            && (intent == Intent::Promise || consecutive_after >= MUTE_PROBE_N)
        {
            let reason = match &findings_read_error {
                Some(error) => format!(
                    "finding store unreadable for {}: {error} - the gate refuses to read it as zero",
                    node_id.as_deref().unwrap_or("?")
                ),
                None => build_findings_block_reason(&open_findings),
            };
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
                    "finding_store_error": findings_read_error
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
                let observed_async_wait = async_wait_class(
                    &pr_info,
                    open_findings.is_empty() && findings_read_error.is_none(),
                    head_shipped,
                );

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
                            open_findings.is_empty() && findings_read_error.is_none(),
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
    let continue_msg = crate::nudge::append_inbox_nudge(CONTINUE_WORKING, &cwd, &session_id);
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
    mod decide_tests;
    mod findings_tests;
    mod fire_history_tests;
    mod gh_read_tests;
    mod intent_tests;
    mod local_attestation_tests;
    mod plan_fidelity_tests;
    mod posture_tests;
    mod pr_read_tests;
    mod review_coverage_verb_tests;
    mod review_findings_tests;
    mod self_review_floor_tests;
    mod settings_tests;
    mod space_chokepoint_tests;
    use gh_read_tests::shipped_pr;

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

    use crate::write_exec_stub as write_exec;

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

    // --- reviewers: local-attestation gate (Phase 2) ---

    fn write_events(dir: &Path, lines: &[&str]) -> std::path::PathBuf {
        let p = dir.join("events.jsonl");
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

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
}
#[cfg(test)]
#[path = "posture_self_lane_tests.rs"]
mod posture_self_lane_tests;
