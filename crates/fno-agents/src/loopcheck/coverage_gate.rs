//! The merge guard's review-coverage predicate, native.
//!
//! One callable answers "is this PR head covered by review, so it may
//! merge?" for every surface: the merge decision reads it in process, the
//! `check` operand prints it, the `publish` operand stamps it onto GitHub,
//! and the `waive` operand records the operator exit beside it. The
//! composition calls the owners that already live in this crate: the row
//! reader in `pr_status::reviews`, the disposition pass in
//! `disposition_gate`, the round counter in `local_attestation`, the
//! waiver overlay in `coverage_status`, and the lane predicate beside
//! them. It writes no second copy of any of them.
//!
//! The order of the steps below is the fail-closed design, kept from the
//! Python gate it replaces: refuse an unreadable cwd before any probe can
//! misattribute it, short-circuit a lane-less install before the head
//! fetch, consult the override valve before the events read, and answer
//! UNANSWERED whenever an instrument died - an empty read is an answer,
//! a dead probe is not.

use super::*;
use crate::authorized_merge::Probes;
use crate::pr_status::reviews::{read_review_coverage, review_lane};
use serde_json::{json, Value};
use std::io::Read;

/// Exit polarity of the three-state answer, kept from the verb's own
/// contract: 0 covered, 3 refused, 4 unanswered. Pub because the verdict
/// rides the pub `Probes::coverage_verdict` surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Covered,
    Refused,
    Unanswered,
}

impl GateState {
    pub(crate) fn exit(self) -> i32 {
        match self {
            GateState::Covered => 0,
            GateState::Refused => 3,
            GateState::Unanswered => 4,
        }
    }

    fn code(self) -> i64 {
        self.exit() as i64
    }
}

/// The gate's answer as data: the state, the refusal sentence (empty unless
/// refused), the head the answer pins (the caller's TOCTOU pin), and the
/// note naming a recompute outcome, a dead probe, a waiver, or a discharged
/// budget.
#[derive(Debug, Clone)]
pub struct GateVerdict {
    pub state: GateState,
    pub refusal: String,
    pub covered_head: String,
    pub note: String,
}

/// The caller's discriminator: an overridden COVERED is a merge that landed
/// on the valve or on operator law, never a merge that was reviewed.
pub(crate) const OVERRIDE_NOTE_PREFIX: &str = "override: ";

/// The note on the one COVERED answer that means "this gate does not apply
/// here".
pub(crate) const NO_LANE_NOTE: &str = "no review lane configured";

const TRUNCATED_SENTENCE: &str = "attestation chain findings were truncated \
(findings_truncated); the truncated remainder is non-terminal, so the gate \
refuses rather than trust a count it cannot re-derive";

const BLOCKING_SENTENCE_TAIL: &str = "; a blocking finding is cleared by \
fixing it and letting the next review cover the fix delta";

const TRUNCATED_KEY: &str = "(truncated remainder)";

/// The one refusal sentence: the reason with the note bracket-appended.
/// Bracket append, never paren-splice surgery: a reason whose trailing paren
/// closes an inner clause (a searched list, a truncated sha) would swallow
/// the note into the wrong parenthetical.
pub(crate) fn refusal_line(refusal: &str, note: &str) -> String {
    if !refusal.is_empty() && !note.is_empty() {
        format!("{refusal} [{note}]")
    } else if refusal.is_empty() {
        note.to_string()
    } else {
        refusal.to_string()
    }
}

/// The head-pinned waiver sentence the Python gate rendered; the overlay
/// owner in `coverage_status` renders the description, this prefixes it so a
/// waived merge and a reviewed one stay legible apart on every surface.
fn waiver_note(description: &str) -> String {
    format!("{OVERRIDE_NOTE_PREFIX}{description}")
}

/// The PR author's login off the REST pulls payload, or None on any read
/// failure. Only read when an override label is held (the one consumer), so
/// a healthy PR never pays for it.
fn pr_author_login<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> Option<String> {
    let args = vec!["api".to_string(), format!("repos/:owner/:repo/pulls/{pr}")];
    let (ok, output) = probes.run_gh(cwd, &args).ok()?;
    if !ok {
        return None;
    }
    let parsed: Value = serde_json::from_str(&output).ok()?;
    parsed
        .pointer("/user/login")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Whether the PR carries the `coverage-override` label. One read; a failed
/// read answers false and the valve stays shut (an unreadable label state
/// must not open it).
fn pr_has_override_label<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> bool {
    let args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr.to_string(),
        "--json".to_string(),
        "labels".to_string(),
        "--jq".to_string(),
        "[.labels[].name] | index(\"coverage-override\") != null".to_string(),
    ];
    match probes.run_gh(cwd, &args) {
        Ok((true, output)) => output.trim() == "true",
        _ => false,
    }
}

/// The login that applied the label, off the paginated issue events feed.
/// The feed is oldest-first, so `--jq`'s `[-1]` picks the latest labeling
/// and the LAST line of a paginated read is the answer. Empty when the read
/// failed or named nobody - an unreadable actor refuses fail-closed.
fn override_label_actor<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> Option<String> {
    let args = vec![
        "api".to_string(),
        "--paginate".to_string(),
        format!("repos/:owner/:repo/issues/{pr}/events"),
        "--jq".to_string(),
        "[.[] | select(.event==\"labeled\" and .label.name==\"coverage-override\")][-1].actor.login // empty"
            .to_string(),
    ];
    let (ok, output) = probes.run_gh(cwd, &args).ok()?;
    if !ok {
        return None;
    }
    output
        .trim()
        .lines()
        .last()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `(held, note, refusal)` for the PR's override label: the one deliberate
/// bypass, with the same author check the approval path beside it has always
/// had. A label applied by the PR AUTHOR does not open the valve, because an
/// agent that cannot merge can label its own PR, and that is
/// reject-and-attest wearing a label. An unreadable actor or author refuses
/// fail-closed. Who applied the label is named in every arm: the receipt
/// survives both verdicts.
fn override_valve<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> (bool, String, String) {
    if !pr_has_override_label(probes, cwd, pr) {
        return (false, String::new(), String::new());
    }
    let Some(actor) = override_label_actor(probes, cwd, pr) else {
        return (
            false,
            String::new(),
            "coverage-override label present but the labelling actor is \
unreadable; refused (an unreadable actor is not an operator waiver)"
                .to_string(),
        );
    };
    let Some(author) = pr_author_login(probes, cwd, pr) else {
        return (
            false,
            String::new(),
            format!(
                "coverage-override label applied by {actor} but the PR author is \
unreadable; refused (cannot prove the labeller is not the author)"
            ),
        );
    };
    // GitHub logins are case-insensitive; an exact-case compare would open
    // the valve for the author the moment the events feed and /pulls/{n}
    // disagree on casing.
    if actor.eq_ignore_ascii_case(&author) {
        return (
            false,
            String::new(),
            format!(
                "coverage-override label applied by {actor}, the PR author; \
refused (an author cannot override its own review gate)"
            ),
        );
    }
    (
        true,
        format!("{OVERRIDE_NOTE_PREFIX}coverage-override label applied by {actor}"),
        String::new(),
    )
}

/// Which gate conjuncts a coverage row satisfies: `(covered, failed)` where
/// failed names the conjunct that broke: `reviewer_refused` / `uncovered` /
/// `no_local_pass` / `stale_head`. `reviewer_refused` stays distinct from
/// generic `uncovered` so consumers can prescribe reviewer recovery. An
/// empty head skips the staleness conjunct.
fn covered_conjuncts(row: &Value, head: &str, code_review_required: bool) -> (bool, &'static str) {
    if row.get("review_state").and_then(Value::as_str) == Some("reviewer_refused") {
        return (false, "reviewer_refused");
    }
    let covered_row = row.get("coverage").and_then(Value::as_str) == Some("covered")
        && row.get("review_state").and_then(Value::as_str) == Some("reviewed");
    if !covered_row {
        return (false, "uncovered");
    }
    if code_review_required && !coverage_has_local_pass(row, "code-review") {
        return (false, "no_local_pass");
    }
    if let Some(ev_head) = row.get("head_sha").and_then(Value::as_str) {
        if !ev_head.is_empty() && !head.is_empty() && head != ev_head {
            return (false, "stale_head");
        }
    }
    (true, "")
}

/// Whether the row carries a head-pinned local pass from `reviewer`.
fn coverage_has_local_pass(row: &Value, reviewer: &str) -> bool {
    let target = reviewer.trim().trim_start_matches('/');
    row.get("verdicts")
        .and_then(Value::as_array)
        .is_some_and(|verdicts| {
            verdicts.iter().any(|v| {
                v.get("verdict").and_then(Value::as_str) == Some("reviewed")
                    && v.get("producer").and_then(Value::as_str) == Some("local_attestation")
                    && v.get("name")
                        .and_then(Value::as_str)
                        .map(|n| n.trim().trim_start_matches('/'))
                        == Some(target)
            })
        })
}

/// The refusal when the resolved review posture is unsatisfied or unknown.
/// The verdict is the Rust producer's (`review_posture` on the row); this
/// gate reads it and never reclassifies. A covered row that NAMES no posture
/// is a two-case read: a row the recompute JUST wrote with no rung names the
/// binary upgrade; a STORED pre-posture row passes through as legacy
/// evidence.
fn posture_refusal(row: &Value, recomputed: bool) -> Option<String> {
    if row.get("coverage").and_then(Value::as_str) != Some("covered") {
        return None;
    }
    // An ABSENT field and a non-object value are the same read, exactly as
    // the Python gate's single `isinstance(posture, dict)` branch held them:
    // both mean "no readable rung", and only a recomputed row refuses on it.
    let Some(posture) = row.get("review_posture").and_then(Value::as_object) else {
        if !recomputed {
            return None;
        }
        return Some(
            "review posture unknown: a fresh coverage recompute resolved no \
review.posture, so the ladder cannot answer what review the rung demands. \
Remedy: refresh the fno-agents binary (`fno doctor update`) and re-run the \
merge"
                .to_string(),
        );
    };
    if posture.get("posture_satisfied").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let gaps = posture
        .get("posture_gaps")
        .and_then(Value::as_array)
        .filter(|gaps| !gaps.is_empty())
        .map(|gaps| {
            gaps.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_else(|| "the rung's evidence is not at this head".to_string());
    let posture_word = posture.get("posture").and_then(Value::as_str).unwrap_or("");
    let rank = posture
        .get("rank")
        .map(|r| r.to_string())
        .unwrap_or_default();
    Some(format!(
        "review posture '{posture_word}' (rank {rank}) is unsatisfied: {gaps}"
    ))
}

/// Whether a verdict went stale - optionally, one from a named reviewer.
/// The name discriminator keeps the waiver text attached to the refusal it
/// answers; names normalize with a leading-slash strip.
fn has_stale_verdict(row: &Value, name: Option<&str>) -> bool {
    let Some(stale) = row.get("stale_verdicts").and_then(Value::as_array) else {
        return false;
    };
    match name {
        None => !stale.is_empty(),
        Some(target) => {
            let target = target.trim().trim_start_matches('/');
            stale.iter().any(|v| {
                v.get("name")
                    .and_then(Value::as_str)
                    .map(|n| n.trim().trim_start_matches('/'))
                    == Some(target)
            })
        }
    }
}

/// The refusal when a STALE attestation caused it: a standing operator
/// ruling waives the gate there, and a worker who cannot see the lever
/// cannot choose it. Advisory text; the caller decides whether staleness is
/// the cause, because only the caller knows which conjunct failed.
fn with_stale_waiver_guidance(refusal: &str, pr: u64) -> String {
    format!(
        "{refusal}. A prior attestation exists but its head moved, which a \
standing operator ruling waives: with CI green and no unresolved P1, this \
may merge without a head-pinned attestation. P1 does not waive and still \
blocks. The lever is attended-operator only: `fno do pr coverage-waive {pr} \
--reason ...`"
    )
}

/// The REFUSED-side note: the budget a worker can see before the cap trips.
fn rounds_remaining_note(rounds: i64, max_rounds: i64) -> String {
    // One less than the raw difference: at rounds = max_rounds - 1 the NEXT
    // round is the last the budget funds, so zero remain after it.
    let remaining = max_rounds - rounds - 1;
    if remaining <= 0 {
        return format!(
            "{rounds}/{max_rounds} review rounds used; the next round is the \
last the budget funds"
        );
    }
    let plural = if remaining == 1 { "" } else { "s" };
    format!(
        "{rounds}/{max_rounds} review rounds used; {remaining} round{plural} \
remain before the cap discharges the review phase"
    )
}

/// Why the guard refused, for the blocked receipt line. Names the CAUSE and
/// the next action, not just the count. `head` is the PR's observed head,
/// passed only when the caller fetched it; its absence is why the staleness
/// branch cannot fire on an unobserved head.
fn coverage_refused_reason(
    row: Option<&Value>,
    head: &str,
    sources: Option<&[String]>,
    self_review_hint: Option<&str>,
) -> String {
    let Some(row) = row else {
        // Name WHERE it looked, not just that it found nothing: the absence
        // is the one refusal a reader cannot diagnose from a count.
        let where_suffix = sources
            .filter(|s| !s.is_empty())
            .map(|s| format!(" (searched: {})", s.join(", ")))
            .unwrap_or_default();
        return format!("no review_coverage event for this PR{where_suffix}");
    };
    let cov_word = row.get("coverage").and_then(Value::as_str).unwrap_or("");
    // `uncovered` is a real known zero and must reach the naming branches
    // below; only `unknown` (and anything a future producer invents) stops
    // at the bare word.
    if cov_word != "covered" && cov_word != "uncovered" {
        return format!("coverage {cov_word}");
    }
    let prefix = if cov_word != "covered" {
        format!("coverage {cov_word}: ")
    } else {
        String::new()
    };
    let sized = |line: &str| match self_review_hint {
        Some(hint) => format!("{line} - `{hint}`"),
        None => line.to_string(),
    };
    let ev_head = row.get("head_sha").and_then(Value::as_str).unwrap_or("");
    if !head.is_empty() && !ev_head.is_empty() && head != ev_head {
        return sized(&format!(
            "coverage was computed at {} but HEAD is {}; attestations are \
head-pinned by design - re-run the review verb at HEAD",
            &ev_head[..ev_head.len().min(8)],
            &head[..head.len().min(8)],
        ));
    }
    let verdicts = row
        .get("verdicts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let names_with = |verdict_word: &str| -> Vec<String> {
        verdicts
            .iter()
            .filter_map(|v| {
                let name = v.get("name").and_then(Value::as_str)?;
                (v.get("verdict").and_then(Value::as_str) == Some(verdict_word) && !name.is_empty())
                    .then(|| name.to_string())
            })
            .collect()
    };
    let refused = names_with("refused");
    if row.get("review_state").and_then(Value::as_str) == Some("reviewer_refused") {
        let who = if refused.is_empty() {
            "configured reviewer".to_string()
        } else {
            refused.join(", ")
        };
        return sized(&format!(
            "{prefix}reviewer-refused: {who} declined to review; run the \
review verb at HEAD"
        ));
    }
    let absent = names_with("absent");
    let stale = names_with("stale");
    if !absent.is_empty() || !stale.is_empty() {
        let mut waits: Vec<String> = Vec::new();
        if !absent.is_empty() {
            waits.push(format!("waiting on {}", absent.join(", ")));
        }
        if !stale.is_empty() {
            waits.push(format!(
                "{} reviewed an older commit and must re-read",
                stale.join(", ")
            ));
        }
        let reviewed = row
            .get("reviewed_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return format!(
            "{prefix}{reviewed} reviewed (no head-pinned pass attestation; {} \
- if a reviewer there is uninstalled or no longer configured, check \
config.review)",
            waits.join("; ")
        );
    }
    let reviewed = row
        .get("reviewed_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    // The hint SPLICES here, between "at HEAD" and the close-every-finding
    // tail, exactly where the Python gate put it; the trailing sized()
    // placement belongs to the sentences that END at the advice.
    let hint_segment = match self_review_hint {
        Some(h) => format!(" - `{h}`"),
        None => String::new(),
    };
    format!(
        "{prefix}{reviewed} reviewed (no head-pinned pass attestation - run \
the review verb at HEAD{hint_segment} - close every finding, commit and push \
first, then attest at the final head with `bash \
skills/review/scripts/emit-attestation.sh <reviewer>`)"
    )
}

/// The sized self-review invocation for a refusal, or None when the render
/// is unsized (no merge-base against the default branch) or unavailable.
/// Advisory text; a broken render reads as the levelless line.
fn sized_self_review_hint(cwd: &Path) -> Option<String> {
    let root = git_toplevel(cwd)?;
    let (result, _harness) = crate::review_capability::render_self_review_invocation(
        None,
        Some(Path::new(&root)),
        None,
        None,
        None,
        None,
    );
    let rendered = result.ok()?;
    if rendered.contains("<level>") {
        None
    } else {
        Some(rendered)
    }
}

/// The git toplevel of `cwd`, for the reads that want the checkout root.
fn git_toplevel(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// `owner/repo` from the checkout's origin remote, for the waiver subject.
fn git_origin_slug(cwd: &Path) -> Option<String> {
    let url = crate::pr_status::reviews::git_origin_url(cwd)?;
    crate::merge_gates::repo_slug_from_origin(&url)
}

/// The evidence-only verdict: everything but the operator-law overlay. The
/// second tuple element carries `(head, hard)` on every path the overlay may
/// consult, None on the covered/unanswered early returns that never need it.
fn ordinary_verdict<P: Probes>(
    probes: &P,
    cwd: &Path,
    pr: u64,
    recompute: bool,
) -> (GateVerdict, Option<(String, bool)>) {
    let none = |state, note: &str| {
        (
            GateVerdict {
                state,
                refusal: String::new(),
                covered_head: String::new(),
                note: note.to_string(),
            },
            None,
        )
    };
    // Refuse a cwd that is not a directory at the door, before any probe has
    // a chance to misattribute it. UNANSWERED, not REFUSED: a gate that
    // cannot read its own inputs has not judged the PR.
    if !cwd.is_dir() {
        return none(
            GateState::Unanswered,
            &format!("no such directory: {}", cwd.display()),
        );
    }
    // A stock install with no review lane configured opts out of coverage
    // entirely. Checked FIRST so neither the head fetch nor the events read
    // runs for a PR nobody configured review for.
    if !review_lane(cwd, pr) {
        return none(GateState::Covered, NO_LANE_NOTE);
    }
    // The head fetch is an instrument, and it can fail. A missing head is
    // not "no coverage" - it is "the probe that pins coverage to what would
    // actually merge could not run". Refuse to answer rather than guess.
    let facts = match probes.pr_facts(cwd, Some(pr)) {
        Ok(facts) => facts,
        Err(_) => return none(GateState::Unanswered, "pr head fetch failed"),
    };
    let head = facts.head_sha;
    if head.is_empty() {
        return none(GateState::Unanswered, "pr head fetch failed");
    }
    // The valve, checked before the events read so an overridden PR skips
    // the recompute entirely. A label held but REFUSED (author-applied, or
    // an unreadable actor) keeps its refusal for the uncovered answer below.
    let (override_valid, override_note, override_refusal) = override_valve(probes, cwd, pr);
    if override_valid {
        return (
            GateVerdict {
                state: GateState::Covered,
                refusal: String::new(),
                covered_head: head.clone(),
                note: override_note,
            },
            None,
        );
    }
    let inputs = resolve_review_inputs(cwd, None, None, None, None, None);
    // The configured code-review requirement, the same two arms the lane
    // floor resolves: an explicit `code-review` reviewer, or the self-review
    // floor on a lane-less install whose payload is code. The floor arm
    // rides the same predicate the lane read just answered, so a PR that
    // entered the gate at all paid at most one payload probe.
    let code_review_required = inputs.code_review_configured
        || (!inputs.lane_configured
            && !inputs.code_review_configured
            && inputs.self_review_floor_on
            && crate::loopcheck::self_review_floor_applies(
                inputs.author_harness.as_deref(),
                inputs.author_harness_pinned_none,
            )
            && crate::pr_status::reviews::payload_is_code(cwd, pr));
    let row_value = read_review_coverage(cwd, pr, Some(&head), recompute, recompute);
    let (covered, failed) = covered_conjuncts(&row_value, &head, code_review_required);
    let recomputed = row_value.get("recompute").and_then(Value::as_str) == Some("recomputed");
    let posture = posture_refusal(&row_value, recomputed);
    let recompute_note = row_value
        .get("recompute")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The chain read needs the PR's head branch to scope its older rounds;
    // on a covered row a probe miss is an instrument failure, answered
    // UNANSWERED rather than silently narrowing the chain. On an uncovered
    // row the miss keeps today's refusal.
    let head_branch = facts.head_ref;
    if covered && head_branch.is_empty() {
        return none(GateState::Unanswered, "pr head branch fetch failed");
    }
    let events_text = review_journal_text(
        &inputs.project_events,
        &inputs.global_events,
        &inputs.repo_slug,
    );
    let blockers = disposition_blockers(&events_text, &head_branch, &head);
    // Python's disposition refusal, rebuilt from the blocker rows: a
    // truncated chain keeps its own sentence and names the remainder; open
    // findings name their keys and teach the fix-delta remedy.
    let truncated = blockers.iter().any(|b| b.axis == "truncated-remainder");
    let disposition_named: Vec<String> = if truncated {
        vec![TRUNCATED_KEY.to_string()]
    } else {
        let mut keys: Vec<String> = blockers.iter().map(|b| b.finding_key.clone()).collect();
        keys.sort();
        keys
    };
    let disposition_text = if truncated {
        TRUNCATED_SENTENCE.to_string()
    } else if !disposition_named.is_empty() {
        format!(
            "blocking finding(s) not terminal: {};{BLOCKING_SENTENCE_TAIL}",
            disposition_named.join(", ")
        )
    } else {
        String::new()
    };
    // The standing operator waiver never clears a CONFIRMED correctness or
    // security finding, so the overlay's input carries the hard subset. A
    // truncated chain counts every key hard: the gate cannot re-derive what
    // it cannot read.
    let waiver_hard = blockers.iter().any(|b| b.hard) || truncated;
    // The budget counts BOTH evidence axes; the paginated reviews read is
    // paid only where it can change the answer. A read failure keeps the
    // events-only answer, and says so.
    let mut reviews_payload: Option<Vec<Value>> = None;
    let mut unread_note = String::new();
    // `named` and `text` are built together (truncated names one key and
    // carries its own sentence; otherwise the keys and the sentence stand or
    // fall as one), so one conjunct carries the trigger.
    if !disposition_named.is_empty() || !covered {
        match read_reviews_payload(probes, cwd, pr) {
            Ok(rows) => reviews_payload = Some(rows),
            Err(why) => unread_note = format!("reviews read unavailable: {why}"),
        }
    }
    let max_rounds = inputs.settings.max_rounds.unwrap_or(2).max(1) as i64;
    let mut rounds = rounds_since_last_pass(
        &events_text,
        &head_branch,
        &head,
        reviews_payload.as_deref(),
    );
    // The row's own budget bit discharges too (the producer is the one
    // counter, and a refusal past its spent budget would be unsatisfiable by
    // construction); its rounds_used lifts the printed count when the
    // re-derivation landed under the cap.
    let row_spent = row_value.get("rounds_exhausted").and_then(Value::as_bool) == Some(true);
    if row_spent {
        let row_rounds = row_value
            .get("rounds_used")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        rounds = rounds.max(row_rounds);
    }
    // The row's pin: evidence of WHAT was judged, rendered in the note
    // bracket of every refused verdict whose row could name it. Python
    // rendered the same bracket on the check line and the op door alike.
    let pin_note = row_value
        .get("coverage_pin")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let join_notes = |extra: Option<&str>| -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !recompute_note.is_empty() {
            parts.push(&recompute_note);
        }
        if !unread_note.is_empty() {
            parts.push(&unread_note);
        }
        if !pin_note.is_empty() {
            parts.push(&pin_note);
        }
        if let Some(e) = extra {
            parts.push(e);
        }
        parts.join("; ")
    };
    // Reaching the cap DISCHARGES the review obligation - it does not fail
    // it. `>=`, not `>`: max_rounds is a MAXIMUM, and the last funded round
    // is the second one. The waiver is NAMED in the note, never silent.
    if rounds >= max_rounds || row_spent {
        let mut unsatisfied_parts: Vec<String> = Vec::new();
        if !failed.is_empty() {
            unsatisfied_parts.push(failed.to_string());
        }
        if code_review_required {
            unsatisfied_parts.push("code-review attestation required by config".to_string());
        }
        let unsatisfied = unsatisfied_parts.join("; ");
        let waiver = format!(
            "review budget discharged ({rounds}/{max_rounds} rounds): the \
review phase is complete; open findings stay in the PR conversation and the \
PR merges on green CI; the operator lever is config.review.max_rounds"
        );
        let waiver = if unsatisfied.is_empty() {
            waiver
        } else {
            format!("{waiver} (waived at the cap: {unsatisfied})")
        };
        let note = join_notes(Some(&waiver));
        return (
            GateVerdict {
                state: GateState::Covered,
                refusal: String::new(),
                covered_head: head,
                note,
            },
            None,
        );
    }
    // The Rust posture verdict, read below the cap only: a budget ruling
    // waives rounds while a posture names what satisfied review must already
    // contain.
    if covered {
        if let Some(posture_text) = posture {
            return (
                GateVerdict {
                    state: GateState::Refused,
                    refusal: posture_text,
                    covered_head: String::new(),
                    note: join_notes(None),
                },
                Some((head, waiver_hard)),
            );
        }
        if !disposition_text.is_empty() {
            // Rounds remain, so the refusal teaches the fix-delta remedy AND
            // shows the budget the next round spends.
            let remaining = rounds_remaining_note(rounds, max_rounds);
            let note = join_notes(Some(&remaining));
            return (
                GateVerdict {
                    state: GateState::Refused,
                    refusal: disposition_text,
                    covered_head: String::new(),
                    note,
                },
                Some((head, waiver_hard)),
            );
        }
        let covered_head = row_value
            .get("head_sha")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return (
            GateVerdict {
                state: GateState::Covered,
                refusal: String::new(),
                covered_head,
                note: join_notes(None),
            },
            None,
        );
    }
    // An uncovered row that a fail attestation answers at this head is
    // uncovered BECAUSE of the non-terminal findings: name them instead of
    // the generic "0 reviewed" text that taught the loop to re-review.
    if failed == "uncovered"
        && !disposition_named.is_empty()
        && events_text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|val| val.get("type").and_then(Value::as_str) == Some("review_attestation"))
            .any(|val| val.pointer("/data/verdict").and_then(Value::as_str) != Some("pass"))
    {
        let remaining = rounds_remaining_note(rounds, max_rounds);
        let note = join_notes(Some(&remaining));
        return (
            GateVerdict {
                state: GateState::Refused,
                refusal: disposition_text,
                covered_head: String::new(),
                note,
            },
            Some((head, waiver_hard)),
        );
    }
    let hint = sized_self_review_hint(cwd);
    let (mut refusal, stale_caused_it) = if code_review_required
        && !coverage_has_local_pass(&row_value, "code-review")
    {
        let sized = match &hint {
            Some(h) => format!(" - `{h}`"),
            None => String::new(),
        };
        (
            format!(
                "required code-review has no head-pinned local pass \
attestation; run the harness review verb at HEAD{sized}, then emit the \
code-review attestation"
            ),
            failed != "reviewer_refused" && has_stale_verdict(&row_value, Some("code-review")),
        )
    } else {
        let sources = if row_value.get("coverage").is_none() {
            Some(coverage_sources(cwd))
        } else {
            None
        };
        let reason =
            coverage_refused_reason(Some(&row_value), &head, sources.as_deref(), hint.as_deref());
        (
            reason,
            failed != "reviewer_refused" && has_stale_verdict(&row_value, None),
        )
    };
    // The stale-verdict guidance rides here, where the row is in scope: one
    // site, and a granted waiver discards the refusal anyway.
    if stale_caused_it {
        refusal = with_stale_waiver_guidance(&refusal, pr);
    }
    if !override_refusal.is_empty() {
        // The label is present but stayed shut; naming that first beats a
        // generic uncovered refusal the operator cannot act on.
        refusal = format!("{override_refusal}; {refusal}");
    }
    let note = join_notes(None);
    (
        GateVerdict {
            state: GateState::Refused,
            refusal,
            covered_head: String::new(),
            note,
        },
        Some((head, waiver_hard)),
    )
}

/// The event logs a coverage read consults, for the refusal text. Mirrors
/// the Python `coverage_sources`: the project journal and the global one,
/// whatever resolved.
fn coverage_sources(cwd: &Path) -> Vec<String> {
    let inputs = resolve_review_inputs(cwd, None, None, None, None, None);
    vec![
        inputs.project_events.display().to_string(),
        inputs.global_events.display().to_string(),
    ]
}

/// The paginated REST reviews read the round budget pays for, mapped into
/// the `{state, commit.oid}` shape the counter reads. Bounded per call; a
/// failed read answers the reason.
fn read_reviews_payload<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> Result<Vec<Value>, String> {
    let slug = git_origin_slug(cwd).ok_or_else(|| "repo slug unreadable".to_string())?;
    let mut rows: Vec<Value> = Vec::new();
    for page in 1..=100u32 {
        let args = vec![
            "api".to_string(),
            format!("repos/{slug}/pulls/{pr}/reviews?per_page=100&page={page}"),
        ];
        let (ok, output) = probes
            .run_gh(cwd, &args)
            .map_err(|e| format!("pull reviews read failed: {e}"))?;
        if !ok {
            let tail: String = output.lines().last().unwrap_or("gh exited nonzero").into();
            return Err(format!("pull reviews read failed: {}", tail.trim()));
        }
        let page_rows: Value = serde_json::from_str(&output)
            .map_err(|_| "pull reviews read failed: page was not JSON".to_string())?;
        let Some(list) = page_rows.as_array() else {
            return Err("pull reviews read failed: page was not a list".to_string());
        };
        let count = list.len();
        for row in list {
            rows.push(json!({
                "state": row.get("state").and_then(Value::as_str).unwrap_or(""),
                "commit": {
                    "oid": row.get("commit_id").and_then(Value::as_str).unwrap_or(""),
                },
            }));
        }
        if count < 100 {
            break;
        }
    }
    Ok(rows)
}

/// The public verdict: the ordinary evidence answer with the operator-law
/// overlay applied to a refused head. The overlay is consulted ONLY where
/// ordinary coverage refused; a waiver lookup that answers UNKNOWN authority
/// is UNANSWERED with the probe named - never a refusal built on a store
/// that could not answer, and never permission either.
pub(crate) fn verdict<P: Probes>(probes: &P, cwd: &Path, pr: u64, recompute: bool) -> GateVerdict {
    let (verdict, waiver_inputs) = ordinary_verdict(probes, cwd, pr, recompute);
    let Some((head, hard)) = waiver_inputs else {
        return verdict;
    };
    if verdict.state != GateState::Refused {
        return verdict;
    }
    let fno_bin = loopcheck_fno_bin();
    // An unreadable slug scopes OUT the per-head subject only; the standing
    // ruling is consulted either way, exactly as the overlay's own contract
    // reads (an empty slug is NoLaw for the scoped probe, never a skip of
    // the standing one).
    let slug = git_origin_slug(cwd).unwrap_or_default();
    let (waiver, authority_unknown) = operator_waiver(&fno_bin, cwd, &slug, pr as i64, &head, hard);
    if let Some(description) = waiver {
        // The live head is the pin: a waiver covers exactly the head its
        // subject names, so the merge still refuses a push that races it.
        return GateVerdict {
            state: GateState::Covered,
            refusal: String::new(),
            covered_head: head,
            note: waiver_note(&description),
        };
    }
    if authority_unknown {
        return GateVerdict {
            state: GateState::Unanswered,
            refusal: String::new(),
            covered_head: String::new(),
            note: "operator waiver authority unknown (decision probe)".to_string(),
        };
    }
    verdict
}

/// The verb bodies behind `fno-agents review-coverage check|publish|waive|op`.
/// `check`, `publish` and `waive` are the human verbs; `op` reads one JSON
/// payload on stdin and answers one JSON line on stdout with exit 0, the
/// shape `verb_call(["review-coverage", "op"], payload)` expects.
pub(crate) fn run_operand(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("check") => run_check_operand(&args[1..]),
        Some("publish") => crate::loopcheck::coverage_gate::publish::run(&args[1..]),
        Some("waive") => crate::loopcheck::coverage_gate::waive::run(&args[1..]),
        Some("op") => run_op_door(),
        _ => {
            eprintln!("review-coverage: unknown operand (want check, publish, waive or op)");
            2
        }
    }
}

fn run_check_operand(args: &[String]) -> i32 {
    let mut pr: Option<u64> = None;
    let mut recompute = false;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--recompute" {
            // The valueless spelling is the common one and sits LAST in the
            // forwarded argv, where try_flag_value would read past the end
            // and silently drop the flag.
            recompute = true;
        } else if let Some(val) = try_flag_value(&args[i], "--recompute", args, &mut i) {
            recompute = matches!(val.as_str(), "" | "true" | "1");
        } else if let Some(val) = try_flag_value(&args[i], "--cwd", args, &mut i) {
            cwd = PathBuf::from(val);
        } else if !args[i].starts_with('-') {
            pr = args[i].parse::<u64>().ok();
        }
        i += 1;
    }
    let Some(pr) = pr else {
        eprintln!("review-coverage check: a PR number is required");
        return 2;
    };
    let v = verdict(&crate::authorized_merge::RealProbes, &cwd, pr, recompute);
    match v.state {
        GateState::Refused => {
            eprintln!("{}", refusal_line(&v.refusal, &v.note));
        }
        GateState::Unanswered => {
            eprintln!("{}", v.note);
        }
        GateState::Covered => {
            if v.note.starts_with(OVERRIDE_NOTE_PREFIX) {
                // Waived-covered receipt: the merge decision parses it off
                // stdout, so a waived ready never reads as reviewed.
                println!("{}", v.note);
            }
        }
    }
    v.state.exit()
}

/// Read one JSON payload on stdin and answer one JSON line with exit 0. The
/// ops mirror the Python legs the callers already hold: `verdict` for the
/// gate answer, `chain` for the attestation-chain read the review-hold
/// surfaces make, `lane` for the lane predicate.
fn run_op_door() -> i32 {
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        eprintln!("review-coverage op: stdin did not answer");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("review-coverage op: unreadable payload: {e}");
            return 2;
        }
    };
    let op = parsed.get("op").and_then(Value::as_str).unwrap_or("");
    let cwd = parsed
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let answer = match op {
        "verdict" => {
            let Some(pr) = parsed.get("pr").and_then(Value::as_u64) else {
                eprintln!("review-coverage op: verdict needs a numeric pr");
                return 2;
            };
            let recompute = parsed
                .get("recompute")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let v = verdict(&crate::authorized_merge::RealProbes, &cwd, pr, recompute);
            json!({
                "state": v.state.code(),
                "refusal": v.refusal,
                "covered_head": v.covered_head,
                "note": v.note,
                "line": refusal_line(&v.refusal, &v.note),
            })
        }
        "chain" => {
            let branch = parsed
                .get("branch")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let head = parsed
                .get("head")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let inputs = resolve_review_inputs(&cwd, None, None, None, None, None);
            let events_text = review_journal_text(
                &inputs.project_events,
                &inputs.global_events,
                &inputs.repo_slug,
            );
            let chain = super::local_attestation::in_scope_chain(&events_text, &branch, &head);
            let last = chain.last().map(|row| {
                let blocking = row
                    .pointer("/data/findings")
                    .and_then(Value::as_array)
                    .map(|findings| {
                        findings
                            .iter()
                            .filter(|f| {
                                f.get("blocking").and_then(Value::as_bool)
                                    == Some(true)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                json!({
                    "head_sha": row.pointer("/data/head_sha").and_then(Value::as_str).unwrap_or(""),
                    "reviewed_base_sha": row.pointer("/data/reviewed_base_sha").and_then(Value::as_str).unwrap_or(""),
                    "blocking": blocking,
                    "findings_truncated": row
                        .pointer("/data/findings_truncated")
                        .and_then(Value::as_bool)
                        == Some(true),
                })
            });
            json!({
                "rounds_used": rounds_since_last_pass(&events_text, &branch, &head, None),
                "max_rounds": inputs.settings.max_rounds.unwrap_or(2).max(1),
                "last": last,
            })
        }
        "lane" => {
            let code_payload = parsed
                .get("code_payload")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let lane = crate::pr_status::reviews::lane_answer(&cwd, code_payload);
            json!({ "lane": lane })
        }
        other => {
            eprintln!("review-coverage op: unknown op {other:?} (want verdict, chain or lane)");
            return 2;
        }
    };
    println!("{answer}");
    0
}

pub(crate) mod publish;
pub(crate) mod waive;

#[cfg(test)]
mod posture_refusal_tests {
    use super::*;

    #[test]
    fn absent_posture_refuses_on_a_recomputed_row() {
        let row = json!({"coverage": "covered"});
        assert_eq!(
            posture_refusal(&row, true).unwrap(),
            "review posture unknown: a fresh coverage recompute resolved no \
review.posture, so the ladder cannot answer what review the rung demands. \
Remedy: refresh the fno-agents binary (`fno doctor update`) and re-run the \
merge"
        );
    }

    #[test]
    fn absent_posture_passes_a_stored_pre_posture_row() {
        let row = json!({"coverage": "covered"});
        assert_eq!(posture_refusal(&row, false), None);
    }

    #[test]
    fn null_posture_refuses_on_a_recomputed_row() {
        let row = json!({"coverage": "covered", "review_posture": null});
        assert!(posture_refusal(&row, true).is_some());
    }
}
