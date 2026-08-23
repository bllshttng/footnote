//! `fno-agents finalize` (control-plane step 6, ab-f8e5f214): the terminal-only
//! WRITER the stop-hook shim invokes on a terminal-allow `loop-check` decision.
//!
//! It re-homes the mechanical session side-effects out of the skill's
//! pre-promise bash so they fire in EVERY mode (attended, autonomous, megawalk
//! worker) and survive context compaction:
//!
//! - **Always** (any terminal reason): one ledger session-record, carrying
//!   `graph_node_id` + `provider_id` + scalar `session_id` + `cost_usd` + a new
//!   `termination_reason`, so a node's true cost and full session list roll up
//!   by grouping ledger entries on `graph_node_id` (US7).
//! - **Legacy ship** (`DonePRGreen` / `DoneAdvisory`): plan stamp + a mechanical
//!   git-derived handoff artifact. A code ship (DonePRGreen) stamps `in_review`
//!   only (done = merged, x-f34f; the flip happens at merge). An advisory ship
//!   (DoneAdvisory) has no merge event, so it also graduates to `done` here.
//! - **Generic delivery** (`DoneDelivery`): consume the selected strict verdict,
//!   stamp or safely graduate with its receipt, and write a generic handoff.
//! - **`DonePRGreen` only**, when the manifest approves it: arm GitHub's native
//!   auto-merge. This is the one terminal that means "green and reviewed", and
//!   arming it HERE rather than at PR creation is the whole point (x-1951) -
//!   see `should_arm_auto_merge`.
//!
//! ## Why this does not break the read-only stop hook
//!
//! `loop-check` stays a pure read-only DECISION verb; `finalize` is a separate
//! WRITER the shim runs AFTER the allow decision. Nothing `finalize` writes is
//! read by a future `loop-check` decision as a gate:
//!
//! - `loop-check`'s budget axis reads ledger `cost_usd` filtered by THIS
//!   session's `session_id`. `finalize`'s terminal ledger row for the same
//!   session can only push a re-fire toward termination (a higher cost trips
//!   `Budget`, which is itself terminal-allow), never away from it - and a
//!   re-fire early-returns on the `session_finalized` event anyway.
//! - A DIFFERENT session's `loop-check` filters by its own `session_id`, so it
//!   never reads this session's finalize row.
//!
//! ## Non-fatal + idempotent
//!
//! Every sub-step records failures in a `session_finalize_failed` event.
//! Legacy terminal side effects remain non-blocking, while `DoneDelivery`
//! returns failure so the stop-hook can keep the session alive and retry its
//! required receipt, stamp, and handoff writes.
//! `session_finalized` is emitted ONLY when every attempted sub-step succeeded;
//! a partial failure leaves it unemitted so a later stop-hook fire retries the
//! remaining work (each shelled script is itself idempotent: ledger flock +
//! scalar-session-id dedup, first-writer-wins stamp, filename-keyed handoff).
//!
//! The proven Python helpers (`fno.cost._session_cost`, `fno.cost._register`,
//! `fno.plan._stamp`, all in-package modules run via `python3 -m`) do the
//! cost/dedup/flock/stamp work; this verb is a thin orchestrator (Locked
//! Decision 6 - avoids the Python->Rust byte-parity trap), so the shim keeps
//! its Rust-only dependency surface (Domain Pitfall).

use crate::loopcheck::{emit_to_both, now_rfc3339_utc};
use crate::run_outcome::classify_legacy;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether a completion eval fires for this termination reason (x-8fc0).
///
/// Before this, an eval only ran for `STUCK_REASONS` (then named
/// `POSTMORTEM_REASONS`) - a failure-only sample that the autocorrect monthly
/// review mined for rules. A failure-only sample writes rules in a
/// predictable direction: nothing ever confirmed what a clean session did
/// right, so every lesson skewed toward caution. The operator ruling
/// (2026-08-14) is that the eval must run on every completion, success and
/// failure alike, so the corpus stops being biased by construction.
///
/// `NoWork` is the sole exclusion: it means megawalk/backlog found nothing
/// to do, so there is no session to evaluate. Every other terminal - every
/// ship reason, every stuck reason, `DoneBatched`/`DoneAwaitingMerge`/
/// `DoneUnreviewed`/`DoneAwaitingReview`/`DonePlanned` alike - gets an eval.
fn eval_should_fire(reason: &str) -> bool {
    classify_legacy(reason)
        .map(|record| record.projection().record_ledger)
        .unwrap_or(false)
}

fn is_stuck_reason(reason: &str) -> bool {
    classify_legacy(reason)
        .map(|record| record.projection().stuck)
        .unwrap_or(false)
}

// ── arg parsing ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct FinalizeArgs {
    state: Option<PathBuf>,
    transcript: Option<PathBuf>,
    cwd: Option<PathBuf>,
    reason: Option<String>,
    // Overrides (primarily for tests / non-default layouts).
    events: Option<PathBuf>,
    global_events: Option<PathBuf>,
    settings: Option<PathBuf>,
    handoffs_dir: Option<PathBuf>,
    postmortems_dir: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<FinalizeArgs, String> {
    let mut a = FinalizeArgs::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let take = |it: &mut std::slice::Iter<String>| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--state" => a.state = Some(PathBuf::from(take(&mut it)?)),
            "--transcript" => a.transcript = Some(PathBuf::from(take(&mut it)?)),
            "--cwd" => a.cwd = Some(PathBuf::from(take(&mut it)?)),
            "--reason" => a.reason = Some(take(&mut it)?),
            "--events" => a.events = Some(PathBuf::from(take(&mut it)?)),
            "--global-events" => a.global_events = Some(PathBuf::from(take(&mut it)?)),
            "--settings" => a.settings = Some(PathBuf::from(take(&mut it)?)),
            "--handoffs-dir" => a.handoffs_dir = Some(PathBuf::from(take(&mut it)?)),
            "--postmortems-dir" => a.postmortems_dir = Some(PathBuf::from(take(&mut it)?)),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(a)
}

const HELP: &str = "fno-agents finalize - terminal-only side-effect writer (step 6)\n\
Usage: fno-agents finalize --state <target-state.md> --cwd <project-root> --reason <TerminationReason> \\\n\
                           [--transcript <transcript.jsonl>] [--events <p>] [--global-events <p>] \\\n\
                           [--settings <p>] [--handoffs-dir <p>] [--postmortems-dir <p>]\n\
Reason values: DonePRGreen|DoneAdvisory|DoneDelivery|DoneBatched|DoneAwaitingMerge|DoneAwaitingReview|DonePlanned|NoWork|Budget|NoProgress|Interrupted|Aborted";

// ── manifest fields finalize reads directly ────────────────────────────────

/// The three manifest fields finalize needs itself (everything else is read by
/// the shelled Python helpers from the same manifest path).
#[derive(Debug, Default)]
struct ManifestFields {
    /// Target-minted session id: idempotency key, handoff filename, event data.
    session_id: Option<String>,
    /// Canonical target-minted id, retained separately so it wins regardless of
    /// manifest key order over the one-release `session_id` fallback.
    fno_id: Option<String>,
    /// Claude transcript UUID: positional arg to fno.cost._session_cost / _register.
    claude_transcript_id: Option<String>,
    /// Plan to stamp/graduate (ship branch only). Empty/absent -> skip.
    plan_path: Option<String>,
    /// Feature title for the handoff header.
    input: Option<String>,
    /// Backlog node id (lives in the manifest BODY, below the frontmatter).
    graph_node_id: Option<String>,
    /// Harness (conversation) session id captured at init: the do-stamp's
    /// identity-continuity input, passed through to the Python primitive.
    harness_session_id: Option<String>,
    /// HEAD at init: baseline for the `initial_head..HEAD` work-evidence range.
    /// Absent on manifests minted before x-0469 -> the do stamp skips.
    initial_head: Option<String>,
    /// Init instant: the author-date floor for work evidence, and the value the
    /// do row carries as `started_at` (the start of the implementation window).
    created_at: Option<String>,
    /// Cross-project plan: graduation must wait for ALL project PRs, so the
    /// expected URL count is derived from the plan's `projects:` map, never 1.
    cross_project: bool,
    /// Merge posture resolved by init (config folded with this run's modifiers,
    /// where every refusal outranks every grant). Gates arming GitHub's native
    /// auto-merge at a green terminal. `None` = the key was absent.
    auto_merge_approved: Option<bool>,
    /// Which input set the posture (x-9d11): config | flag-no-merge |
    /// env-target-auto-merge | default-off. `None` = pre-provenance manifest;
    /// surfaced as `unknown`, never guessed. Advisory only: arming still reads
    /// `auto_merge_approved`.
    auto_merge_source: Option<String>,
}

/// Does this line close the double-quoted scalar `init-target-state.sh` opened?
///
/// The rule is NOT backslash parity. The writer escapes quotes and NOT
/// backslashes (`${INITIAL_INPUT//\"/\\\"}`, init:811), so the manifest is not
/// backslash-escaped YAML and a parity rule is wrong in both directions: user
/// text ending in `\"` arrives as `\\"`, which parity reads as even and closes
/// the scalar early, handing the forgery back.
///
/// What that escaping DOES guarantee is one-directional and enough: every quote
/// the user typed gets exactly one `\` prepended, so a user quote is ALWAYS
/// immediately preceded by a backslash, however many backslashes they typed. A
/// closing quote with no backslash before it therefore cannot have come from the
/// user, and is the terminator.
///
/// The residual ambiguity is input ending in a lone `\`: its own closing quote
/// carries a preceding backslash and so reads as user text, and the scalar
/// instead ends at the next quoted line - `plan_path: "..."` (init:840) in the
/// real layout. That costs one line of reduced trust and nothing else, because
/// `parse_manifest_fields` lets the terminator line fall through and parse; only
/// the merge posture consults the mark, and `plan_path` does not.
fn ends_quoted_scalar(line: &str) -> bool {
    let Some(rest) = line.strip_suffix('"') else {
        return false;
    };
    !rest.ends_with('\\')
}

/// Scan the WHOLE manifest (frontmatter AND body) for the keys we need.
/// `graph_node_id`/`target_claim_*` live below the closing `---`, so a
/// frontmatter-only parse (like loop-check's) would miss them.
fn parse_manifest_fields(content: &str) -> ManifestFields {
    let mut m = ManifestFields::default();
    // Init writes the run's raw argument as `input: "<...>"` (init:839), so a
    // MULTI-LINE argument spills real newlines into the manifest and every
    // continuation line reaches this loop looking like a `key: value` pair.
    // `input` is written BEFORE the canonical `auto_merge_approved` (init:886),
    // so a pasted spec containing that key would be read as the merge posture
    // and outrank the real refusal below it.
    //
    // Lines inside that scalar are tracked as UNTRUSTED rather than skipped.
    // Skipping them is what an earlier cut of this did, and it silently ate
    // `plan_path`: the scalar's terminator is ambiguous for input ending in a
    // lone backslash (see `ends_quoted_scalar`), and an over-long skip swallowed
    // the very next line - dropping the plan stamp with no error. Only the merge
    // posture is withheld here, so every other field parses exactly as it did
    // before this guard existed and an ambiguous scalar costs nothing.
    //
    // That asymmetry is the whole safety argument: an unterminated scalar marks
    // MORE lines untrusted, and untrusted only ever withholds the grant, leaving
    // `auto_merge_approved` as `None` -> no arming. Both directions fail closed.
    let mut untrusted = false;
    for line in content.lines() {
        let line = line.trim();
        // The terminator line closes the scalar for everything AFTER it, but is
        // itself still untrusted: when the scalar ends on the same line as the
        // user's last line of text, that line is user text wearing a closing
        // quote (`auto_merge_approved: true"`). It must still FALL THROUGH and
        // parse, never be consumed - for input ending in a lone backslash the
        // real terminator is `plan_path: "..."` (init:840), and skipping it was
        // how an earlier cut silently dropped the plan stamp.
        let line_untrusted = untrusted;
        if untrusted && ends_quoted_scalar(line) {
            untrusted = false;
        }
        // Skip markdown headings and frontmatter fences; a `key: value` match
        // below is all we want.
        if line.is_empty() || line.starts_with('#') || line == "---" {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let raw = v.trim();
        // A multi-line `input` opens a quoted scalar here; everything up to its
        // closing quote is the user's text, not manifest keys.
        if !line_untrusted
            && k == "input"
            && raw.starts_with('"')
            && !(raw.len() >= 2 && ends_quoted_scalar(raw))
        {
            untrusted = true;
        }
        let v = raw.trim_matches(|c| c == '"' || c == '\'');
        // First non-empty wins (frontmatter precedes body); never overwrite a
        // real value with a later blank.
        let set = |slot: &mut Option<String>, val: &str| {
            if slot.is_none() && !val.is_empty() && val != "null" {
                *slot = Some(val.to_string());
            }
        };
        match k {
            // fno_id is canonical regardless of key order; session_id is the
            // pre-rename fallback.
            "fno_id" => set(&mut m.fno_id, v),
            "session_id" => set(&mut m.session_id, v),
            // Current key is claude_session_id; accept the pre-rename
            // claude_transcript_id as a fallback for one release. `set` keeps the
            // first non-empty value, so the current key (written first) wins.
            "claude_session_id" | "claude_transcript_id" => set(&mut m.claude_transcript_id, v),
            "plan_path" => set(&mut m.plan_path, v),
            "input" => set(&mut m.input, v),
            "graph_node_id" => set(&mut m.graph_node_id, v),
            "harness_session_id" => set(&mut m.harness_session_id, v),
            "initial_head" => set(&mut m.initial_head, v),
            "created_at" => set(&mut m.created_at, v),
            "cross_project" => m.cross_project = v == "true",
            // The merge-authority read, and the only key that consults
            // `untrusted`. First occurrence wins, unlike cross_project's
            // last-wins, so a trailing line cannot overwrite the canonical one
            // either. A line inside the `input` scalar is ignored outright: the
            // "arbitrary prose must never grant merge authority" rule init
            // applies when it folds the posture (x-e938) has to hold here too,
            // or the fold is decorative.
            "auto_merge_approved" if !line_untrusted && m.auto_merge_approved.is_none() => {
                m.auto_merge_approved = Some(v == "true")
            }
            // Provenance (x-9d11): same untrusted-line rule as the posture
            // itself - prose inside the `input` scalar must not be able to
            // claim an origin either. Advisory, so no separate trust gate.
            "auto_merge_source" if !line_untrusted => set(&mut m.auto_merge_source, v),
            _ => {}
        }
    }
    if m.fno_id.is_some() {
        m.session_id = m.fno_id.clone();
    }
    m
}

fn canonical_session_id(m: &ManifestFields) -> Result<Option<String>, &'static str> {
    let Some(session_id) = m
        .session_id
        .clone()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    if m.fno_id.is_some() && !crate::loopcheck::is_full_run_id(&session_id) {
        return Err("invalid canonical fno_id; refusing to finalize");
    }
    Ok(Some(session_id))
}

// ── idempotency ─────────────────────────────────────────────────────────────

/// Inspect prior `session_finalized` events for this session_id.
///
/// Returns:
/// - `Some(true)`  - a prior finalize completed a SHIP (stamp/graduate/handoff
///   ran); the session is fully done, nothing more to do on any later fire.
/// - `Some(false)` - a prior finalize completed but only the non-ship ledger
///   record (the always-branch); the ledger row exists, but the ship
///   side-effects have NOT run.
/// - `None`        - no completed finalize yet (or only `session_finalize_failed`,
///   which is intentionally not counted so a later fire retries).
///
/// A successful finalize is recorded ONLY when every attempted sub-step
/// succeeded, so a partially-failed prior run leaves this `None` and the next
/// fire retries. The `ship` flag distinguishes a non-ship terminal (Budget /
/// NoProgress / ...) from a real ship, so a session that terminated non-ship
/// first and then ships within the same session still runs its ship
/// side-effects on the ship fire (the lockout bug, sigma-review HIGH).
fn prior_finalize_ship(project_events: &Path, session_id: &str) -> Option<bool> {
    let content = fs::read_to_string(project_events).ok()?;
    let mut seen = None;
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("session_finalized")
            || val.pointer("/data/session_id").and_then(|v| v.as_str()) != Some(session_id)
        {
            continue;
        }
        let ship = val
            .pointer("/data/ship")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if ship {
            return Some(true); // a completed ship is terminal-complete
        }
        seen = Some(false);
    }
    seen
}

// ── a2a status-breakpoint run_summary (x-dbaf) ──────────────────────────────

/// Payload cap for the run_summary `data` object (mirrors events.rs
/// MAX_EVENT_PAYLOAD_BYTES). run_summary is lean by construction, but honoring
/// the cap keeps the Rust path's behavior identical to the daemon EventEmitter.
const RUN_SUMMARY_DATA_CAP: usize = 500;

/// Count the run's task ticks in events.jsonl. Correlates on the envelope-level
/// `run` (the target-run id), so a co-located second run's events never mix in.
/// tasks_failed counts task_done events whose outcome is FAILED - the gap
/// (tasks_started > tasks_done) is what exposes a crashed executor (AC2-FR).
fn count_run_tasks(project_events: &Path, run: &str) -> (u64, u64, u64) {
    use std::io::BufRead;
    let (mut started, mut done, mut failed) = (0u64, 0u64, 0u64);
    // Stream line-by-line and reuse one buffer: events.jsonl grows to the
    // rotation cap, so reading it whole would balloon memory (gemini review).
    if let Ok(file) = fs::File::open(project_events) {
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("run").and_then(|r| r.as_str()) == Some(run) {
                    match v.get("type").and_then(|t| t.as_str()) {
                        Some("task_started") => started += 1,
                        Some("task_done") => {
                            done += 1;
                            if v.get("outcome").and_then(|o| o.as_str()) == Some("FAILED") {
                                failed += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            line.clear();
        }
    }
    (started, done, failed)
}

/// Append a pre-built extended envelope through the shared Branch-A mutex.
/// Non-fatal: a write failure logs and returns, never wedging finalize.
fn append_envelope(path: &Path, envelope: &Value) {
    if let Err(error) =
        crate::claims::append_event_line(path, envelope, std::time::Duration::from_secs(2))
    {
        eprintln!(
            "finalize: run_summary write to {} failed: {error}",
            path.display()
        );
    }
}

/// Build + emit the run_summary terminal event to both event logs. Best-effort
/// throughout: emission never changes the exit code or holds session_finalized.
#[allow(clippy::too_many_arguments)]
fn emit_run_summary(
    project_events: &Path,
    global_events: &Path,
    run: &str,
    node: Option<&str>,
    ship: bool,
    reason: &str,
    pr_url: Option<&str>,
) {
    let (started, done, failed) = count_run_tasks(project_events, run);
    // Terminal reason -> return-contract outcome: a ship terminal is SUCCESS
    // (DONE_WITH_CONCERNS if any task failed); a non-ship terminal (Budget /
    // NoProgress / Interrupted) is FAILED.
    let outcome = if !ship {
        "FAILED"
    } else if failed > 0 {
        "DONE_WITH_CONCERNS"
    } else {
        "SUCCESS"
    };
    let mut data = json!({
        "tasks_started": started,
        "tasks_done": done,
        "tasks_failed": failed,
        "termination_reason": reason,
    });
    if let Some(url) = pr_url {
        data["pr_url"] = json!(url);
    }
    // Honor the payload cap (AC2-EDGE, Rust path): oversized data -> the small
    // meta-event, so an auditor sees the drop rather than a silently huge line.
    let payload_len = serde_json::to_string(&data).map(|s| s.len()).unwrap_or(0);
    if payload_len > RUN_SUMMARY_DATA_CAP {
        data = json!({"intended_kind": "run_summary", "size": payload_len});
    }
    let mut env = json!({
        "ts": now_rfc3339_utc(),
        "v": 1,
        "type": "run_summary",
        "source": "target",
        "run": run,
        "outcome": outcome,
        "data": data,
    });
    if let Some(n) = node {
        env["node"] = json!(n);
    }
    append_envelope(project_events, &env);
    if project_events != global_events {
        append_envelope(global_events, &env);
    }
}

/// Push leg for run_summary (x-dbaf): notify the parent handle. run_summary
/// emits natively above, so the push shells the Python resolver (`fno doctor event
/// push-parent`) rather than reimplementing registry lookup + mail in Rust.
/// Best-effort: a missing `fno` / no spawn lineage is a silent skip; the
/// events.jsonl line already landed independently (AC1-FR). `fno` (not a bare
/// interpreter) is safe to shell - a PATH miss just skips.
fn push_run_summary_to_parent(run: &str, node: Option<&str>, reason: &str) {
    let mut cmd = Command::new("fno");
    cmd.args([
        "doctor",
        "event",
        "push-parent",
        "--type",
        "run_summary",
        "--run",
        run,
        "--reason",
        reason,
    ]);
    if let Some(n) = node {
        cmd.args(["--node", n]);
    }
    if let Err(e) = cmd.output() {
        eprintln!("finalize: run_summary parent push skipped (non-fatal): {e}");
    }
}

// ── public entry ────────────────────────────────────────────────────────────

/// `fno-agents finalize ...`. Returns 0 for a completed write, 1 when generic
/// delivery must retry, and 2 for CLI misuse. Legacy side-effect failures stay
/// non-fatal.
pub fn run_finalize(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|a| a == "-h" || a == "--help" || a == "help")
    {
        println!("{HELP}");
        return 0;
    }
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("finalize: {msg}\n{HELP}");
            return 2;
        }
    };
    let (Some(state), Some(cwd), Some(reason)) = (a.state, a.cwd, a.reason) else {
        eprintln!("finalize: --state, --cwd and --reason are required\n{HELP}");
        return 2;
    };
    let classification = match classify_legacy(&reason) {
        Ok(record) => record,
        Err(error) => {
            eprintln!("finalize: {error}\n{HELP}");
            return 2;
        }
    };
    let predicates = classification.projection();
    let delivery_ship = predicates.delivery_ship;

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let project_events = a.events.unwrap_or_else(|| cwd.join(".fno/events.jsonl"));
    let global_events = a.global_events.unwrap_or_else(|| {
        home.clone()
            .unwrap_or_else(|| cwd.clone())
            .join(".fno/events.jsonl")
    });

    // Missing manifests are the non-fatal delegated-session path for legacy
    // reasons. Generic delivery must retry because its required writes cannot
    // be proven without the session-bound manifest.
    let content = match fs::read_to_string(&state) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "finalize: manifest {} unreadable ({e}); nothing to finalize (likely delegated/archived)",
                state.display()
            );
            return i32::from(delivery_ship);
        }
    };
    let m = parse_manifest_fields(&content);
    let Some(session_id) = (match canonical_session_id(&m) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("finalize: {error}");
            return 1;
        }
    }) else {
        eprintln!("finalize: manifest has no session_id; skipping (cannot dedup)");
        return i32::from(delivery_ship);
    };

    let legacy_ship = predicates.ship_reason;
    let ship = legacy_ship || delivery_ship;

    // Idempotency, ship-aware (sigma-review HIGH): a prior COMPLETED ship means
    // the whole session is done. A prior non-ship finalize means only the ledger
    // row exists; if THIS fire is also non-ship there is nothing new to do, but
    // if THIS fire is a SHIP it must still run the ship side-effects - a session
    // that hit a non-ship terminal (Budget / NoProgress) and then shipped within
    // the same session would otherwise never get stamped/graduated/handed off.
    let mut skip_ledger = false;
    match prior_finalize_ship(&project_events, &session_id) {
        Some(true) if shadow_run_needs_finalize_done(&cwd, &session_id) => {
            if record_finalize_done(&cwd, &session_id, &project_events, &global_events) {
                eprintln!("finalize: repaired shadow closure for previously finalized session {session_id}");
                return 0;
            }
            eprintln!("finalize: shadow closure still pending for previously finalized session {session_id}");
            return 1;
        }
        Some(true) => {
            eprintln!("finalize: session {session_id} already finalized (ship); early-return");
            return 0;
        }
        // `DoneAwaitingMerge` is not in SHIP_REASONS, so without this it would
        // early-return here and never reach the always-run tail. A session that
        // hits Budget and then resumes to DoneAwaitingMerge would silently lose
        // its do stamp - the same "correct wiring, missing coverage" failure this
        // backstop exists to fix. Everything downstream is idempotent.
        Some(false) if !ship && !predicates.do_stamp_terminal => {
            eprintln!(
                "finalize: session {session_id} ledger already recorded (non-ship); early-return"
            );
            return 0;
        }
        Some(false) => {
            // Ledger row already written by the prior non-ship finalize; skip the
            // redundant ledger step (register-task would dedup it anyway) and run
            // only the ship side-effects below.
            skip_ledger = true;
        }
        None => {}
    }

    // Transcript UUID for the cost/ledger scripts: prefer the manifest's
    // canonical claude_transcript_id, fall back to the --transcript basename.
    let transcript_uuid = m
        .claude_transcript_id
        .clone()
        .or_else(|| {
            a.transcript
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    let mut failed: Vec<String> = Vec::new();

    // ── ALWAYS: ledger session-record (skipped only when a prior non-ship
    //    finalize already wrote this session's row) ──────────────────────────
    let ledger_written = if skip_ledger {
        true // the prior non-ship finalize already wrote the row
    } else {
        match write_ledger_record(&cwd, &state, &transcript_uuid, &reason) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("finalize: ledger record failed: {e}");
                failed.push("ledger".into());
                false
            }
        }
    };

    // ── ALWAYS: rescue uncommitted work at the only terminal a worker gets
    //    (x-cdc7 HALF ONE) ──────────────────────────────────────────────────
    // A provider 429 (or any other hard stop) gives a worker no future
    // stop-hook fire to save it - this fire, whatever `reason` is, is the only
    // one it gets. A dirty worktree here is one GC/orphan sweep from gone
    // (measured: 950 insertions across 11 files, zero commits, rescued by
    // hand). `git commit` is cheap and reversible; losing the diff is not.
    let wip_commit_sha = commit_wip_if_dirty(&cwd, &reason);

    // ── SHIP ONLY: stamp (+ graduate for advisory) + handoff ───────────────
    // For a CODE ship (DonePRGreen) the plan is stamped `in_review` only: done now
    // means MERGED (x-f34f), and the `in_review -> done` flip happens at merge via
    // the write-time status projection. An ADVISORY/doc ship (DoneAdvisory) has
    // NO merge event - ship IS its completion - so it must still graduate to
    // `done` here, else the plan is stranded at `in_review` on the active board
    // (codex P2). expected_url_count is still recorded either way for the manual
    // `graduate` verb and the cross-project safety net.
    let mut stamped = false;
    let mut handoff_path: Option<String> = None;
    let mut delivery_terminal_message: Option<String> = None;
    if legacy_ship {
        let plan = m.plan_path.clone().unwrap_or_default();
        if !plan.is_empty() {
            let expected = derive_expected_url_count(&cwd, &plan, m.cross_project);
            // Graduate only for the merge-less advisory terminal; a cross-project
            // advisory still waits for a derivable count (never graduate early).
            let do_graduate = predicates.graduate && (!m.cross_project || expected.is_some());
            match stamp_and_graduate(&cwd, &plan, &session_id, expected, do_graduate, None) {
                Ok(()) => stamped = true,
                Err(step) => {
                    eprintln!("finalize: {step} failed");
                    failed.push(step);
                }
            }
        }
        match write_handoff(
            &cwd,
            &state,
            &session_id,
            &m,
            &transcript_uuid,
            a.handoffs_dir.as_deref(),
            a.settings.as_deref(),
            home.as_deref(),
        ) {
            Ok(p) => handoff_path = Some(p),
            Err(e) => {
                eprintln!("finalize: handoff failed: {e}");
                failed.push("handoff".into());
            }
        }

        // W6 verifier advisory (x-f063): AC-vs-diff verdict, recorded then
        // ignored. Log-only and never pushed to `failed` - an advisory must
        // never wedge the loop or hold session_finalized open for retry.
        // Events paths are forwarded so the module's per-session exactly-once
        // guard reads the same log finalize writes (a retried fire after a
        // partial failure must not double-emit or re-spend on a spawn).
        let mut adv = py_module(&cwd);
        adv.arg("-m")
            .arg("fno.verify_advise")
            .arg("--node-id")
            .arg(m.graph_node_id.as_deref().unwrap_or(""))
            .arg("--plan-path")
            .arg(m.plan_path.as_deref().unwrap_or(""))
            .arg("--session-id")
            .arg(&session_id)
            .arg("--reason")
            .arg(&reason)
            .arg("--events")
            .arg(&project_events)
            .arg("--global-events")
            .arg(&global_events);
        match adv.output() {
            Ok(out) if !out.status.success() => eprintln!(
                "finalize: verify_advise failed with exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Ok(out) => {
                // rc=0 is the module's contract even when its internals failed
                // (verifier died, event emit failed): forward any stderr so
                // those messages reach the stop hook's finalize log instead of
                // dying in a dead channel (sigma silent-failure P1).
                let err_raw = String::from_utf8_lossy(&out.stderr);
                let err = err_raw.trim();
                if !err.is_empty() {
                    eprintln!("finalize: verify_advise: {err}");
                }
            }
            Err(e) => eprintln!("finalize: verify_advise spawn failed: {e}"),
        }
    }

    if delivery_ship {
        match crate::delivery_completion::selected_receipt(
            &project_events,
            m.graph_node_id.as_deref(),
            &session_id,
        ) {
            Some(receipt) => {
                delivery_terminal_message =
                    Some(format!("generic delivery finalized via {}", receipt.uri));
                let plan = m.plan_path.clone().unwrap_or_default();
                if !plan.is_empty() {
                    let expected = derive_expected_url_count(&cwd, &plan, m.cross_project);
                    let do_graduate = !m.cross_project || expected.is_some();
                    match stamp_and_graduate(
                        &cwd,
                        &plan,
                        &session_id,
                        expected,
                        do_graduate,
                        Some(&receipt.uri),
                    ) {
                        Ok(()) => stamped = true,
                        Err(step) => failed.push(step),
                    }
                }
                let dir = resolve_handoffs_dir(
                    a.handoffs_dir.as_deref(),
                    a.settings.as_deref(),
                    &cwd,
                    home.as_deref(),
                );
                match crate::delivery_completion::write_receipt_handoff(&dir, &session_id, &receipt)
                {
                    Ok(path) => handoff_path = Some(path),
                    Err(error) => {
                        eprintln!("finalize: generic handoff failed: {error}");
                        failed.push("handoff".into());
                    }
                }
            }
            None => {
                eprintln!("finalize: selected delivery verdict event missing");
                failed.push("delivery_receipt".into());
            }
        }
    }

    // ── completion eval artifact, every terminal but NoWork (ab-1a92b677, x-8fc0) ──
    // Originally re-homed the BLOCKED-postmortem generator the control-plane
    // wedge dropped, gated to stuck terminals only: NoProgress/Budget/
    // Interrupted/Aborted. That gate made the autocorrect monthly review's
    // input a failure-only sample, and a failure-only sample writes rules -
    // it never confirms what a clean session did right, so every lesson
    // skewed toward caution nobody asked for (x-8fc0). `eval_should_fire`
    // now fires this for every terminal reason except NoWork (nothing to
    // evaluate). `write_postmortem` branches its body on STUCK_REASONS so a
    // stuck session still gets the failure-triage prose; every other reason
    // gets a lighter completion-eval prose pointing at the pre-promise
    // blocklist. Non-fatal and idempotent (filename keyed by date+session)
    // like every other sub-step.
    let mut postmortem_path: Option<String> = None;
    if eval_should_fire(&reason) {
        match write_postmortem(
            &cwd,
            &session_id,
            &m,
            &reason,
            a.transcript.as_deref(),
            a.postmortems_dir.as_deref(),
            a.settings.as_deref(),
            home.as_deref(),
        ) {
            Ok(p) => postmortem_path = Some(p),
            Err(e) => {
                eprintln!("finalize: postmortem failed: {e}");
                failed.push("postmortem".into());
            }
        }
    }

    // ── STUCK ONLY: file an unanswered operator question (x-32f3 HALF TWO) ──
    // A worker that idles on an unanswered question and then dies (measured:
    // 7h idle, "awaiting operator's terminal/mux info", `fno inbox outstanding`
    // empty the whole time) takes the diagnosis it already completed down
    // with it. The verb (`fno inbox outstanding ask`) already exists; nothing
    // forced the filing. Mechanical, not instructional: the same STUCK bucket
    // that gets a postmortem, with an operator-directed question in its last
    // message and nothing already filed for this session, gets it filed here.
    let mut outstanding_filed = false;
    if predicates.stuck {
        let question = a
            .transcript
            .as_deref()
            .and_then(last_assistant_text)
            .as_deref()
            .and_then(extract_operator_question);
        if let Some(q) = question {
            if !session_already_filed(&cwd, &session_id) {
                outstanding_filed = file_outstanding_question(&cwd, &q, m.graph_node_id.as_deref());
            }
        }
    }

    // ── bg worker terminal-stop marker (x-fcbf) ────────────────────────────
    // A fire-and-forget `claude --bg` /target|/think worker parks at its idle
    // prompt on a terminal loop decision and never exits (the stop hook allows
    // the TURN, not the PROCESS), piling up against agents.max_live. finalize
    // cannot self-exit (it is the worker's child), so it drops a marker the
    // external daemon sweep consumes to `claude stop` the parked worker. Gated
    // to footnote-SPAWNED (FNO_AGENT_SELF) + non-loop-driven (FNO_DRIVER_LIB
    // unset) sessions so an operator's own terminal /target and loop-run
    // children stay parked. Best-effort + log-only: never held for retry, never
    // rolls back the ledger/stamp (mirrors verify_advise's non-wedge contract).
    let agent_self = std::env::var_os("FNO_AGENT_SELF").is_some();
    let driver_lib = std::env::var_os("FNO_DRIVER_LIB").is_some();
    let mut terminal_stop_marked = false;
    if let Some(uuid) =
        crate::terminal_stop::should_mark(agent_self, driver_lib, m.claude_transcript_id.as_deref())
    {
        let agents_home = crate::paths::AgentsHome::from_env();
        match crate::terminal_stop::write_marker(&agents_home, uuid, &reason) {
            Ok(p) => {
                terminal_stop_marked = true;
                eprintln!("finalize: terminal-stop marker written: {}", p.display());
            }
            Err(e) => eprintln!("finalize: terminal-stop marker failed (non-fatal): {e}"),
        }
    }

    // ── a2a status-breakpoint run_summary (x-dbaf) ──────────────────────────
    // One per-run terminal summary carrying task counts + termination reason,
    // in the extended envelope. Best-effort; the pull leg (events.jsonl) is
    // authoritative and the push leg (task 1.4) rides it. gh is shelled for the
    // PR url only on a ship terminal.
    let run_summary_pr = if legacy_ship { gh_pr_url(&cwd) } else { None };
    emit_run_summary(
        &project_events,
        &global_events,
        &session_id,
        m.graph_node_id.as_deref(),
        ship,
        &reason,
        run_summary_pr.as_deref(),
    );
    push_run_summary_to_parent(&session_id, m.graph_node_id.as_deref(), &reason);

    // ── node<->PR pr_number backstop stamp (x-280d) ────────────────────────
    // Runs in the always-run tail (first fire of every reason), so it stamps
    // even a non-ship/awaiting-merge terminal that left an open PR. Non-fatal;
    // deliberately not returned into `failed`.
    if !delivery_ship {
        stamp_node_pr(&cwd, m.graph_node_id.as_deref());
    }

    // ── guarded do-provenance backstop (x-0469) ────────────────────────────
    // Same shape and same fatality as the stamp above: log-only, deliberately
    // not returned into `failed` (a guard skip must never wedge the loop).
    stamp_node_do(&cwd, &m, &reason);

    // ── arm auto-merge at the green gate, not at PR creation (x-1951) ──────
    // Last, so the plan stamp and both node<->PR stamps have already landed
    // before the merge is handed to GitHub. Same log-only fatality as the two
    // stamps above.
    let approved = m.auto_merge_approved.unwrap_or(false);
    let should_arm = should_arm_auto_merge(&reason, approved);
    let (auto_merge_armed, auto_merge_blocked_reason) = if should_arm {
        if !crate::agents_config::auto_merge_enabled(&cwd) {
            let blocked = "config.auto_merge.enabled=false".to_string();
            eprintln!("finalize: native auto-merge withheld: {blocked}");
            (false, Some(blocked))
        } else {
            match optional_review_block_reason(&cwd) {
                None => arm_auto_merge(&cwd),
                Some(blocked) => {
                    eprintln!("finalize: native auto-merge withheld: {blocked}");
                    (false, Some(blocked))
                }
            }
        }
    } else {
        (false, None)
    };
    // Without this, "approved but this terminal is ineligible" and "never
    // approved" are the same silence, and the event's `auto_merge_armed: false`
    // cannot tell them apart either.
    if approved && !should_arm {
        // x-0eaf: name the coverage reason when the autonomous path is declined.
        // The generic "not an arming terminal" hides WHY behind a vocabulary
        // term; an operator who armed auto-merge and sees it not fire needs to
        // learn the diff was unreviewed, not decode a terminal name.
        let why = if predicates.awaiting_review_notify {
            "the PR is unreviewed (coverage 0); review the diff then re-run, or merge by hand"
        } else {
            "not an arming terminal"
        };
        eprintln!("finalize: auto-merge approved but {reason}: {why}; not armed");
    }

    // ── emit terminal event ────────────────────────────────────────────────
    let mut data = json!({
        "session_id": session_id,
        "termination_reason": reason,
        "ship": ship,
        "ledger_written": ledger_written,
        "stamped": stamped,
        "handoff_path": handoff_path,
        "postmortem_path": postmortem_path,
        "terminal_stop_marked": terminal_stop_marked,
        "graph_node_id": m.graph_node_id,
        // Re-homed from `fno agents worker ship`'s return dict (x-1951): the fact now
        // belongs to the terminal that authorized it, not to PR creation.
        "auto_merge_armed": auto_merge_armed,
        // Provenance (x-9d11): `unknown` when the manifest predates the field,
        // so an operator reading the event can always tell WHICH layer set the
        // posture - or that the manifest cannot say.
        "auto_merge_source": m.auto_merge_source.as_deref().unwrap_or("unknown"),
        // x-cdc7 HALF ONE: the sha of the rescue commit, or null on a clean
        // tree / non-git dir / commit failure.
        "wip_commit_sha": wip_commit_sha,
        // x-32f3 HALF TWO: whether an operator question was auto-filed this fire.
        "outstanding_filed": outstanding_filed,
    });
    if let Some(blocked) = auto_merge_blocked_reason {
        data["auto_merge_blocked_reason"] = json!(blocked);
    }
    if delivery_ship && failed.is_empty() {
        let emitted = delivery_terminal_message.as_deref().is_some_and(|message| {
            crate::delivery_completion::emit_terminal(
                &project_events,
                &global_events,
                &session_id,
                message,
            )
        });
        if !emitted {
            failed.push("delivery_terminal".into());
        }
    }
    if failed.is_empty() {
        if record_finalize_done(&cwd, &session_id, &project_events, &global_events) {
            emit_to_both(&project_events, &global_events, "session_finalized", data);
        } else {
            failed.push("run_state_finalize_done".into());
            data["failed_steps"] = json!(failed);
            emit_to_both(
                &project_events,
                &global_events,
                "session_finalize_failed",
                data,
            );
        }
    } else {
        data["failed_steps"] = json!(failed);
        // session_finalized intentionally NOT emitted: a later fire retries the
        // failed step (each shelled helper is idempotent).
        emit_to_both(
            &project_events,
            &global_events,
            "session_finalize_failed",
            data,
        );
    }
    let should_retry = (delivery_ship && !failed.is_empty())
        || failed.iter().any(|step| step == "run_state_finalize_done");
    if should_retry {
        1
    } else {
        0
    }
}

fn shadow_run_needs_finalize_done(cwd: &Path, run: &str) -> bool {
    let path = cwd.join(".fno/run-log.jsonl");
    if !path.exists() {
        return false;
    }
    !matches!(
        crate::run_state::fold_run_state(&path, run),
        Ok(crate::run_state::RunState::Closed | crate::run_state::RunState::Aborted)
    )
}

fn record_finalize_done(
    cwd: &Path,
    run: &str,
    project_events: &Path,
    global_events: &Path,
) -> bool {
    let path = cwd.join(".fno/run-log.jsonl");
    if !path.exists() {
        return true;
    }
    if matches!(
        crate::run_state::fold_run_state(&path, run),
        Ok(crate::run_state::RunState::Closed | crate::run_state::RunState::Aborted)
    ) {
        return true;
    }
    crate::loopcheck::observe_shadow_transition(
        &path,
        run,
        crate::run_state::RunEvent::FinalizeDone,
        project_events,
        global_events,
    )
}

// ── ledger (always) ─────────────────────────────────────────────────────────

/// Build a `python3` command (for `-m <module>`) rooted at `cwd`, injecting the
/// repo's `cli/src` onto PYTHONPATH when running from a source checkout so the
/// in-package `fno.*` modules import without an installed/editable package
/// (codex PR #515 P1). When the stop hook resolves the checkout-built binary,
/// these children otherwise run with only `cwd` on `sys.path`, where `fno` is
/// not importable, so every terminal finalize silently failed to write the
/// ledger / stamp the plan. In an installed environment `cli/src` is not found
/// relative to the binary, PYTHONPATH is left untouched, and the installed
/// `fno` package is used.
fn py_module(cwd: &Path) -> Command {
    let mut cmd = Command::new(py_interpreter(cwd));
    cmd.current_dir(cwd);
    if let Some(src) = repo_cli_src(cwd) {
        let joined = match std::env::var_os("PYTHONPATH") {
            Some(prev) if !prev.is_empty() => {
                // APPEND (not prepend): cli/src is only a fallback that resolves
                // `fno` when nothing else does (the codex P1 source-checkout
                // case). An existing PYTHONPATH - a deliberate override, or the
                // finalize_e2e stub package - must keep precedence, so we add
                // cli/src AFTER it rather than shadowing it.
                let mut s = prev;
                s.push(":");
                s.push(&src);
                s
            }
            _ => std::ffi::OsString::from(&src),
        };
        cmd.env("PYTHONPATH", joined);
    }
    cmd
}

/// A `cli/.venv/bin/python3` under `root`, but ONLY when `root` is genuinely a
/// footnote source checkout (it also holds `cli/src/fno/__init__.py`). Gating on
/// the co-located package guards two cases (codex review on the x-b74b PR): a
/// foreign project `cwd` that happens to carry its own `cli/.venv` without `fno`
/// installed, and a mis-derived canonical root from a nonstandard git-dir
/// layout - either would otherwise hand back a venv where `import fno` fails and
/// silently regress ledger/stamp finalization.
fn footnote_venv(root: &Path) -> Option<String> {
    let venv = root.join("cli/.venv/bin/python3");
    if venv.is_file() && root.join("cli/src/fno/__init__.py").is_file() {
        return Some(venv.to_string_lossy().into_owned());
    }
    None
}

/// Locate `<repo>/cli/src` (the dir holding `cli/src/fno/__init__.py`). Anchored
/// on the target PROJECT (`cwd`), NOT the running binary: the DEPLOYED
/// fno-agents binary lives in `~/.local/bin`, whose ancestors hold no checkout,
/// so a `current_exe()` walk found neither `cli/src` nor `cli/.venv` and every
/// deployed-binary finalize dropped its ledger row (x-b74b). `cwd` is the
/// worktree, which tracks `cli/src`. Falls back to the canonical repo, then to a
/// `current_exe()` walk (checkout-built binary run outside its repo). `None`
/// when nothing resolves, so PYTHONPATH stays unset and an installed `fno` is
/// used.
fn repo_cli_src(cwd: &Path) -> Option<String> {
    for anc in cwd.ancestors() {
        if anc.join("cli/src/fno/__init__.py").is_file() {
            return Some(anc.join("cli/src").to_string_lossy().into_owned());
        }
    }
    if let Some(root) = crate::paths::canonical_repo_root(cwd) {
        if root.join("cli/src/fno/__init__.py").is_file() {
            return Some(root.join("cli/src").to_string_lossy().into_owned());
        }
    }
    let exe = std::env::current_exe().ok()?;
    for anc in exe.ancestors() {
        if anc.join("cli/src/fno/__init__.py").is_file() {
            return Some(anc.join("cli/src").to_string_lossy().into_owned());
        }
    }
    None
}

/// The interpreter finalize's Python helpers run under. In a source checkout,
/// prefer the repo's `cli/.venv` python: bare `python3` on PATH (e.g. Homebrew's
/// `/opt/homebrew/opt/python@3.x`) resolves the `fno` package off PYTHONPATH but
/// lacks fno's third-party deps (pydantic, ...), so `import fno.config` raised
/// ModuleNotFoundError and every terminal finalize logged `ledger record failed`
/// / `stamp failed` and wrote no termination_reason row. The venv has both fno
/// and its deps. PYTHONPATH entries still precede site-packages, so the
/// finalize_e2e stub package keeps precedence over the venv's installed `fno`.
/// Falls back to `python3` when no venv is found (installed-wheel or bare
/// environment). Anchored on `cwd`, then the CANONICAL repo, then a
/// `current_exe()` walk: a linked worktree has `cli/src` but NO `cli/.venv`, and
/// bare Homebrew `python3` lacks fno's deps (pydantic, ...), so a worktree
/// finalize must resolve the canonical checkout's venv (x-b74b) - PYTHONPATH
/// alone would still fail on the missing dep.
fn py_interpreter(cwd: &Path) -> String {
    for anc in cwd.ancestors() {
        if let Some(v) = footnote_venv(anc) {
            return v;
        }
    }
    if let Some(root) = crate::paths::canonical_repo_root(cwd) {
        if let Some(v) = footnote_venv(&root) {
            return v;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            if let Some(v) = footnote_venv(anc) {
                return v;
            }
        }
    }
    "python3".to_string()
}

/// Run `python3 -m fno.cost._session_cost` for cost, then
/// `python3 -m fno.cost._register` to append exactly one ledger row carrying
/// graph_node_id + provider_id + session_id + cost + termination_reason.
/// Dedup/flock stay in _register (proven). A missing transcript yields
/// cost=null - the row still lands (US7-ERR).
fn write_ledger_record(
    cwd: &Path,
    state: &Path,
    transcript_uuid: &str,
    reason: &str,
) -> Result<(), String> {
    // Cost JSON (best-effort: empty string -> register-task records cost=null).
    let cost_json = if transcript_uuid.is_empty() {
        String::new()
    } else {
        match py_module(cwd)
            .arg("-m")
            .arg("fno.cost._session_cost")
            .arg("--json")
            .arg(transcript_uuid)
            .output()
        {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            Ok(out) => {
                eprintln!(
                    "finalize: fno.cost._session_cost exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                String::new()
            }
            Err(e) => {
                eprintln!("finalize: fno.cost._session_cost spawn failed: {e}");
                String::new()
            }
        }
    };

    let mut cmd = py_module(cwd);
    cmd.arg("-m")
        .arg("fno.cost._register")
        .arg(state)
        .arg(transcript_uuid)
        .arg("--termination-reason")
        .arg(reason);
    if !cost_json.is_empty() {
        cmd.arg("--cost-json").arg(&cost_json);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("fno.cost._register spawn failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "fno.cost._register exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ── stamp (ship only) ────────────────────────────────────────────────────────

/// After a stamp writes, validate the plan's frontmatter via `fno do plan validate`
/// (the same read-only verb). Non-fatal-but-loud: a non-zero exit (e.g. a stamp
/// that left `status` unset) is reported on stderr for the next session to fix;
/// the stamp is never rolled back and finalize never fails on it (AC1-FR). A
/// concurrent edit is fine - the verb reads whatever snapshot is on disk.
fn validate_stamped_frontmatter(cwd: &Path, plan_path: &str) {
    if plan_path.is_empty() {
        return;
    }
    let full = cwd.join(plan_path);
    match py_module(cwd)
        .arg("-m")
        .arg("fno.cli")
        .arg("do")
        .arg("plan")
        .arg("validate")
        .arg(&full)
        .output()
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => eprintln!(
            "finalize: post-stamp `fno do plan validate` FAILED (exit {:?}); stamp NOT rolled back - fix the plan frontmatter next session:\n{}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => eprintln!("finalize: post-stamp `fno do plan validate` spawn failed: {e}"),
    }
}

/// Stamp the plan `in_review` and, when `do_graduate`, flip it to `done`.
///
/// For a CODE ship (`DonePRGreen`) the caller passes `do_graduate = false`: done
/// means merged (x-f34f), so the `in_review -> done` flip happens later via the
/// write-time projection at merge, not here. For an ADVISORY ship
/// (`DoneAdvisory`) there is no merge event, so the caller passes `true` and
/// this graduates the plan now. `expected_url_count` is recorded either way for
/// the manual `graduate` verb and the cross-project safety net.
fn stamp_and_graduate(
    cwd: &Path,
    plan_path: &str,
    session_id: &str,
    expected_url_count: Option<u32>,
    do_graduate: bool,
    url_override: Option<&str>,
) -> Result<(), String> {
    let pr_url = url_override.map(str::to_owned).or_else(|| gh_pr_url(cwd));
    let mut stamp = py_module(cwd);
    stamp
        .arg("-m")
        .arg("fno.plan._stamp")
        .arg("stamp")
        .arg("--plan-path")
        .arg(plan_path)
        .arg("--session-id")
        .arg(session_id);
    if let Some(n) = expected_url_count {
        stamp.arg("--expected-url-count").arg(n.to_string());
    }
    if let Some(url) = &pr_url {
        stamp.arg("--url").arg(url);
    }
    let out = stamp.output().map_err(|_| "stamp".to_string())?;
    if !out.status.success() {
        eprintln!(
            "finalize: fno.plan._stamp stamp exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Err("stamp".into());
    }

    // Post-stamp schema check (AC1-FR): validate the freshly-stamped frontmatter
    // against fno.plan.schema. Non-fatal-but-loud - a schema-invalidating stamp
    // is surfaced for the next session, never rolled back and never failing
    // finalize (finalize is idempotent/non-fatal by design).
    validate_stamped_frontmatter(cwd, plan_path);

    // A code ship stamps `in_review` only; done flips at merge via the projection.
    if !do_graduate {
        return Ok(());
    }

    let out = py_module(cwd)
        .arg("-m")
        .arg("fno.plan._stamp")
        .arg("graduate")
        .arg("--plan-path")
        .arg(plan_path)
        .output()
        .map_err(|_| "graduate".to_string())?;
    if !out.status.success() {
        eprintln!(
            "finalize: fno.plan._stamp graduate exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Err("graduate".into());
    }
    Ok(())
}

/// Derive the expected URL count for graduation. Returns `None` for a
/// single-project plan (let fno.plan._stamp keep any declared count, else
/// default to 1) and `Some(n)` for a cross-project plan, counting the direct keys under
/// the plan's frontmatter `projects:` map. Returns `None` for a cross-project
/// plan whose count can't be read (missing/garbled projects map) so the caller
/// can skip graduate rather than guess. This restores the pre-promise contract:
/// cross-project graduation waits for ALL project PRs (codex P1).
fn derive_expected_url_count(cwd: &Path, plan_path: &str, cross_project: bool) -> Option<u32> {
    if !cross_project {
        return None;
    }
    let doc = cwd.join(plan_path);
    let content = fs::read_to_string(&doc).ok()?;

    let mut in_fm = false;
    let mut in_projects = false;
    let mut child_indent: Option<usize> = None;
    let mut count: u32 = 0;
    for line in content.lines() {
        let t = line.trim();
        if t == "---" {
            if !in_fm {
                in_fm = true;
                continue;
            }
            break; // end of frontmatter
        }
        if !in_fm {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if !in_projects {
            if indent == 0 && t.starts_with("projects:") {
                in_projects = true;
            }
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if indent == 0 {
            break; // next top-level frontmatter key ends the projects map
        }
        match child_indent {
            None => {
                child_indent = Some(indent);
                count += 1;
            }
            Some(ci) if indent == ci => count += 1,
            _ => {} // deeper-nested key under a project entry; not a project
        }
    }
    if count >= 1 {
        Some(count)
    } else {
        None
    }
}

// ── mechanical handoff artifact (ship only) ──────────────────────────────────

/// Write a git-derived end-of-session summary to the persistent handoffs dir.
/// Filename keyed by session-id so a re-run overwrites rather than duplicating.
#[allow(clippy::too_many_arguments)]
fn write_handoff(
    cwd: &Path,
    state: &Path,
    session_id: &str,
    m: &ManifestFields,
    transcript_uuid: &str,
    handoffs_override: Option<&Path>,
    settings_override: Option<&Path>,
    home: Option<&Path>,
) -> Result<String, String> {
    let dir = resolve_handoffs_dir(handoffs_override, settings_override, cwd, home);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let date = &now_rfc3339_utc()[..10]; // YYYY-MM-DD
    let sid_prefix: String = session_id.chars().take(16).collect();
    let file = dir.join(format!("{date}-{sid_prefix}-handoff.md"));

    let title = m.input.clone().unwrap_or_else(|| "Untitled".into());
    let plan = m.plan_path.clone().unwrap_or_else(|| "-".into());
    let node = m.graph_node_id.clone().unwrap_or_else(|| "-".into());
    let pr = gh_pr_url(cwd).unwrap_or_else(|| "-".into());
    let diffstat = git_capture(cwd, &["diff", "--stat", "origin/main...HEAD"])
        .filter(|s| !s.trim().is_empty())
        .or_else(|| git_capture(cwd, &["diff", "--stat", "HEAD~5..HEAD"]))
        .unwrap_or_else(|| "(diff unavailable)".into());
    let commits = git_capture(cwd, &["log", "--oneline", "origin/main..HEAD"])
        .filter(|s| !s.trim().is_empty())
        .or_else(|| git_capture(cwd, &["log", "--oneline", "-10"]))
        .unwrap_or_else(|| "(log unavailable)".into());
    let cost = handoff_cost_line(cwd, transcript_uuid);
    // Completed commit + idempotency keys (x-c3a2): a worker that died after
    // shipping but before the terminal journal write is reconcilable from this
    // artifact. The keys are derived from the delivered HEAD so a resumed worker
    // checking them skips a replayed publish/PR-create/comment/merge.
    let head = git_capture(cwd, &["rev-parse", "HEAD"]).unwrap_or_else(|| "-".into());
    let head_short: String = head.chars().take(7).collect();

    let body = format!(
        "# Session handoff: {title}\n\n\
         - session: `{session_id}`\n\
         - node: `{node}`\n\
         - plan: `{plan}`\n\
         - PR: {pr}\n\
         - completed_commit: `{head}`\n\
         - idempotency_keys: `pr_create:{head_short}`, `merge:{head_short}`\n\
         - cost: {cost}\n\
         - generated: {generated} (mechanical, by `fno-agents finalize`)\n\n\
         ## Files changed (origin/main...HEAD)\n\n```\n{diffstat}\n```\n\n\
         ## Commits\n\n```\n{commits}\n```\n",
        generated = now_rfc3339_utc(),
    );

    // Keep the manifest path referenced so the variable is meaningful even when
    // we add fields later; it is the canonical source of the fields above.
    let _ = state;
    fs::write(&file, body).map_err(|e| format!("write {}: {e}", file.display()))?;
    Ok(file.to_string_lossy().into_owned())
}

/// One-line cost summary for the handoff header, sourced from the in-package
/// _session_cost module (`python3 -m fno.cost._session_cost`).
fn handoff_cost_line(cwd: &Path, transcript_uuid: &str) -> String {
    if transcript_uuid.is_empty() {
        return "(unavailable)".into();
    }
    // Route through py_module so this shares the interpreter + PYTHONPATH
    // resolution used by the ledger write; a raw `python3` here (no PYTHONPATH,
    // no venv) was the source of the recurring `handoff cost: ... exit` errors.
    match py_module(cwd)
        .arg("-m")
        .arg("fno.cost._session_cost")
        .arg("--json")
        .arg(transcript_uuid)
        .output()
    {
        Ok(out) if out.status.success() => {
            match serde_json::from_slice::<Value>(&out.stdout) {
                Ok(v) => match v.get("cost_usd").and_then(|c| c.as_f64()) {
                    Some(c) => format!("${c:.2}"),
                    None => "(unavailable)".into(),
                },
                Err(e) => {
                    // Surface a crashed/garbage cost module (mirrors
                    // write_ledger_record): a reader must tell "no transcript"
                    // from "fno.cost._session_cost emitted non-JSON".
                    eprintln!(
                        "finalize: handoff cost: fno.cost._session_cost emitted non-JSON: {e}"
                    );
                    "(unavailable)".into()
                }
            }
        }
        Ok(out) => {
            eprintln!(
                "finalize: handoff cost: fno.cost._session_cost exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            "(unavailable)".into()
        }
        Err(e) => {
            eprintln!("finalize: handoff cost: fno.cost._session_cost spawn failed: {e}");
            "(unavailable)".into()
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Resolve the persistent handoffs directory:
///   1. explicit `--handoffs-dir`
///   2. `$HANDOFFS_DIR`
///   3. `config.paths.handoffs_dir` from project then global settings.yaml,
///      with `~` and `{project}` expanded (skipped if it still has `{...}`)
///   4. vault-derived `<vault>/internal/<project>/handoffs/` when
///      `obsidian.enabled` + `obsidian.vault` are set (placement rule,
///      ab-f063 Wave 2 - mirrors `paths.handoffs_dir()` in the Python CLI)
///   5. fallback `~/.fno/handoffs/<project>`
///
/// Pure-Rust resolution: it never shells `fno`, so the verb keeps its Python-CLI
/// independence (it only ever runs the in-package metric modules via
/// `python3 -m`).
fn resolve_handoffs_dir(
    override_dir: Option<&Path>,
    settings_override: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(d) = override_dir {
        return d.to_path_buf();
    }
    if let Some(d) = env_dir_unless_null("HANDOFFS_DIR") {
        return d;
    }
    let project = resolve_project_name(settings_override, home, cwd);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(s) = settings_override {
        candidates.push(s.to_path_buf());
    }
    candidates.push(cwd.join(".fno/config.toml"));
    if let Some(h) = home {
        candidates.push(h.join(".fno/config.toml"));
    }
    for sp in &candidates {
        if let Some(raw) = read_path_setting(sp, "handoffs_dir") {
            if let Some(expanded) = expand_handoffs_template(&raw, home, &project) {
                return expanded;
            }
        }
    }
    if let Some(vault) = resolve_obsidian_vault(&candidates) {
        if let Some(vroot) = resolve_vault_root(&vault, home) {
            return vroot.join("internal").join(&project).join("handoffs");
        }
    }
    let base = home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    base.join(".fno/handoffs").join(project)
}

/// One file's `obsidian:` block, keyed per-field so a caller can merge across
/// project/global candidates the same way `fno.config._deep_merge` merges
/// settings.yaml: per KEY, not per file. `None` in either field means that
/// file's `obsidian:` block (if any) did not set that key, so the caller
/// should keep looking in the next, lower-priority candidate - NOT that the
/// key is false/absent overall (codex review, PR #185: a project file that
/// sets only `enabled: false` must still inherit `vault:` from global, and
/// must NOT let its own absence of an opinion fall through to a lower-priority
/// file that re-enables obsidian).
#[derive(Default)]
struct ObsidianBlock {
    enabled: Option<bool>,
    vault: Option<String>,
}

/// Parse a config.toml file into a table; None on a missing or unparseable file.
fn load_config_toml(path: &Path) -> Option<toml::Table> {
    fs::read_to_string(path).ok()?.parse::<toml::Table>().ok()
}

/// A dotted string value from a config.toml table (e.g. `["paths","handoffs_dir"]`).
fn toml_string_at(t: &toml::Table, path: &[&str]) -> Option<String> {
    let mut cur = t.get(*path.first()?)?;
    for k in &path[1..] {
        cur = cur.as_table()?.get(*k)?;
    }
    cur.as_str().map(str::to_string)
}

/// Read the `[obsidian]` block (enabled + vault) from a flat config.toml. `None`
/// in a field means that file did not set the key, so the caller keeps looking
/// in the next candidate (per-KEY merge, not per-file).
fn read_obsidian_block(path: &Path) -> ObsidianBlock {
    let Some(t) = load_config_toml(path) else {
        return ObsidianBlock::default();
    };
    let ob = t.get("obsidian").and_then(|v| v.as_table());
    ObsidianBlock {
        enabled: ob.and_then(|o| o.get("enabled")).and_then(|v| v.as_bool()),
        vault: ob
            .and_then(|o| o.get("vault"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
            .map(str::to_string),
    }
}

/// Resolve `obsidian.enabled` + `obsidian.vault`, merged key-by-key across
/// `candidates` in priority order (project before global) - matching
/// `fno.config._deep_merge` semantics, NOT "first file with an opinion wins
/// wholesale". Returns the vault name only when the merged `enabled` is true
/// AND a `vault` value was found somewhere in the chain.
fn resolve_obsidian_vault(candidates: &[PathBuf]) -> Option<String> {
    let mut enabled: Option<bool> = None;
    let mut vault: Option<String> = None;
    for sp in candidates {
        let block = read_obsidian_block(sp);
        if enabled.is_none() {
            enabled = block.enabled;
        }
        if vault.is_none() {
            vault = block.vault;
        }
        if enabled.is_some() && vault.is_some() {
            break;
        }
    }
    if enabled == Some(true) {
        vault
    } else {
        None
    }
}

/// Expand a vault name to its filesystem root - mirrors `paths.vault_root()`:
/// a bare name (e.g. `c3po`) maps to `~/c3po`; an already-absolute or
/// `~`-prefixed value is honored as-is.
fn resolve_vault_root(vault: &str, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = vault.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    if vault == "~" {
        return home.map(Path::to_path_buf);
    }
    if Path::new(vault).is_absolute() {
        return Some(PathBuf::from(vault));
    }
    home.map(|h| h.join(vault))
}

/// Read a `<key>:` path value from a settings.yaml (any indent level). The
/// `config.paths.*` keys (`handoffs_dir`, `postmortems_dir`, ...) are
/// distinctive enough that a flat scan is safe.
/// Read a dir from an env var, treating an empty or literal-"null" value as
/// unset. emit_shell never emits "null", but a stale/hand-edited environment
/// can, and trusting it verbatim is what wrote `./null/` inside the repo
/// (x-54c2). Mirrors the same guard in read_path_setting.
fn env_dir_unless_null(key: &str) -> Option<PathBuf> {
    let v = std::env::var_os(key)?;
    // Only the string-decodable "null"/empty sentinel is filtered; a non-UTF-8
    // value (valid arbitrary-byte path on Unix) is preserved verbatim, matching
    // the original var_os behavior (gemini review).
    if let Some(s) = v.to_str() {
        let t = s.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("null") {
            return None;
        }
        return Some(PathBuf::from(t));
    }
    Some(PathBuf::from(v))
}

/// Read a `paths.<key>` value (e.g. `handoffs_dir`, `postmortems_dir`) from a
/// flat config.toml. A literal `"null"` string is treated as absent (the "use
/// default" sentinel), so the caller falls through to `~/.fno/<dir>` (x-54c2).
fn read_path_setting(path: &Path, key: &str) -> Option<String> {
    let t = load_config_toml(path)?;
    toml_string_at(&t, &["paths", key]).filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("null"))
}

/// Expand `~` and `{project}` in a handoffs_dir template. Returns None when the
/// result still contains an unresolved `{...}` token (e.g. `{vault}`), so the
/// caller falls back rather than writing to a literal-brace path.
fn expand_handoffs_template(raw: &str, home: Option<&Path>, project: &str) -> Option<PathBuf> {
    let mut s = raw.to_string();
    // Cannot expand a leading ~ without a home; return None so the caller falls
    // back to the default dir rather than writing to a literal "~..." path
    // (gemini review).
    if let Some(stripped) = s.strip_prefix("~/") {
        let h = home?;
        s = h.join(stripped).to_string_lossy().into_owned();
    } else if s == "~" {
        let h = home?;
        s = h.to_string_lossy().into_owned();
    }
    s = s.replace("{project}", project);
    if s.contains('{') {
        return None;
    }
    Some(PathBuf::from(s))
}

/// Project name = basename of the MAIN worktree (the first `git worktree list
/// --porcelain` entry), so a linked worktree resolves to "fno", not the
/// worktree directory name. The porcelain first-entry is robust across layouts
/// (--separate-git-dir, bare) where the `--git-common-dir` parent is wrong
/// (gemini review HIGH). Falls back to the cwd basename.
fn repo_project_name(cwd: &Path) -> String {
    if let Some(porcelain) = git_capture(cwd, &["worktree", "list", "--porcelain"]) {
        if let Some(path_str) = porcelain
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("worktree "))
        {
            if let Some(name) = Path::new(path_str.trim()).file_name() {
                return name.to_string_lossy().into_owned();
            }
        }
    }
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into())
}

/// Last path segment of a git remote URL, one trailing `.git` stripped. Mirrors
/// the Python `_remote_url_to_slug` (paths.py): takes the last `/`-or-`:` segment
/// so scp-like (`git@host:org/repo.git`), https, and local-path remotes all
/// resolve. Returns None for an empty URL or a degenerate segment that would
/// escape `internal/<project>/`.
fn slug_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return None;
    }
    let tail = url.rsplit(['/', ':']).next()?;
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    // A Windows-style/local remote (`C:\repos\foo.git`) leaves backslashes in the
    // tail; reject any separator so the slug can never become a stray path
    // segment, matching the Python `_remote_url_to_slug` (paths.py).
    if tail.is_empty() || tail == "." || tail == ".." || tail.contains(['/', '\\']) {
        return None;
    }
    Some(tail.to_string())
}

/// Full repository identity `host/owner/repo` from a git remote URL, lowercased.
///
/// Distinct from `slug_from_remote_url`, which yields only the last path segment
/// and is a PATH TOKEN (`internal/<project>/`) where collisions are cosmetic.
/// This one keys `review_coverage` events in the cross-project global log, where
/// a collision is a correctness hole: `org-a/widget` and `org-b/widget` both
/// reduce to `widget`, so with a shared PR number one repo's coverage can
/// satisfy the other's auto-merge review guard - and a fork shares head SHAs,
/// which defeats the staleness check that would otherwise catch it.
///
/// Normalizes the forms git accepts so every clone of one repo agrees: scheme,
/// credentials, and port are stripped, `:` after the host is treated as the
/// path separator (scp form), one trailing `.git` and trailing slashes go, and
/// the result is lowercased (hosts are case-insensitive and the forge treats
/// owner/repo that way). Requires at least three segments (host + two path
/// parts), so a local or degenerate remote returns None rather than a key that
/// could alias another repo. Mirrored by `repo_identity_from_remote_url` in
/// `fno/paths.py`; the two MUST agree or the reader stops finding the writer's
/// events.
pub(crate) fn repo_identity_from_remote_url(url: &str) -> Option<String> {
    let mut s = url.trim().trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    if let Some(idx) = s.find("://") {
        s = &s[idx + 3..];
    }
    if let Some(idx) = s.find('@') {
        s = &s[idx + 1..];
    }
    // Split host from path on the first `/` or `:` (scp form uses `:`).
    let (host_port, path) = match s.find(['/', ':']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => return None,
    };
    // `ssh://host:22/o/r` leaves a numeric port glued to the host; drop it, but
    // never drop a non-numeric suffix that is part of the host.
    let host = match host_port.split_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host_port,
    };
    // An scp remote may still carry a leading port segment (`host:22/o/r`).
    let path = path.trim_start_matches('/');
    let path = match path.split_once('/') {
        Some((first, rest)) if !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) => {
            rest
        }
        _ => path,
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if host.is_empty() || path.contains('\\') {
        return None;
    }
    let segments: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if segments.len() < 2 || segments.iter().any(|p| *p == "." || *p == "..") {
        return None;
    }
    Some(format!("{}/{}", host, segments.join("/")).to_lowercase())
}

/// `host/owner/repo` for `cwd` - stable across worktrees and clones, and unique
/// across repos. Best-effort: any git failure or missing remote returns None,
/// and the caller then omits the field so no reader can claim the event.
pub(crate) fn repo_identity_from_git_remote(cwd: &Path) -> Option<String> {
    let url = git_capture(cwd, &["config", "--get", "remote.origin.url"])?;
    repo_identity_from_remote_url(&url)
}

/// `remote.origin.url` slug for `cwd` - stable across worktrees and clones.
/// Best-effort: any git failure or missing remote returns None so the caller
/// falls through to the basename.
fn slug_from_git_remote(cwd: &Path) -> Option<String> {
    let url = git_capture(cwd, &["config", "--get", "remote.origin.url"])?;
    slug_from_remote_url(&url)
}

/// Resolve the `{project}` path token, matching the Python resolver
/// `fno.paths._project_name`: `config.project.id` (project-local then global
/// settings.yaml) -> git-remote slug (stable across worktrees/clones) ->
/// git main-worktree basename via `repo_project_name`. The remote-slug tier is
/// load-bearing for parity: the Python side writes `internal/<remote-slug>/` for
/// an id-unset repo, so this terminal handoff writer must agree or it recreates
/// the very `internal/<basename>/` strays the Python change removes. Non-fatal:
/// a missing/malformed settings file, an unset/`null` id, or no remote degrades
/// to the basename, so unconfigured installs never break. Uses the SAME
/// project-then-global candidate order the callers use for `config.paths.*_dir`.
fn resolve_project_name(
    settings_override: Option<&Path>,
    home: Option<&Path>,
    cwd: &Path,
) -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(s) = settings_override {
        candidates.push(s.to_path_buf());
    }
    candidates.push(cwd.join(".fno/config.toml"));
    if let Some(h) = home {
        candidates.push(h.join(".fno/config.toml"));
    }
    for sp in candidates {
        if let Some(id) = read_project_id(&sp) {
            return id;
        }
    }
    if let Some(slug) = slug_from_git_remote(cwd) {
        return slug;
    }
    repo_project_name(cwd)
}

/// Read the project id from a flat config.toml (`[project]\nid = "..."`). The
/// legacy top-level `project.id` and the canonical `config.project.id` both map
/// to the same flat `project.id`, so one lookup covers both. An empty/`null`
/// value, an unreadable file, or an id outside `[A-Za-z0-9._-]` yields None so
/// the caller falls back to the basename.
fn read_project_id(path: &Path) -> Option<String> {
    let t = load_config_toml(path)?;
    let id = toml_string_at(&t, &["project", "id"])?;
    // The Python settings model rejects ids outside [A-Za-z0-9._-]
    // (config/__init__.py). A hand-edited invalid value (e.g. `foo/bar`) must
    // never be spliced into a `{project}` path segment, so degrade to the
    // basename rather than write artifacts outside the project dir.
    valid_project_id(&id).then_some(id)
}

/// Project ids are restricted to `[A-Za-z0-9._-]`, matching the Python
/// `validate_project_id` regex. ASCII byte check (no `regex` dependency).
fn valid_project_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Best-effort PR metadata for the current HEAD/branch through the REST reader.
fn pr_info(cwd: &Path, number: Option<u64>) -> Option<Value> {
    let mut command = Command::new("fno");
    command.args(["do", "pr", "info"]);
    if let Some(number) = number {
        command.arg(number.to_string());
    }
    let out = command.current_dir(cwd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Best-effort PR URL for the current HEAD/branch through REST.
fn gh_pr_url(cwd: &Path) -> Option<String> {
    let url = pr_info(cwd, None)?.get("url")?.as_str()?.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// Resolve the branch's open PR as `(number, url)` for the node<->PR backstop
/// stamp (x-280d). Returns None when gh fails/rate-limits, no PR exists, or the
/// JSON is malformed - all of which the caller treats as "nothing to stamp".
pub(crate) fn gh_pr_ref(cwd: &Path) -> Option<(u64, String)> {
    let payload = pr_info(cwd, None)?;
    parse_pr_ref(&serde_json::to_vec(&payload).ok()?)
}

/// Pure parse of REST `fno do pr info` output, with the legacy GraphQL field kept
/// for callers carrying cached payloads.
fn parse_pr_ref(stdout: &[u8]) -> Option<(u64, String)> {
    let v: Value = serde_json::from_slice(stdout).ok()?;
    let number = v.get("pr").or_else(|| v.get("number"))?.as_u64()?;
    let url = v.get("url")?.as_str()?.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some((number, url))
    }
}

/// Deterministic node<->PR `pr_number` backstop (x-280d): the create-time skill
/// stamp (pr-creator §5.5) is best-effort and was skipped for x-1829/#358,
/// leaving `pr_number` null so the derived `in_review` status never engaged.
/// Gated on node-presence + PR-exists (NOT `ship`) so `DoneAwaitingMerge` - the
/// exact terminal `in_review` covers - is included. Best-effort + non-fatal +
/// idempotent: never returned into `failed`, never changes the exit code; a
/// re-stamp of the same value is a no-op via `fno backlog update`'s lock.
fn stamp_node_pr(cwd: &Path, node: Option<&str>) {
    let Some(node) = node else { return };
    let Some((number, url)) = gh_pr_ref(cwd) else {
        eprintln!("finalize: no open PR found for branch; skipped pr_number stamp for node {node}");
        return;
    };
    let ok = Command::new("fno")
        .args([
            "backlog",
            "update",
            node,
            "--pr-number",
            &number.to_string(),
            "--pr-url",
            &url,
        ])
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        eprintln!("finalize: stamped pr_number {number} on node {node}");
    } else {
        eprintln!("finalize: pr_number stamp failed for node {node} (non-fatal)");
    }
}

/// Whether this terminal fire should arm GitHub's native auto-merge (x-1951).
///
/// Auto-merge used to be armed by `fno agents worker ship` at PR-CREATION time, gated
/// only on the manifest's posture. That pre-authorized a merge before any gate
/// had run: from the moment `--auto` is set GitHub owns the timing and fires the
/// instant ITS OWN branch protections pass, so footnote is no longer in the
/// decision path and a reviewer who posts a blocking finding after CI greens
/// loses the race (the PR #566 shape). Arming here instead authorizes exactly
/// the state `loop-check` just verified - PR up, CI green, no unaddressed
/// blocking finding - and buys every reviewer the whole CI duration to post
/// before the merge is armed at all.
///
/// `DonePRGreen` only. `DoneAdvisory` is the other `SHIP_REASONS` member but is
/// a doc ship with no PR, and `DoneAwaitingMerge` is by definition a merge a
/// human performs past pre-existing main-red - arming either would merge
/// something no gate greened.
fn should_arm_auto_merge(reason: &str, auto_merge_approved: bool) -> bool {
    auto_merge_approved
        && classify_legacy(reason)
            .map(|record| record.projection().merge_armable)
            .unwrap_or(false)
}

/// Return why configured optional-review evidence forbids native auto-merge,
/// or `None` when arming may proceed.
///
/// x-0eaf: coverage is the authority. When loop-check emitted a covered
/// `review_coverage` event, a local lane reviewed the diff and a quota-refused
/// or silent optional App no longer withholds (that recreates the wedge this
/// node exists to escape). Without such an event (e.g. finalize run with no
/// prior loop-check fire), the per-app check below is the fallback: a missing
/// or usage-limited optional App still withholds so GitHub does not merge
/// without the review coverage the operator configured.
///
/// The per-app check itself is `loopcheck::bot_verdict`, the ONE per-bot
/// predicate (round 3, PR 917): this scan used to read review objects only,
/// with no commit pin and no clean-pass lane, so an optional App whose pass is
/// a pinned clean-pass comment (the shape loop-check and the Python gates
/// already read) withheld arming forever - three readers, three answers. A
/// verdict on an older commit now reads Stale and withholds as
/// `optional-review-stale` rather than arming: arming on a review of a commit
/// that is no longer HEAD re-arms exactly the post-green race (PR #566 shape)
/// this gate moved to the terminal fire to escape.
fn optional_review_block_reason(cwd: &Path) -> Option<String> {
    let optional_apps = crate::agents_config::review_optional_apps(cwd);
    if optional_apps.is_empty() {
        return None;
    }
    // x-0eaf finding 1 (retraction): coverage bypasses ONLY the REFUSED case
    // (quota-limited), NOT the ABSENT case. An absent optional App (nothing
    // posted) must keep withholding, because unaddressed_findings is empty
    // (built from POSTED comments only) and cannot wait for a review that does
    // not exist yet.
    let coverage_satisfied = coverage_satisfied_in_latest_event(cwd);

    let output = match Command::new("fno-gh-coverage")
        .args([
            "pr",
            "view",
            "--json",
            "reviews,comments,headRefOid,baseRefName",
        ])
        .current_dir(cwd)
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!(
                "finalize: optional-review evidence read failed (non-fatal): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return Some("optional-review-read-failed".to_string());
        }
        Err(error) => {
            eprintln!("finalize: optional-review evidence read failed (non-fatal): {error}");
            return Some("optional-review-read-failed".to_string());
        }
    };

    let payload: Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("finalize: optional-review evidence parse failed (non-fatal): {error}");
            return Some("optional-review-read-failed".to_string());
        }
    };
    let Some(reviews) = payload.get("reviews").and_then(Value::as_array) else {
        return Some("optional-review-read-failed".to_string());
    };
    let Some(comments) = payload.get("comments").and_then(Value::as_array) else {
        return Some("optional-review-read-failed".to_string());
    };
    // Freshness resolves against the PR head gh reports, the same pin the
    // coverage gate uses. An exact-head pin is Fresh with no git call; a moved
    // head needs the repo's git, and an unresolvable identity reads Stale -
    // fail closed, never an arming on unpinned evidence.
    let head = payload
        .get("headRefOid")
        .and_then(Value::as_str)
        .unwrap_or("");
    let base = payload
        .get("baseRefName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let resolver = crate::loopcheck::FreshnessResolver::new("git", cwd, base, head);

    for app in optional_apps {
        let (verdict, _, _) =
            crate::loopcheck::bot_verdict(&app, reviews, comments, &|sha| resolver.freshness(sha));
        match verdict {
            crate::loopcheck::CoverageVerdict::Reviewed => continue,
            crate::loopcheck::CoverageVerdict::Refused => {
                // x-0eaf finding 1 (retraction): a REFUSED optional App bypasses
                // the withhold ONLY when coverage is satisfied (a local lane
                // reviewed). Without coverage, the refused bot still withholds.
                if coverage_satisfied {
                    continue;
                }
                return Some(format!("optional-review-usage-limited:{app}"));
            }
            crate::loopcheck::CoverageVerdict::Stale => {
                return Some(format!("optional-review-stale:{app}"));
            }
            _ => return Some(format!("optional-review-outstanding:{app}")),
        }
    }

    None
}

/// Whether the latest `review_coverage` event in the project log reports
/// coverage satisfied (covered, count > 0). Used to defer the optional-app
/// withhold to the coverage authority (x-0eaf). Missing/unreadable -> false
/// (fall back to the per-app check).
fn coverage_satisfied_in_latest_event(cwd: &Path) -> bool {
    let path = cwd.join(".fno").join("events.jsonl");
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    // Pin to the current HEAD: a coverage event for a prior commit doesn't
    // describe what finalize is about to arm. (x-0eaf finding 2.)
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut latest: Option<Value> = None;
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_coverage") {
            continue;
        }
        if !head.is_empty() {
            let ev_head = val
                .pointer("/data/head_sha")
                .and_then(Value::as_str)
                .unwrap_or("");
            if ev_head != head {
                continue;
            }
        }
        latest = Some(val);
    }
    match latest {
        Some(v) => {
            v.pointer("/data/coverage").and_then(Value::as_str) == Some("covered")
                && v.pointer("/data/reviewed_count")
                    .and_then(Value::as_i64)
                    .map_or(false, |n| n > 0)
        }
        None => false,
    }
}

/// Arm GitHub's native auto-merge for the branch's open PR. Returns whether it
/// armed, for the terminal event's `auto_merge_armed` field.
///
/// Best-effort and log-only, the same fatality as `stamp_node_pr` and every
/// other gh-dependent step here: it is deliberately NOT returned into `failed`.
/// Failing to arm leaves a green, reviewed, mergeable PR for a human, which is
/// the safe direction; holding `session_finalized` open to retry an arm would
/// re-run the stamp/handoff steps for a merge GitHub may already have performed.
///
/// Re-arming needs no per-head dedup: `--auto` sets a PR-level flag rather than
/// appending anything, so a retried terminal fire is a no-op on GitHub's side.
///
/// `config.auto_merge.merge_strategy` and `.delete_branch_on_merge` shape the
/// argv, matching `fno do pr merge`. The strategy used to be hardcoded `--merge`,
/// carried over verbatim from the PR-creation call site this replaced, so a
/// squash-only repo was armed with a merge method it forbids - and because
/// arming is log-only, GitHub's rejection was one stderr line inside a stop
/// hook. The symptom was not a wrong commit shape but auto-merge silently never
/// working, indistinguishable from nobody having opted in.
///
/// `require_checks_pass` is deliberately NOT read. On `fno do pr merge` it decides
/// whether `--auto` is passed at all (false meaning "merge now, do not wait");
/// here `--auto` IS the operation and `loop-check` has already verified green,
/// so honoring it would let a config value turn arming into a no-op.
/// The head_sha from the latest covered review_coverage event (matching the
/// current HEAD), or None. Used to pin the auto-merge arm. (x-0eaf)
fn covered_head_from_event(cwd: &Path) -> Option<String> {
    let path = cwd.join(".fno").join("events.jsonl");
    let content = fs::read_to_string(&path).ok()?;
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut latest: Option<String> = None;
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_coverage") {
            continue;
        }
        if val.pointer("/data/coverage").and_then(|v| v.as_str()) != Some("covered") {
            continue;
        }
        if val
            .pointer("/data/reviewed_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            <= 0
        {
            continue;
        }
        let ev_head = val
            .pointer("/data/head_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !head.is_empty() && ev_head != head {
            continue;
        }
        latest = Some(ev_head.to_string());
    }
    latest.filter(|s| !s.is_empty())
}

fn classify_dispatch_hold_probe(success: bool, stdout: &[u8], stderr: &[u8]) -> Option<String> {
    if success {
        return None;
    }
    // A moved verb spelling prints one teaching line to stderr ("fno X is
    // now fno Y"). It is not a refusal reason: kept, it would lead the
    // operator message with noise and permanently mask the empty-output
    // fallback below, because stderr would never be empty on this probe.
    let stripped: String = String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !(line.starts_with("fno ") && line.contains(" is now fno ")))
        .collect::<Vec<&str>>()
        .join("\n");
    let detail = if stripped.trim().is_empty() {
        stdout
    } else {
        stripped.as_bytes()
    };
    let message = String::from_utf8_lossy(detail).trim().to_string();
    Some(if message.is_empty() {
        "dispatch hold state unreadable; refusing to assume unheld".to_string()
    } else {
        message
    })
}

fn dispatch_hold_refusal(cwd: &Path, number: u64) -> Option<String> {
    match Command::new("fno")
        .args(["do", "pr", "hold-check", number.to_string().as_str()])
        .current_dir(cwd)
        .output()
    {
        Ok(output) => {
            classify_dispatch_hold_probe(output.status.success(), &output.stdout, &output.stderr)
        }
        Err(error) => Some(format!(
            "dispatch hold check unavailable ({error}); refusing to assume unheld"
        )),
    }
}

fn arm_auto_merge(cwd: &Path) -> (bool, Option<String>) {
    let Some((number, _url)) = gh_pr_ref(cwd) else {
        eprintln!("finalize: no open PR found for branch; auto-merge not armed");
        return (false, None);
    };
    if let Some(blocked) = dispatch_hold_refusal(cwd, number) {
        eprintln!("finalize: auto-merge NOT armed for PR {number}: {blocked}");
        return (false, Some(blocked));
    }
    // Stacked-base guard: a PR merged into a base branch that no longer leads to
    // the default branch reports MERGED and ships nothing. This arm reaches
    // `gh pr merge` without passing through `fno do pr merge`, so it calls the
    // shared predicate itself - a guard on one of N reachable merge paths is
    // decorative.
    //
    // The arm is also the one path where the check is a SNAPSHOT: `--auto` fires
    // server-side later, so the base can die between here and the merge with no
    // push to invalidate anything. `.github/workflows/stacked-base-guard.yml`
    // re-stamps on push-to-main to cover that window.
    //
    // Exit 3 is a confirmed stale base and refuses the arm. Every other non-zero
    // (4 unknown, 127 no gh, a spawn error) arms anyway with a breadcrumb:
    // refusing on an unevaluated probe would turn a gh hiccup into auto-merge
    // silently never working, which reads exactly like nobody having opted in.
    let pr_arg = number.to_string();
    // One exit-code read, three outcomes, rather than several field accesses:
    // `check-plan-rung-authority.sh` ratchets a per-file identifier count over
    // production Rust, so each extra access here fails CI with a message about
    // plan frontmatter that has nothing to do with this code.
    match Command::new("fno")
        .args(["do", "pr", "base-lineage-check", pr_arg.as_str()])
        .current_dir(cwd)
        .output()
    {
        Ok(o) => match o.status.code() {
            Some(3) => {
                eprintln!(
                    "finalize: auto-merge NOT armed for PR {number}: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                return (false, Some("stale base".to_string()));
            }
            Some(0) => {}
            // Includes None (killed by a signal): unevaluated, so arm anyway.
            other => eprintln!(
                "finalize: stacked-base probe inconclusive for PR {number} (exit {other:?}); \
                 arming anyway: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        },
        Err(e) => eprintln!(
            "finalize: stacked-base probe unavailable for PR {number} ({e}); arming anyway"
        ),
    }
    let strategy = crate::agents_config::auto_merge_strategy(cwd);
    // No --delete-branch here (x-9d11): the flag's LOCAL delete attempt is the
    // x-7267 false-failure shape. KNOWN GAP: nothing deletes the remote ref
    // when the queue later lands the merge (the executor-side
    // _post_merge_remote_delete never runs on that path). Repos wanting the
    // ref gone should enable GitHub's own delete-head-branches setting.
    let mut args = vec![
        "pr".to_string(),
        "merge".to_string(),
        number.to_string(),
        "--auto".to_string(),
        format!("--{strategy}"),
    ];
    // x-0eaf P1 (codex round 3): pin the arm to the covered head so a racing
    // remote push cannot land an unreviewed head via GitHub's --auto queue.
    if let Some(sha) = covered_head_from_event(cwd) {
        args.push("--match-head-commit".to_string());
        args.push(sha);
    }
    // The strategy is named in every failure line below: a repo that forbids the
    // configured merge method fails here exactly like stale auth or an
    // unmergeable state would, and the config key is the only way to tell them
    // apart from a log nobody is watching live.
    match Command::new("gh").args(&args).current_dir(cwd).output() {
        Ok(o) if o.status.success() => {
            eprintln!("finalize: auto-merge armed for PR {number} with --{strategy}");
            (true, None)
        }
        // Surface gh's own message so an operator can tell a repo with the
        // auto-merge feature disabled from stale auth or an unmergeable state.
        Ok(o) => {
            // x-7267: gh exits nonzero on an ALREADY-MERGED PR ("was already
            // merged") - a second path landed it between the terminal and this
            // arm. That is success-shaped (the merge the arm existed to cause
            // has happened), so name it as such instead of logging a false
            // failure an operator would chase.
            let merged = pr_info(cwd, Some(number))
                .and_then(|payload| {
                    payload
                        .get("state")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .is_some_and(|state| state == "MERGED");
            if merged {
                eprintln!(
                    "finalize: PR {number} already merged (another path landed it); \
                     nothing to arm - auto-merge goal already met"
                );
                // No queue entry exists, so armed=false; the blocked_reason
                // names the state so the event can neither claim a phantom arm
                // (armed=true for an arm that never happened) nor read as an
                // unexplained decline.
                return (false, Some("already merged".to_string()));
            }
            eprintln!(
                "finalize: auto-merge arm failed for PR {number} with --{strategy} \
                 (from config.auto_merge.merge_strategy; check the repo allows that \
                 merge method) (non-fatal): {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            (false, None)
        }
        Err(e) => {
            eprintln!(
                "finalize: auto-merge arm failed for PR {number} with --{strategy} \
                 (from config.auto_merge.merge_strategy) (non-fatal): {e}"
            );
            (false, None)
        }
    }
}

/// The terminals a `do` stamp is allowed on. Planner-only sessions exit via
/// Budget/NoProgress/Interrupted, so those never stamp.
///
/// Deliberately NOT `SHIP_REASONS`, which also contains `DoneAdvisory`: a doc
/// ship authors no branch commits, so reusing that constant here would stamp
/// every doc ship. The two sets disagree on purpose.
fn is_do_stamp_terminal(reason: &str) -> bool {
    classify_legacy(reason)
        .map(|record| record.projection().do_stamp_terminal)
        .unwrap_or(false)
}

/// Guarded `do` lifecycle stamp (x-0469). `/execute` Step 1.5 is the earlier truthful
/// stamp, but most `/target` runs implement inline and never invoke `/execute`, so the
/// phase was recorded twice across ~2800 nodes. This is the backstop: one record
/// per implementing session, at its own finish line.
///
/// `sessions[]` is append-only and every worker-session resolver trusts it, so a
/// WRONG stamp is strictly worse than none - stamping at init recorded the
/// PLANNER (PR #504, reverted). G1 (ship reason) and G4 (authored-commit
/// evidence) evaluate here; G2 (identity continuity) and G3 (plan agreement)
/// ride flags into `fno backlog session add`, which already owns ambient-identity
/// precedence.
///
/// G1, G2 and G4 fail closed. G3 is the deliberate exception: an unreadable plan
/// or an absent `claims:` counts as agreement (mirroring `/execute` Step 1.5, since
/// absent evidence of conflict is not conflict), and only a positive
/// disagreement skips. The primitive says so on stderr when it could not
/// evaluate, so that leniency is never silent.
///
/// Log-only and never retried: a guard skip is a designed outcome, and retrying
/// one would spin.
fn stamp_node_do(cwd: &Path, m: &ManifestFields, reason: &str) {
    let Some(node) = m.graph_node_id.as_deref() else {
        return;
    };
    let skip = |guard: &str, why: String| {
        eprintln!("finalize: do stamp skipped for node {node} ({guard}: {why})");
    };
    if !is_do_stamp_terminal(reason) {
        return skip("G1", format!("{reason} is not a ship terminal"));
    }
    // G2 input: an absent id cannot prove continuity, so it is not stamped.
    let Some(session) = m.harness_session_id.as_deref() else {
        return skip("G2", "manifest carries no harness_session_id".into());
    };
    let (Some(created_at), Some(head)) = (m.created_at.as_deref(), m.initial_head.as_deref())
    else {
        return skip("G4", "manifest predates initial_head/created_at".into());
    };
    let Some(floor) = parse_utc_epoch(created_at) else {
        return skip("G4", format!("unparseable created_at {created_at}"));
    };
    if !authored_work_since(cwd, head, floor) {
        return skip(
            "G4",
            format!("no non-merge commit in {head}..HEAD authored at/after {created_at}"),
        );
    }

    let mut cmd = Command::new("fno");
    cmd.args(["backlog", "session", "add", node, "--phase", "do"]);
    cmd.args(["--require-session", session]);
    if let Some(plan) = m.plan_path.as_deref() {
        cmd.args(["--guard-plan", plan]);
    }
    cmd.args(["--started-at", created_at]);
    let ok = cmd
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("finalize: do stamp failed for node {node} (non-fatal)");
    }
}

/// True when `initial_head..HEAD` holds a non-merge commit on HEAD's own
/// first-parent chain authored at or after `floor` (epoch seconds).
///
/// Author dates, never committer dates: `git rebase` rewrites the committer date
/// to now while preserving the author date, so a successor that merely rebased a
/// predecessor's branch onto main moves HEAD maximally while authoring nothing.
/// `--since` filters on committer date and would re-open exactly that hole.
///
/// `--first-parent` closes the sibling hole. The range is set subtraction over
/// the whole DAG, and `--no-merges` drops a merge commit but KEEPS its payload,
/// so a session that authored nothing and only ran `git merge origin/main` would
/// otherwise pass on other contributors' commits - genuinely recent, so the
/// author-date floor cannot catch them. The session's own commits sit on HEAD's
/// first-parent chain, so they still count.
///
/// The `:/` pathspec (repo root, cwd-independent) requires the commit to have
/// actually changed a file, so an empty commit is not evidence of work.
///
/// The range survives a rebased-away `initial_head`; any git failure (GC'd
/// baseline, deleted worktree) reads as no evidence, and so does a single
/// unparseable timestamp - partial evidence is not evidence.
///
/// Accepted residual: the floor is inclusive at one-second resolution, so a
/// predecessor commit authored in the same second as this session's init would
/// count. Tightening to exclusive would instead drop a legitimate commit
/// authored in the same second as init; both windows are one second wide, and
/// this direction keeps the fast-implementer case covered.
fn authored_work_since(cwd: &Path, initial_head: &str, floor: i64) -> bool {
    let range = format!("{initial_head}..HEAD");
    let args = [
        "log",
        "--no-merges",
        "--first-parent",
        "--format=%at",
        &range,
        "--",
        ":/",
    ];
    let Some(out) = git_capture(cwd, &args) else {
        return false;
    };
    let mut any_after_floor = false;
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(at) = line.parse::<i64>() else {
            return false; // unreadable evidence, not partial evidence
        };
        any_after_floor |= at >= floor;
    }
    any_after_floor
}

/// Parse a manifest ISO-8601 UTC instant to epoch seconds.
fn parse_utc_epoch(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp())
}

/// Run `git <args>` in cwd, returning trimmed stdout on success.
pub(crate) fn git_capture(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

// ── WIP rescue commit (x-cdc7 HALF ONE) ────────────────────────────────────

/// If `cwd`'s worktree is dirty, commit everything as a clearly-labeled WIP
/// commit naming the terminal reason and what landed. Returns the new commit
/// sha, or `None` on a clean tree / non-git dir / commit failure.
///
/// `git add -A` is deliberate here, unlike the stale-base-rebase case this
/// repo otherwise warns off `-A` for (AGENTS.md pitfalls corpus): this is the
/// SAME worker's own in-flight work, not a merge in progress (checked below,
/// not assumed), and only `-A` reliably captures new untracked files alongside
/// modifications - the exact shape of the measured near-miss (950 insertions,
/// 11 files, none staged).
fn commit_wip_if_dirty(cwd: &Path, reason: &str) -> Option<String> {
    if !cwd.join(".git").exists() {
        return None; // not a git worktree at all
    }
    // Refuse to touch a tree mid-merge/mid-rebase/mid-cherry-pick: `git add -A`
    // would stage conflicted files (still holding `<<<<<<<` markers) as
    // "resolved", and `git commit` would silently finish the operation on
    // garbage content. `git rev-parse --git-path` resolves worktree-relative
    // (a linked worktree's real gitdir lives under `.git/worktrees/<name>/`,
    // not `<cwd>/.git/`), unlike a bare `cwd.join(".git")` check.
    let in_progress = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "rebase-merge",
        "rebase-apply",
    ]
    .iter()
    .any(|marker| {
        // `--git-path` prints a path relative to the git invocation's cwd
        // (the worktree, via `current_dir(cwd)` inside `git_capture`), so
        // it must be re-joined onto `cwd` before checking existence - this
        // process's own cwd is unrelated.
        git_capture(cwd, &["rev-parse", "--git-path", marker]).is_some_and(|p| cwd.join(p).exists())
    });
    if in_progress {
        eprintln!("finalize: worktree is mid-merge/rebase; skipping WIP rescue commit ({reason})");
        return None;
    }
    let status = git_capture(cwd, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return None; // clean tree, nothing to rescue
    }
    let files: Vec<&str> = status.lines().collect();
    let shown: Vec<&str> = files.iter().take(20).copied().collect();
    let mut landed = shown.join("; ");
    if files.len() > shown.len() {
        landed.push_str(&format!("; +{} more", files.len() - shown.len()));
    }
    let message = format!(
        "WIP: session terminated ({reason}) with uncommitted work\n\n\
         Auto-committed by the terminal WIP-commit gate (x-cdc7) so a killed \
         worker never loses in-flight work. Not reviewed, not tested - treat \
         as a checkpoint, not a finished change.\n\n\
         Landed:\n{landed}"
    );
    let add_ok = Command::new("git")
        .current_dir(cwd)
        .args(["add", "-A"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !add_ok {
        eprintln!("finalize: WIP git add failed; leaving worktree dirty");
        return None;
    }
    // Try the repo's own commit hooks + signing config first; a hook
    // rejection or an unavailable gpg key must never be the reason in-flight
    // work is lost, so fall back to skipping both rather than leaving the
    // tree uncommitted. This rescue commit is clearly labeled WIP, never
    // mistaken for an authored, reviewed change.
    let committed = Command::new("git")
        .current_dir(cwd)
        .args(["commit", "-q", "-m", &message])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        || Command::new("git")
            .current_dir(cwd)
            .args([
                "commit",
                "-q",
                "--no-verify",
                "-c",
                "commit.gpgsign=false",
                "-m",
                &message,
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !committed {
        eprintln!("finalize: WIP git commit failed");
        return None;
    }
    let sha = git_capture(cwd, &["rev-parse", "HEAD"]);
    if let Some(sha) = &sha {
        eprintln!("finalize: WIP commit {sha} saved uncommitted work ({reason})");
    }
    sha
}

// ── unanswered-question filing (x-32f3 HALF TWO) ────────────────────────────

/// Positive-marker detector for an operator-directed question left in a
/// worker's last message: matches the phrase, never an absence (AGENTS.md
/// pitfalls corpus: "assert a positive marker, never an absence"). Narrow and
/// specimen-derived over "any text with a question mark" - a false positive
/// costs a redundant outstanding row, a false negative loses the question,
/// and the second failure mode is the one this fix exists for.
const OPERATOR_QUESTION_MARKERS: &[&str] = &[
    "awaiting operator",
    "waiting for operator",
    "waiting on operator",
    "need the operator",
    "need operator",
    "ask the operator",
    "operator's",
    "operator to",
    "need you to",
    "need your",
    "please advise",
    "please confirm",
    "need confirmation",
];

fn extract_operator_question(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if !OPERATOR_QUESTION_MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Cap length: this becomes a CLI argument and an `outstanding` report line.
    const MAX_CHARS: usize = 600;
    if trimmed.chars().count() > MAX_CHARS {
        Some(trimmed.chars().take(MAX_CHARS).collect::<String>() + "…")
    } else {
        Some(trimmed.to_string())
    }
}

/// Has this session already filed an open question? Read via the same `fno
/// inbox outstanding --json` an operator would run, so dedup can never drift
/// from what is actually on record (never re-derived state).
fn session_already_filed(cwd: &Path, session_id: &str) -> bool {
    let out = match Command::new("fno")
        .current_dir(cwd)
        .args(["inbox", "outstanding", "--json"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false, // unreadable -> fail toward filing, never toward silence
    };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
        return false;
    };
    v.get("questions")
        .and_then(|q| q.as_array())
        .is_some_and(|qs| {
            qs.iter()
                .any(|q| q.get("session_id").and_then(|s| s.as_str()) == Some(session_id))
        })
}

fn file_outstanding_question(cwd: &Path, question: &str, node: Option<&str>) -> bool {
    let mut cmd = Command::new("fno");
    cmd.current_dir(cwd)
        .args(["inbox", "outstanding", "ask", question]);
    if let Some(n) = node {
        cmd.args(["--node", n]);
    }
    match cmd.status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("finalize: outstanding ask exited {:?}", s.code());
            false
        }
        Err(e) => {
            eprintln!("finalize: outstanding ask spawn failed: {e}");
            false
        }
    }
}

// ── completion eval artifact, every terminal but NoWork (ab-1a92b677, x-8fc0) ──

/// Write a structured completion eval for this session to the postmortems
/// dir, then best-effort append a corrections.log pointer so the autocorrect
/// monthly review consumes it. Filename keyed by date + session-id prefix so
/// a retry overwrites rather than duplicating (idempotent). The body branches
/// on `STUCK_REASONS`: a stuck session gets failure-triage prose (unchanged
/// from the original stuck-only artifact); every other reason gets a lighter
/// eval prose - the corpus this feeds must see both what went wrong and what
/// went right, not failures only (x-8fc0).
#[allow(clippy::too_many_arguments)]
fn write_postmortem(
    cwd: &Path,
    session_id: &str,
    m: &ManifestFields,
    reason: &str,
    transcript: Option<&Path>,
    postmortems_override: Option<&Path>,
    settings_override: Option<&Path>,
    home: Option<&Path>,
) -> Result<String, String> {
    let dir = resolve_postmortems_dir(postmortems_override, settings_override, home, cwd);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let now = now_rfc3339_utc();
    // Defensive slice: now_rfc3339_utc() always returns a full RFC3339 string,
    // but never index a str blindly (gemini review). Falls back to the whole
    // string if it were ever shorter than the date prefix.
    let date = now.get(..10).unwrap_or(&now); // YYYY-MM-DD
    let sid_short: String = session_id.chars().take(16).collect();
    let file = dir.join(format!("{date}-{sid_short}.md"));

    let node = m.graph_node_id.clone().unwrap_or_else(|| "-".into());
    let plan = m.plan_path.clone().unwrap_or_else(|| "-".into());
    let title = m.input.clone().unwrap_or_else(|| "Untitled".into());
    let last_msg = transcript
        .and_then(last_assistant_text)
        .unwrap_or_else(|| "(transcript unavailable)".into());
    let commits = git_capture(cwd, &["log", "--oneline", "-10"])
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(log unavailable)".into());
    let tree = git_capture(cwd, &["status", "--short"])
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(clean)".into());

    // Header/commits/tree scaffolding is identical in both branches; only the
    // heading word, the termination-line suffix, the trailing section name,
    // and its prose differ. Share the scaffold, branch only what differs
    // (was two ~20-line near-identical format! blocks).
    let stuck = is_stuck_reason(reason);
    let (heading, term_suffix, section, prose): (&str, &str, &str, String) = if stuck {
        (
            "Postmortem",
            " (stuck: exited without shipping)",
            "Triage",
            format!(
                "A `{reason}` terminal means `fno-agents loop-check` saw no forward \
                 progress (or the budget cap tripped) and let the session exit. Review \
                 the last message and working tree above: was the agent blocked on an \
                 external dependency, looping without committing, or done but unable to \
                 emit a promise? Feed recurring patterns back into the rules."
            ),
        )
    } else {
        (
            "Completion eval",
            "",
            "Eval",
            format!(
                "A `{reason}` terminal means this session completed - a clean \
                 completion still belongs in the corpus the autocorrect monthly \
                 review reads, not only a stuck one (x-8fc0: a failure-only \
                 sample writes rules in a predictable, overcautious direction). \
                 Review the last message above for anything the pre-promise \
                 memory pass should have captured and check it against the \
                 blocklist in skills/target/references/pre-promise.md before \
                 promoting it - do not write an env-dependent finding, a \
                 negative tool claim, a transient error, or an unresolved \
                 failure dressed up as a validated workflow."
            ),
        )
    };
    let body = format!(
        "# {heading}: {sid_short}\n\n\
         - session: `{session_id}`\n\
         - termination: **{reason}**{term_suffix}\n\
         - node: `{node}`\n\
         - plan: `{plan}`\n\
         - feature: {title}\n\
         - generated: {now} (mechanical, by `fno-agents finalize`)\n\n\
         ## Last assistant message\n\n```\n{last_msg}\n```\n\n\
         ## Recent commits\n\n```\n{commits}\n```\n\n\
         ## Working tree\n\n```\n{tree}\n```\n\n\
         ## {section}\n\n{prose}\n",
    );
    fs::write(&file, &body).map_err(|e| format!("write {}: {e}", file.display()))?;

    append_corrections_pointer(home, &file, reason, &last_msg);
    Ok(file.to_string_lossy().into_owned())
}

/// Resolve the postmortems dir: explicit override -> `$POSTMORTEMS_DIR`
/// (exported by emit_shell.py from config.paths.postmortems_dir) -> the
/// `--settings` override file then project then global settings.yaml
/// `postmortems_dir:` -> default `~/.fno/postmortems`. Pure-Rust; never
/// shells `fno` (Domain Pitfall), mirroring resolve_handoffs_dir (which also
/// honors `--settings`, codex P2).
fn resolve_postmortems_dir(
    override_dir: Option<&Path>,
    settings_override: Option<&Path>,
    home: Option<&Path>,
    cwd: &Path,
) -> PathBuf {
    if let Some(d) = override_dir {
        return d.to_path_buf();
    }
    if let Some(d) = env_dir_unless_null("POSTMORTEMS_DIR") {
        return d;
    }
    let project = resolve_project_name(settings_override, home, cwd);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(s) = settings_override {
        candidates.push(s.to_path_buf());
    }
    candidates.push(cwd.join(".fno/config.toml"));
    if let Some(h) = home {
        candidates.push(h.join(".fno/config.toml"));
    }
    for sp in candidates {
        if let Some(raw) = read_path_setting(&sp, "postmortems_dir") {
            if let Some(expanded) = expand_handoffs_template(&raw, home, &project) {
                return expanded;
            }
        }
    }
    let base = home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    base.join(".fno/postmortems")
}

/// Best-effort: the newest assistant text message in the transcript JSONL, used
/// as the "what was it doing when it got stuck" signal. Bounded to keep the
/// artifact readable. Returns None on any read/parse miss.
fn last_assistant_text(transcript: &Path) -> Option<String> {
    let content = fs::read_to_string(transcript).ok()?;
    for line in content.lines().rev() {
        let line = line.trim();
        // Cheap pre-filter: an assistant entry always carries the literal
        // "assistant" (its role), so skip the JSON parse for the many user /
        // tool-output lines that don't (gemini review). No false negatives.
        if line.is_empty() || !line.contains("assistant") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = val
            .pointer("/message/role")
            .or_else(|| val.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if role != "assistant" {
            continue;
        }
        let text = assistant_text_blocks(&val);
        if !text.trim().is_empty() {
            return Some(text.chars().take(4000).collect());
        }
    }
    None
}

/// Join the text blocks of a transcript assistant entry: string content, or an
/// array of content blocks (tool_use/tool_result blocks skipped).
fn assistant_text_blocks(val: &Value) -> String {
    if let Some(s) = val.pointer("/message/content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = val.pointer("/message/content").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
    }
    // Top-level `{"role":"assistant","content":"..."}` shape (matches
    // loopcheck::extract_assistant_text and the hook tests; codex P2). Without
    // this, last_assistant_text accepts the role but records no message.
    if let Some(s) = val.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    String::new()
}

/// Best-effort: append a pointer line to `~/.fno/corrections.log` so the
/// autocorrect monthly review picks the postmortem up. Only writes when the log
/// already exists (the autocorrect feature creates it) - never creates it.
/// Format mirrors the pre-wedge generator:
/// `{ts} | S1 | target-postmortem | {path} | {reason}: {detail_truncated}`.
///
/// Lives under ~/.fno/, not ~/.claude/, per the placement rule (ab-f063 Wave
/// 2). Resolution order mirrors scripts/lib/corrections-lock.sh's
/// corrections_log_path(): POSTMORTEM_CORRECTIONS_LOG override, then
/// FNO_HOME, then home-relative default.
fn append_corrections_pointer(home: Option<&Path>, postmortem: &Path, reason: &str, detail: &str) {
    let log = match std::env::var_os("POSTMORTEM_CORRECTIONS_LOG") {
        Some(p) => PathBuf::from(p),
        None => match std::env::var_os("FNO_HOME") {
            Some(p) => PathBuf::from(p).join("corrections.log"),
            None => match home {
                Some(h) => h.join(".fno/corrections.log"),
                None => return,
            },
        },
    };
    if !log.is_file() {
        return; // autocorrect not enabled here; nothing to feed
    }
    let detail_trunc: String = detail.replace(['\n', '\r'], " ").chars().take(80).collect();
    let detail_trunc = if detail_trunc.trim().is_empty() {
        "-".to_string()
    } else {
        detail_trunc
    };
    let line = format!(
        "{} | S1 | target-postmortem | {} | {reason}: {detail_trunc}\n",
        now_rfc3339_utc(),
        postmortem.display(),
    );
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&log) {
        let _ = f.write_all(line.as_bytes());
    }
}

// ── unit tests (process-free) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_required_and_optional() {
        let a = parse_args(&[
            "--state".into(),
            "/x/state.md".into(),
            "--cwd".into(),
            "/x".into(),
            "--reason".into(),
            "DonePRGreen".into(),
            "--transcript".into(),
            "/t/abc.jsonl".into(),
        ])
        .unwrap();
        assert_eq!(a.state.unwrap(), PathBuf::from("/x/state.md"));
        assert_eq!(a.reason.unwrap(), "DonePRGreen");
        assert_eq!(a.transcript.unwrap(), PathBuf::from("/t/abc.jsonl"));
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        assert!(parse_args(&["--bogus".into()]).is_err());
    }

    #[test]
    fn parse_pr_ref_valid_missing_and_malformed() {
        // Valid: number + url.
        assert_eq!(
            parse_pr_ref(br#"{"number": 358, "url": "https://x/pull/358"}"#),
            Some((358, "https://x/pull/358".to_string()))
        );
        // REST `fno do pr info` names the same field `pr`.
        assert_eq!(
            parse_pr_ref(br#"{"pr": 358, "url": "https://x/pull/358"}"#),
            Some((358, "https://x/pull/358".to_string()))
        );
        // Malformed JSON -> None (treated as "no PR", not a crash). AC1-ERR.
        assert_eq!(parse_pr_ref(b"not json"), None);
        // Missing number field -> None.
        assert_eq!(parse_pr_ref(br#"{"url": "https://x/pull/1"}"#), None);
        // Empty url -> None.
        assert_eq!(parse_pr_ref(br#"{"number": 1, "url": ""}"#), None);
    }

    #[test]
    fn manifest_reads_frontmatter_and_body_keys() {
        let content = "---\n\
            session_id: 20260607T220509Z-42092-ceefb9\n\
            plan_path: \"internal/fno/design/step6.md\"\n\
            input: \"ab-f8e5f214 no-merge\"\n\
            claude_transcript_id: de977b03-aaaa\n\
            ---\n\
            # Target Session State\n\
            graph_node_id: ab-f8e5f214\n\
            target_claim_key: \"node:ab-f8e5f214\"\n";
        let m = parse_manifest_fields(content);
        assert_eq!(
            m.session_id.as_deref(),
            Some("20260607T220509Z-42092-ceefb9")
        );
        assert_eq!(m.plan_path.as_deref(), Some("internal/fno/design/step6.md"));
        assert_eq!(m.claude_transcript_id.as_deref(), Some("de977b03-aaaa"));
        assert_eq!(m.graph_node_id.as_deref(), Some("ab-f8e5f214"));
        assert_eq!(m.input.as_deref(), Some("ab-f8e5f214 no-merge"));
    }

    #[test]
    fn canonical_fno_id_wins_when_manifest_keys_are_reversed() {
        let m = parse_manifest_fields(
            "---\n\
             session_id: legacy-run\n\
             fno_id: 20260823T060900Z-cx73523-e04109\n\
             ---\n",
        );
        assert_eq!(
            m.session_id.as_deref(),
            Some("20260823T060900Z-cx73523-e04109")
        );
    }

    #[test]
    fn manifest_reads_do_stamp_guard_inputs() {
        // created_at carries colons, so the split_once(':') parse must keep the
        // whole remainder, not the first segment.
        let content = "---\n\
            created_at: 2026-07-20T21:48:25Z\n\
            initial_head: eb7505a737c53a102c0f03e04ca7b92995175bb4\n\
            harness_session_id: 3c6aaaa0-db8b-48ff\n\
            ---\n";
        let m = parse_manifest_fields(content);
        assert_eq!(m.created_at.as_deref(), Some("2026-07-20T21:48:25Z"));
        assert_eq!(
            m.initial_head.as_deref(),
            Some("eb7505a737c53a102c0f03e04ca7b92995175bb4")
        );
        assert_eq!(m.harness_session_id.as_deref(), Some("3c6aaaa0-db8b-48ff"));
    }

    #[test]
    fn manifest_without_do_stamp_guard_inputs_reads_none() {
        // Legacy manifests (and a repo with no commits, which writes `null`) leave
        // the guards without inputs, so the stamp fails closed.
        let m = parse_manifest_fields("---\ninitial_head: null\nsession_id: x\n---\n");
        assert_eq!(m.initial_head, None);
        assert_eq!(m.created_at, None);
        assert_eq!(m.harness_session_id, None);
    }

    /// A git repo with deterministic author dates, so work evidence is testable
    /// without sleeping. `git` env vars set both dates; the rebase case overrides
    /// only the committer date, exactly as `git rebase` does.
    fn git_fixture(dir: &Path) {
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
        };
        run(&["init", "-q", "."]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
    }

    /// A commit that changes a file. Evidence requires a content change, so a
    /// fixture built on `--allow-empty` would test a case the guard rejects.
    /// The filename is derived from the message so parallel branches do not
    /// collide when merged.
    fn commit_at(dir: &Path, msg: &str, author_epoch: i64, committer_epoch: i64) {
        let name: String = msg.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        fs::write(dir.join(format!("{name}.txt")), msg).unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        commit_raw(dir, msg, author_epoch, committer_epoch, false);
    }

    fn commit_raw(dir: &Path, msg: &str, author: i64, committer: i64, empty: bool) {
        let mut args = vec!["commit", "-q", "-m", msg];
        if empty {
            args.insert(1, "--allow-empty");
        }
        Command::new("git")
            .args(&args)
            .current_dir(dir)
            .env("GIT_AUTHOR_DATE", format!("@{author} +0000"))
            .env("GIT_COMMITTER_DATE", format!("@{committer} +0000"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
    }

    fn head_of(dir: &Path) -> String {
        git_capture(dir, &["rev-parse", "HEAD"]).unwrap()
    }

    // ── x-cdc7 HALF ONE: WIP-commit at every terminal ───────────────────────

    #[test]
    fn commit_wip_if_dirty_rescues_a_dirty_tree() {
        // The specimen this fix exists for: a worker dies mid-flight holding
        // uncommitted work. Assert the work is ON THE BRANCH afterward, not
        // just that the function returns something.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        let before = head_of(d);
        std::fs::write(d.join("in_flight.txt"), "950 insertions worth").unwrap();
        std::fs::write(d.join("new_untracked.txt"), "never staged").unwrap();

        let sha = commit_wip_if_dirty(d, "NoProgress");

        assert!(sha.is_some(), "a dirty tree must produce a rescue commit");
        assert_ne!(sha.as_deref(), Some(before.as_str()));
        assert_eq!(
            git_capture(d, &["status", "--porcelain"]),
            Some(String::new())
        );
        let log = git_capture(d, &["log", "-1", "--format=%s"]).unwrap();
        assert!(log.contains("WIP") && log.contains("NoProgress"));
        // The untracked file must be captured too - that is the whole point
        // of `git add -A` over a partial `git add -u`.
        let tracked = git_capture(d, &["show", "--stat", "HEAD"]).unwrap_or_default();
        assert!(tracked.contains("new_untracked.txt"));
    }

    #[test]
    fn commit_wip_if_dirty_is_a_noop_on_a_clean_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        let before = head_of(d);

        assert_eq!(commit_wip_if_dirty(d, "DonePRGreen"), None);
        assert_eq!(head_of(d), before, "a clean tree must not gain a commit");
    }

    #[test]
    fn commit_wip_if_dirty_is_a_noop_outside_a_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(commit_wip_if_dirty(tmp.path(), "Budget"), None);
    }

    #[test]
    fn commit_wip_if_dirty_refuses_a_tree_mid_merge_conflict() {
        // The specimen this guard exists for: a worker dies with an unresolved
        // merge conflict on disk. `git add -A` would stage the conflicted file
        // as "resolved" and commit garbage, silently finishing the merge.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base on main", 1000, 1000);
        Command::new("git")
            .args(["checkout", "-qb", "other"])
            .current_dir(d)
            .status()
            .unwrap();
        std::fs::write(d.join("in_flight.txt"), "other side").unwrap();
        commit_at(d, "other side", 1001, 1001);
        Command::new("git")
            .args(["checkout", "-q", "-"])
            .current_dir(d)
            .status()
            .unwrap();
        std::fs::write(d.join("in_flight.txt"), "main side").unwrap();
        commit_at(d, "main side", 1002, 1002);
        // Provoke a real conflict: silently ignore the failing merge exit code.
        let _ = Command::new("git")
            .args(["merge", "-q", "--no-edit", "other"])
            .current_dir(d)
            .status();
        assert!(
            d.join(".git/MERGE_HEAD").exists(),
            "fixture must actually be mid-conflict"
        );
        let before = head_of(d);

        let sha = commit_wip_if_dirty(d, "NoProgress");

        assert_eq!(sha, None, "a mid-merge tree must never be auto-committed");
        assert_eq!(head_of(d), before, "HEAD must not move");
        assert!(
            d.join(".git/MERGE_HEAD").exists(),
            "the merge must still be in progress, not silently finished"
        );
    }

    // ── x-32f3 HALF TWO: mandatory outstanding-question filing ─────────────

    #[test]
    fn extract_operator_question_matches_the_measured_specimen() {
        let text = "mouse-mode root cause identified; awaiting operator's terminal/mux info";
        assert_eq!(extract_operator_question(text).as_deref(), Some(text));
    }

    #[test]
    fn extract_operator_question_ignores_ordinary_status_text() {
        // A false negative loses a real question; a false positive is a
        // redundant outstanding row. This asserts the positive-marker list
        // does not fire on prose that merely happens to end a sentence.
        assert_eq!(
            extract_operator_question("implemented the fix and pushed a commit"),
            None
        );
    }

    #[test]
    fn extract_operator_question_truncates_long_text() {
        let long = "need you to ".to_string() + &"x".repeat(1000);
        let out = extract_operator_question(&long).unwrap();
        assert!(out.chars().count() <= 601);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn work_evidence_accepts_a_commit_authored_after_init() {
        // AC1-HP: the inline implementer. Its own commit is authored after its
        // own init, so the range carries evidence.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        let base = head_of(d);
        commit_at(d, "work", 3000, 3000);
        assert!(authored_work_since(d, &base, 2000));
    }

    #[test]
    fn work_evidence_rejects_an_empty_range() {
        // AC4-ERR: the session respawned onto an already-green PR. HEAD never
        // moved, so there is nothing to attribute.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        let base = head_of(d);
        assert!(!authored_work_since(d, &base, 2000));
    }

    #[test]
    fn work_evidence_rejects_a_rebase_only_successor() {
        // AC4b-ERR, the reason this guard reads AUTHOR dates. A successor that
        // only rebased a predecessor's branch moves HEAD maximally while
        // authoring nothing: the committer date is rewritten to now (5000, after
        // its init at 2000) but the author date is preserved (1500, before it).
        // A committer-date test (or `git log --since`) would wrongly pass here.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        let base = head_of(d);
        commit_at(d, "predecessor work, replayed by a rebase", 1500, 5000);
        assert!(!authored_work_since(d, &base, 2000));
    }

    #[test]
    fn work_evidence_rejects_commits_merged_in_from_upstream() {
        // A session that authored nothing and only ran `git merge origin/main`.
        // `--no-merges` drops the merge commit but keeps its payload, so without
        // --first-parent this passes on other contributors' commits - and their
        // author dates are genuinely recent, so the floor cannot catch them.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        let base = head_of(d);
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(d)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap();
        };
        git(&["checkout", "-q", "-b", "upstream"]);
        commit_at(d, "someone else's work", 5000, 5000);
        git(&["checkout", "-q", "-"]);
        git(&["merge", "-q", "--no-ff", "upstream", "-m", "merge upstream"]);
        assert!(!authored_work_since(d, &base, 2000));

        // ...and the session's own commit still counts: it lands on HEAD's
        // first-parent chain.
        commit_at(d, "my own work", 6000, 6000);
        assert!(authored_work_since(d, &base, 2000));
    }

    #[test]
    fn work_evidence_rejects_an_empty_commit() {
        // An empty commit moves HEAD and carries a fresh author date, so without
        // a pathspec it reads as work. Evidence has to be a content change.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        let base = head_of(d);
        commit_raw(d, "empty", 3000, 3000, true);
        assert!(!authored_work_since(d, &base, 2000));

        // ...and a commit that actually changes a file still counts.
        commit_at(d, "real work", 4000, 4000);
        assert!(authored_work_since(d, &base, 2000));
    }

    #[test]
    fn do_stamp_terminals_exclude_doc_ships_and_planner_exits() {
        assert!(is_do_stamp_terminal("DonePRGreen"));
        assert!(is_do_stamp_terminal("DoneAwaitingMerge"));
        // DoneAdvisory is in SHIP_REASONS but must NOT stamp: a doc ship authors
        // no branch commits. Swapping this predicate for SHIP_REASONS would
        // stamp every doc ship, and this assertion is what catches that.
        assert!(!is_do_stamp_terminal("DoneAdvisory"));
        for planner in ["Budget", "NoProgress", "Interrupted", "NoWork"] {
            assert!(!is_do_stamp_terminal(planner), "{planner} must not stamp");
        }
    }

    // ── x-1951: arm auto-merge at the green gate, not at PR creation ────────

    #[test]
    fn auto_merge_arms_only_on_an_approved_green_pr_terminal() {
        // AC2-HP: the one terminal that means "PR up, CI green, reviewed".
        assert!(should_arm_auto_merge("DonePRGreen", true));

        // AC6-EDGE: the human-merge path is not taxed by this at all. A refused
        // posture outranks everything, including the green terminal.
        assert!(!should_arm_auto_merge("DonePRGreen", false));

        // DoneAdvisory is the other SHIP_REASONS member but is a doc ship with
        // no PR; DoneAwaitingMerge is by definition a human's merge past
        // pre-existing main-red. Reusing SHIP_REASONS here would arm the first.
        for reason in ["DoneAdvisory", "DoneAwaitingMerge", "DoneBatched"] {
            assert!(
                !should_arm_auto_merge(reason, true),
                "{reason} must never arm auto-merge"
            );
        }
        for stuck in ["Budget", "NoProgress", "Interrupted", "Aborted", "NoWork"] {
            assert!(
                !should_arm_auto_merge(stuck, true),
                "{stuck} must never arm auto-merge"
            );
        }
    }

    #[test]
    fn dispatch_hold_probe_fails_closed_on_every_non_success() {
        assert_eq!(classify_dispatch_hold_probe(true, b"unheld", b""), None);
        assert_eq!(
            classify_dispatch_hold_probe(false, b"", b"dispatch-hold:x-owner"),
            Some("dispatch-hold:x-owner".to_string())
        );
        assert_eq!(
            classify_dispatch_hold_probe(false, b"", b""),
            Some("dispatch hold state unreadable; refusing to assume unheld".to_string())
        );
    }

    #[test]
    fn dispatch_hold_probe_strips_the_move_teaching_line() {
        // `fno pr hold-check` is a cold leaf of a moved spelling, so a failed
        // probe carries the deprecation announce on stderr. The refusal
        // reason must be the real error, and an announce-only stderr must
        // fall back to the unreadable message rather than quote the announce.
        assert_eq!(
            classify_dispatch_hold_probe(
                false,
                b"",
                b"fno pr hold-check is now fno do pr hold-check\ndispatch-hold:x-owner\n"
            ),
            Some("dispatch-hold:x-owner".to_string())
        );
        assert_eq!(
            classify_dispatch_hold_probe(
                false,
                b"",
                b"fno pr hold-check is now fno do pr hold-check\n"
            ),
            Some("dispatch hold state unreadable; refusing to assume unheld".to_string())
        );
    }

    #[test]
    fn manifest_auto_merge_posture_cannot_be_forged_by_input_text() {
        // Absent key -> no grant (a manifest minted before this field existed).
        assert_eq!(
            parse_manifest_fields("session_id: s1\n").auto_merge_approved,
            None
        );

        let approved = parse_manifest_fields("session_id: s1\nauto_merge_approved: true\n");
        assert_eq!(approved.auto_merge_approved, Some(true));
        let refused = parse_manifest_fields("session_id: s1\nauto_merge_approved: false\n");
        assert_eq!(refused.auto_merge_approved, Some(false));

        // The load-bearing case, in the REAL manifest layout: init writes the
        // untrusted `input` scalar (:839) BEFORE the canonical posture (:886).
        // A multi-line argument - a pasted spec under a refusing posture - spills
        // real newlines, so its lines reach the parser looking like manifest
        // keys and arrive FIRST. Neither first-wins nor last-wins is safe here;
        // the injected lines must not be read as keys at all.
        let injected = parse_manifest_fields(
            "---\n\
             session_id: s1\n\
             input: \"paste line one\n\
             auto_merge_approved: true\n\
             paste line three\"\n\
             plan_path: plan.md\n\
             auto_merge_approved: false\n\
             ---\n\
             graph_node_id: x-1a2b\n",
        );
        assert_eq!(
            injected.auto_merge_approved,
            Some(false),
            "input text must never forge the merge posture"
        );
        assert!(!should_arm_auto_merge(
            "DonePRGreen",
            injected.auto_merge_approved.unwrap_or(false)
        ));
        // Keys after the scalar closes are still real, and so is the body.
        assert_eq!(injected.plan_path.as_deref(), Some("plan.md"));
        assert_eq!(injected.graph_node_id.as_deref(), Some("x-1a2b"));

        // A single-line quoted input must NOT swallow the rest of the manifest.
        let normal = parse_manifest_fields(
            "session_id: s1\n\
             input: \"ordinary feature\"\n\
             auto_merge_approved: true\n",
        );
        assert_eq!(normal.auto_merge_approved, Some(true));

        // An input whose text ends in an ESCAPED quote does not close the scalar
        // early - otherwise the lines after it resume forging keys.
        let escaped = parse_manifest_fields(
            "session_id: s1\n\
             input: \"he said \\\"go\\\"\n\
             auto_merge_approved: true\n\
             done\"\n\
             auto_merge_approved: false\n",
        );
        assert_eq!(escaped.auto_merge_approved, Some(false));

        // sigma P1: a line ENDING in a user-typed `\"`. The writer escapes the
        // quote and NOT the backslash (init:811), so it lands as `\\"` - which a
        // backslash-PARITY rule reads as even, closes the scalar, and hands the
        // forgery back. Only "no backslash immediately before the quote" holds.
        // Every fixture below quotes `plan_path`, because init:840 ALWAYS writes
        // `plan_path: "..."`. An unquoted fixture is the decorative-guard shape:
        // it does not end in a quote, so it can never be mistaken for the
        // scalar's terminator, and the test passes on a shape no writer emits.
        let trailing_escaped_quote = parse_manifest_fields(
            "session_id: s1\n\
             input: \"snippet ending in \\\\\"\n\
             auto_merge_approved: true\n\
             rest of spec\"\n\
             plan_path: \"real.md\"\n\
             auto_merge_approved: false\n",
        );
        assert_eq!(
            trailing_escaped_quote.auto_merge_approved,
            Some(false),
            "a line ending in an escaped quote must not close the scalar"
        );
        assert_eq!(trailing_escaped_quote.plan_path.as_deref(), Some("real.md"));

        // The terminator line is itself untrusted: here the scalar closes on the
        // SAME line as the injection, so falling through must not grant it.
        let injection_on_terminator = parse_manifest_fields(
            "session_id: s1\n\
             input: \"paste line one\n\
             auto_merge_approved: true\"\n\
             plan_path: \"real.md\"\n\
             auto_merge_approved: false\n",
        );
        assert_eq!(
            injection_on_terminator.auto_merge_approved,
            Some(false),
            "an injection wearing the closing quote must not grant the posture"
        );
        assert_eq!(
            injection_on_terminator.plan_path.as_deref(),
            Some("real.md")
        );

        // sigma P2, the other direction: input ending in a lone `\` makes the
        // real terminator ambiguous, so the scalar reads as never closing. That
        // must cost only TRUST, never data - `plan_path` still parses (an
        // earlier cut skipped these lines and silently dropped the plan stamp),
        // and the posture falls back to no-grant.
        let trailing_backslash = parse_manifest_fields(
            "session_id: s1\n\
             input: \"fix the C:\\\\path\\\\\"\n\
             plan_path: \"real.md\"\n\
             graph_node_id: x-1a2b\n\
             auto_merge_approved: true\n",
        );
        assert_eq!(
            trailing_backslash.plan_path.as_deref(),
            Some("real.md"),
            "an ambiguous scalar must never swallow a load-bearing field"
        );
        assert_eq!(trailing_backslash.graph_node_id.as_deref(), Some("x-1a2b"));
        // The scalar closed AT plan_path, so the canonical posture below it is
        // trusted and honored. The grant is real here, not withheld - the cost
        // of the ambiguity is one line of reduced trust, never a dropped field.
        assert_eq!(trailing_backslash.auto_merge_approved, Some(true));
    }

    #[test]
    fn work_evidence_rejects_a_merge_only_range_and_a_bad_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git_fixture(d);
        commit_at(d, "base", 1000, 1000);
        // A GC'd / unknown baseline makes git fail: no evidence, never a stamp.
        assert!(!authored_work_since(
            d,
            "0000000000000000000000000000000000000000",
            0
        ));
        // So does a directory that is not a repo at all.
        assert!(!authored_work_since(
            Path::new("/nonexistent-xyz"),
            &head_of(d),
            0
        ));
    }

    #[test]
    fn utc_epoch_parses_manifest_timestamps_and_rejects_junk() {
        assert_eq!(parse_utc_epoch("1970-01-01T00:00:42Z"), Some(42));
        assert_eq!(parse_utc_epoch("2026-07-20"), None);
        assert_eq!(parse_utc_epoch(""), None);
    }

    #[test]
    fn manifest_reads_new_claude_session_id_key() {
        // The current key is claude_session_id (renamed from
        // claude_transcript_id). A manifest written by the new minter carries an
        // infix-tagged session_id and the new claude key; both must parse.
        let content = "---\n\
            session_id: 20260630T192705Z-cl52366-8979b6\n\
            claude_session_id: 26bf185f-a747-4624\n\
            ---\n";
        let m = parse_manifest_fields(content);
        assert_eq!(
            m.session_id.as_deref(),
            Some("20260630T192705Z-cl52366-8979b6")
        );
        assert_eq!(
            m.claude_transcript_id.as_deref(),
            Some("26bf185f-a747-4624")
        );
    }

    #[test]
    fn manifest_null_and_blank_are_skipped() {
        let m = parse_manifest_fields("plan_path: null\nsession_id: \nclaude_transcript_id: x\n");
        assert!(m.plan_path.is_none());
        assert!(m.session_id.is_none());
        assert_eq!(m.claude_transcript_id.as_deref(), Some("x"));
    }

    #[test]
    fn ship_reasons_gate() {
        assert!(
            classify_legacy("DonePRGreen")
                .unwrap()
                .projection()
                .ship_reason
        );
        assert!(
            classify_legacy("DoneAdvisory")
                .unwrap()
                .projection()
                .ship_reason
        );
        for non_ship in ["Budget", "NoProgress", "Interrupted", "Aborted", "NoWork"] {
            assert!(!classify_legacy(non_ship).unwrap().projection().ship_reason);
        }
    }

    #[test]
    fn done_planned_is_benign_terminal() {
        // A plan-only terminal graduates nothing, but (x-8fc0) it DOES still
        // get a completion eval - only NoWork is exempt from the eval.
        let planned = classify_legacy("DonePlanned").unwrap().projection();
        assert!(!planned.ship_reason);
        assert!(!planned.stuck);
        assert!(eval_should_fire("DonePlanned"));
    }

    #[test]
    fn finalize_done_closes_a_sealing_shadow_run() {
        let dir = tempfile::tempdir().unwrap();
        let run = "20260823T060900Z-cx73523-e04109";
        let log = dir.path().join(".fno/run-log.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        crate::run_state::append_transition(
            &log,
            run,
            crate::run_state::RunEvent::DispatchClassified,
        )
        .unwrap();
        crate::run_state::append_transition(&log, run, crate::run_state::RunEvent::TerminalDecided)
            .unwrap();

        let events = dir.path().join("events.jsonl");
        assert!(record_finalize_done(dir.path(), run, &events, &events));

        assert_eq!(
            crate::run_state::fold_run_state(&log, run).unwrap(),
            crate::run_state::RunState::Closed
        );
    }

    #[test]
    fn finalize_done_is_already_complete_for_an_aborted_shadow_run() {
        let dir = tempfile::tempdir().unwrap();
        let run = "20260823T060900Z-cx73523-e04109";
        let log = dir.path().join(".fno/run-log.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        crate::run_state::append_transition(
            &log,
            run,
            crate::run_state::RunEvent::DispatchClassified,
        )
        .unwrap();
        crate::run_state::append_transition(&log, run, crate::run_state::RunEvent::Abort).unwrap();

        let events = dir.path().join("events.jsonl");
        assert!(record_finalize_done(dir.path(), run, &events, &events));
        assert_eq!(
            crate::run_state::fold_run_state(&log, run).unwrap(),
            crate::run_state::RunState::Aborted
        );
        assert!(!events.exists(), "aborted runs need no finalize transition");
    }

    #[test]
    fn finalize_rejects_an_invalid_canonical_fno_id() {
        let manifest =
            parse_manifest_fields("---\nfno_id: short-run\nsession_id: valid-fallback\n---\n");
        assert_eq!(
            canonical_session_id(&manifest),
            Err("invalid canonical fno_id; refusing to finalize")
        );
    }

    #[test]
    fn prior_finalize_ship_reads_ship_flag_and_session() {
        let dir = std::env::temp_dir().join(format!("finalize-idem-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("events.jsonl");
        // S1: a non-ship finalize (Budget); S2: a ship finalize.
        fs::write(
            &log,
            "{\"ts\":\"t\",\"type\":\"loop_check\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\"}}\n\
             {\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\",\"ship\":false}}\n\
             {\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S2\",\"ship\":true}}\n",
        )
        .unwrap();
        assert_eq!(
            prior_finalize_ship(&log, "S1"),
            Some(false),
            "non-ship prior"
        );
        assert_eq!(prior_finalize_ship(&log, "S2"), Some(true), "ship prior");
        assert_eq!(prior_finalize_ship(&log, "S3"), None, "no prior for S3");
        assert_eq!(
            prior_finalize_ship(&dir.join("missing.jsonl"), "S1"),
            None,
            "missing log -> None"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ship_flag_wins_regardless_of_event_order() {
        // A non-ship finalize followed by a ship finalize for the SAME session
        // must report Some(true) (the lockout-bug fix: a ship is terminal-complete).
        let dir = std::env::temp_dir().join(format!("finalize-order-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("events.jsonl");
        fs::write(
            &log,
            "{\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\",\"ship\":false}}\n\
             {\"ts\":\"t\",\"type\":\"session_finalized\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\",\"ship\":true}}\n",
        )
        .unwrap();
        assert_eq!(prior_finalize_ship(&log, "S1"), Some(true));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_failed_event_does_not_count_as_finalized() {
        // A session_finalize_failed must NOT satisfy the idempotency guard, so
        // a later fire retries.
        let dir = std::env::temp_dir().join(format!("finalize-retry-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("events.jsonl");
        fs::write(
            &log,
            "{\"ts\":\"t\",\"type\":\"session_finalize_failed\",\"source\":\"hook\",\"data\":{\"session_id\":\"S1\"}}\n",
        )
        .unwrap();
        assert_eq!(prior_finalize_ship(&log, "S1"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrections_pointer_prefers_fno_home_over_claude_dir() {
        // ab-f063 Wave 2: corrections.log lives under ~/.fno/, not ~/.claude/.
        // FNO_HOME must win over a bare `home` fallback so an operator's
        // override (and the shared bash corrections_log_path() convention)
        // stays in sync with this Rust writer.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fno_home = std::env::temp_dir().join(format!("fin-corr-fh-{}", std::process::id()));
        let unused_home = std::env::temp_dir().join(format!("fin-corr-uh-{}", std::process::id()));
        let _ = fs::create_dir_all(&fno_home);
        let _ = fs::create_dir_all(&unused_home);
        let log_path = fno_home.join("corrections.log");
        fs::write(&log_path, "").unwrap();

        std::env::remove_var("POSTMORTEM_CORRECTIONS_LOG");
        std::env::set_var("FNO_HOME", &fno_home);
        append_corrections_pointer(
            Some(&unused_home),
            Path::new("/tmp/pm.md"),
            "Budget",
            "detail",
        );
        std::env::remove_var("FNO_HOME");

        let contents = fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("target-postmortem"), "{contents}");
        // The old ~/.claude/ location must not be touched.
        assert!(!unused_home.join(".claude").exists());
        let _ = fs::remove_dir_all(&fno_home);
        let _ = fs::remove_dir_all(&unused_home);
    }

    #[test]
    fn corrections_pointer_falls_back_to_home_dot_fno() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("fin-corr-home-{}", std::process::id()));
        let fno_dir = home.join(".fno");
        fs::create_dir_all(&fno_dir).unwrap();
        let log_path = fno_dir.join("corrections.log");
        fs::write(&log_path, "").unwrap();

        std::env::remove_var("POSTMORTEM_CORRECTIONS_LOG");
        std::env::remove_var("FNO_HOME");
        append_corrections_pointer(Some(&home), Path::new("/tmp/pm.md"), "NoProgress", "d");

        let contents = fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("target-postmortem"), "{contents}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_handoffs_dir_uses_vault_when_obsidian_enabled() {
        // ab-f063 Wave 2: no explicit handoffs_dir override, obsidian enabled
        // with a vault -> <vault>/internal/<project>/handoffs/, matching
        // paths.handoffs_dir() in the Python CLI (not the old ~/.fno/handoffs
        // fallback).
        let dir = std::env::temp_dir().join(format!("fin-hd-vault-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "[project]\nid = \"demo\"\n[obsidian]\nenabled = true\nvault = \"myvault\"\n",
        );
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join("myvault/internal/demo/handoffs"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_handoffs_dir_ignores_vault_when_obsidian_disabled() {
        // obsidian.enabled: false must NOT take the vault branch even though
        // vault: is set - falls through to the ~/.fno/handoffs/<project> default.
        let dir = std::env::temp_dir().join(format!("fin-hd-novault-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "[project]\nid = \"demo\"\n[obsidian]\nenabled = false\nvault = \"myvault\"\n",
        );
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join(".fno/handoffs/demo"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_handoffs_dir_indent_scan_ignores_other_sections_enabled_key() {
        // A generic `enabled:` key in an earlier, unrelated section must not be
        // mistaken for obsidian.enabled (flat-scan-by-key would get this wrong;
        // the indent-aware block scan must not).
        let dir = std::env::temp_dir().join(format!("fin-hd-indent-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "[project]\nid = \"demo\"\n[post_merge]\nenabled = false\n[obsidian]\nenabled = true\nvault = \"myvault\"\n",
        );
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join("myvault/internal/demo/handoffs"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_handoffs_dir_project_disabled_wins_over_global_enabled() {
        // codex review, PR #185: project explicitly sets obsidian.enabled:
        // false while GLOBAL enables it with a vault. fno.config._deep_merge
        // merges per-key with project winning, so the merged `enabled` is
        // false and handoffs must NOT resolve into the vault - a per-file
        // "first file with an opinion wins wholesale" scan gets this wrong
        // (it would see project's block, find no vault key there, and keep
        // scanning into global's enabled:true+vault).
        let dir = std::env::temp_dir().join(format!("fin-hd-proj-off-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "[project]\nid = \"demo\"\n[obsidian]\nenabled = false\n",
        );
        write_settings(&home, "[obsidian]\nenabled = true\nvault = \"myvault\"\n");
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join(".fno/handoffs/demo"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_handoffs_dir_inherits_global_vault_when_project_only_sets_enabled() {
        // The other half of the same merge: project enables obsidian but
        // does not itself set a vault, so `vault` should inherit from the
        // global file - per-key merge, not "project's block has no vault so
        // give up".
        let dir = std::env::temp_dir().join(format!("fin-hd-proj-inherit-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "[project]\nid = \"demo\"\n[obsidian]\nenabled = true\n",
        );
        write_settings(&home, "[obsidian]\nvault = \"myvault\"\n");
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join("myvault/internal/demo/handoffs"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_handoffs_dir_vault_scan_strips_inline_comments() {
        // gemini review, PR #185: "obsidian: # comment" must still match the
        // block header, and a comment on the vault: line must not get folded
        // into the resolved path.
        let dir = std::env::temp_dir().join(format!("fin-hd-inlinecmt-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(
            &cwd,
            "[project]\nid = \"demo\"\n[obsidian] # vault settings\nenabled = true # on\nvault = \"myvault\" # personal vault\n",
        );
        let got = resolve_handoffs_dir(None, None, &cwd, Some(&home));
        assert_eq!(got, home.join("myvault/internal/demo/handoffs"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn handoffs_template_expands_tilde_and_project() {
        let home = PathBuf::from("/home/user");
        let got = expand_handoffs_template(
            "~/myvault/internal/{project}/handoffs/",
            Some(&home),
            "demo",
        );
        assert_eq!(
            got,
            Some(PathBuf::from("/home/user/myvault/internal/demo/handoffs/"))
        );
    }

    #[test]
    fn handoffs_template_none_home_falls_back() {
        // No home -> a ~ template cannot expand -> None so the caller uses the
        // default dir instead of writing a literal "~..." path (gemini review).
        assert_eq!(
            expand_handoffs_template("~/myvault/internal/{project}/handoffs/", None, "demo"),
            None
        );
        // A non-tilde absolute template still expands fine without a home.
        assert_eq!(
            expand_handoffs_template("/srv/{project}/handoffs", None, "demo"),
            Some(PathBuf::from("/srv/demo/handoffs"))
        );
    }

    #[test]
    fn handoffs_template_unresolved_brace_falls_back() {
        let home = PathBuf::from("/home/user");
        // {vault} cannot be resolved here -> None so the caller uses the fallback.
        assert_eq!(
            expand_handoffs_template("{vault}/fno/{project}/handoffs", Some(&home), "demo"),
            None
        );
    }

    #[test]
    fn read_path_setting_parses_value() {
        let dir = std::env::temp_dir().join(format!("finalize-set-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("config.toml");
        fs::write(
            &f,
            "[paths]\nhandoffs_dir = \"~/myvault/internal/{project}/handoffs/\"  # note\npostmortems_dir = \"~/pm\"\n",
        )
        .unwrap();
        assert_eq!(
            read_path_setting(&f, "handoffs_dir").as_deref(),
            Some("~/myvault/internal/{project}/handoffs/")
        );
        assert_eq!(
            read_path_setting(&f, "postmortems_dir").as_deref(),
            Some("~/pm")
        );
        assert_eq!(read_path_setting(&f, "absent_key"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_path_setting_null_is_absent() {
        // emit_shell writes `postmortems_dir: null` for an unset path; reading it
        // as the literal "null" wrote `./null/` inside the repo (x-54c2). It must
        // read as absent so resolve_*_dir falls through to the `~/.fno` default.
        let dir = std::env::temp_dir().join(format!("finalize-null-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("config.toml");
        fs::write(&f, "[paths]\n").unwrap();
        assert_eq!(read_path_setting(&f, "postmortems_dir"), None);
        assert_eq!(read_path_setting(&f, "handoffs_dir"), None);

        // With the env override absent, the null settings value must resolve to
        // the absolute global default, never a relative `./null`.
        if std::env::var_os("POSTMORTEMS_DIR").is_none() {
            let home = PathBuf::from("/home/user");
            let resolved = resolve_postmortems_dir(None, Some(&f), Some(&home), &dir);
            assert_eq!(resolved, PathBuf::from("/home/user/.fno/postmortems"));
            assert!(resolved.is_absolute());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stuck_reasons_classify_the_eval_body_not_whether_it_fires() {
        for stuck in ["NoProgress", "Budget", "Interrupted", "Aborted"] {
            assert!(is_stuck_reason(stuck));
        }
        for not_stuck in ["DonePRGreen", "DoneAdvisory", "DoneDelivery", "NoWork"] {
            assert!(!is_stuck_reason(not_stuck));
        }
    }

    #[test]
    fn eval_fires_on_every_reason_but_nowork() {
        // x-8fc0: the trigger used to be STUCK_REASONS-only (a failure-only
        // sample). Verify by making it fail both ways - a successful
        // completion DOES get an eval, a stuck one still does too, and the
        // sole exclusion is NoWork (nothing happened, nothing to evaluate).
        for shipped in [
            "DonePRGreen",
            "DoneAdvisory",
            "DoneDelivery",
            "DoneBatched",
            "DoneAwaitingMerge",
            "DoneUnreviewed",
            "DoneAwaitingReview",
            "DonePlanned",
        ] {
            assert!(eval_should_fire(shipped), "expected eval for {shipped}");
        }
        for stuck in ["NoProgress", "Budget", "Interrupted", "Aborted"] {
            assert!(eval_should_fire(stuck), "expected eval for {stuck}");
        }
        assert!(!eval_should_fire("NoWork"));
    }

    #[test]
    fn resolve_postmortems_dir_prefers_override_then_settings_then_default() {
        let cwd = std::env::temp_dir().join(format!("finalize-pmdir-{}", std::process::id()));
        let _ = fs::create_dir_all(&cwd);
        let home = cwd.join("home");
        let ovr = cwd.join("explicit");
        std::env::remove_var("POSTMORTEMS_DIR");
        assert_eq!(
            resolve_postmortems_dir(Some(&ovr), None, Some(&home), &cwd),
            ovr,
            "explicit override wins"
        );
        // A `--settings` override file with postmortems_dir is honored (codex P2).
        let settings = cwd.join("custom-settings.toml");
        fs::write(&settings, "[paths]\npostmortems_dir = \"/srv/pm\"\n").unwrap();
        assert_eq!(
            resolve_postmortems_dir(None, Some(&settings), Some(&home), &cwd),
            PathBuf::from("/srv/pm"),
            "--settings postmortems_dir is honored"
        );
        // No override, no env, no settings -> ~/.fno/postmortems.
        assert_eq!(
            resolve_postmortems_dir(None, None, Some(&home), &cwd),
            home.join(".fno/postmortems")
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn assistant_text_blocks_handles_string_and_array() {
        let s = serde_json::json!({"message": {"content": "hi"}});
        assert_eq!(assistant_text_blocks(&s), "hi");
        let arr = serde_json::json!({"message": {"content": [
            {"type": "text", "text": "a"},
            {"type": "tool_use", "name": "x"},
            {"type": "text", "text": "b"}
        ]}});
        assert_eq!(assistant_text_blocks(&arr), "a b");
        // Top-level {"content": "..."} shape (codex P2 fallback).
        let top = serde_json::json!({"role": "assistant", "content": "top-level"});
        assert_eq!(assistant_text_blocks(&top), "top-level");
        assert_eq!(assistant_text_blocks(&serde_json::json!({})), "");
    }

    #[test]
    fn last_assistant_text_reads_top_level_content_shape() {
        // codex P2: a top-level {"role":"assistant","content":"..."} transcript
        // entry must yield its message, not "(transcript unavailable)".
        let dir = std::env::temp_dir().join(format!("finalize-lat-top-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let t = dir.join("transcript.jsonl");
        fs::write(
            &t,
            "{\"role\":\"assistant\",\"content\":\"top-level final\"}\n",
        )
        .unwrap();
        assert_eq!(last_assistant_text(&t).as_deref(), Some("top-level final"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_assistant_text_picks_newest_assistant_entry() {
        let dir = std::env::temp_dir().join(format!("finalize-lat-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let t = dir.join("transcript.jsonl");
        fs::write(
            &t,
            "{\"message\":{\"role\":\"assistant\",\"content\":\"old\"}}\n\
             {\"message\":{\"role\":\"user\",\"content\":\"ignored\"}}\n\
             {\"message\":{\"role\":\"assistant\",\"content\":\"newest\"}}\n",
        )
        .unwrap();
        assert_eq!(last_assistant_text(&t).as_deref(), Some("newest"));
        assert_eq!(last_assistant_text(&dir.join("missing.jsonl")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_postmortem_writes_artifact_with_reason_and_node() {
        let dir = std::env::temp_dir().join(format!("finalize-pm-{}", std::process::id()));
        let pmdir = dir.join("postmortems");
        let _ = fs::create_dir_all(&dir);
        let m = ManifestFields {
            graph_node_id: Some("ab-1a92b677".into()),
            plan_path: Some("plan.md".into()),
            input: Some("a stuck feature".into()),
            ..Default::default()
        };
        let path = write_postmortem(
            &dir,
            "20260607T010101Z-1-abc",
            &m,
            "NoProgress",
            None,
            Some(&pmdir),
            None,
            Some(&dir),
        )
        .expect("postmortem written");
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("termination: **NoProgress**"));
        assert!(body.contains("ab-1a92b677"));
        assert!(body.contains("a stuck feature"));
        assert!(body.contains("(transcript unavailable)"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_expected_url_count_cases() {
        let dir = std::env::temp_dir().join(format!("finalize-xpc-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        // Single-project always -> None (let stamp/graduate default to 1).
        assert_eq!(derive_expected_url_count(&dir, "plan.md", false), None);

        // Cross-project plan with a 2-key projects map (with nested sub-keys
        // that must NOT be counted) -> Some(2). (codex P1 regression.)
        let plan = dir.join("xproj.md");
        fs::write(
            &plan,
            "---\nstatus: ready\nscope: cross-project\nprojects:\n  alpha:\n    repo: a\n    branch: x\n  beta:\n    repo: b\nwaves:\n  - 1\n---\n# plan\n",
        )
        .unwrap();
        assert_eq!(
            derive_expected_url_count(&dir, "xproj.md", true),
            Some(2),
            "counts direct project keys only, not nested repo/branch"
        );

        // Cross-project but no projects map -> None so the caller skips graduate.
        let nomap = dir.join("nomap.md");
        fs::write(&nomap, "---\nstatus: ready\n---\n# plan\n").unwrap();
        assert_eq!(derive_expected_url_count(&dir, "nomap.md", true), None);

        let _ = fs::remove_dir_all(&dir);
    }

    // ── project name resolution (x-44e7) ──────────────────────────────────

    fn write_settings(dir: &Path, body: &str) {
        let cfg = dir.join(".fno");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(cfg.join("config.toml"), body).unwrap();
    }

    #[test]
    fn project_id_parses_nested_scalar() {
        let dir = std::env::temp_dir().join(format!("fin-projid-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("config.toml");
        // basename of the dir differs from project.id on purpose.
        fs::write(
            &f,
            "[project]\nid = \"fno\"\n[obsidian]\nid = \"ignored\"\n",
        )
        .unwrap();
        assert_eq!(read_project_id(&f).as_deref(), Some("fno"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_null_and_missing_are_unset() {
        let dir = std::env::temp_dir().join(format!("fin-projnull-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let null = dir.join("null.yaml");
        fs::write(&null, "[project]\n").unwrap();
        assert_eq!(read_project_id(&null), None, "null id -> unset");
        let empty = dir.join("empty.yaml");
        fs::write(&empty, "[project]\n").unwrap();
        assert_eq!(read_project_id(&empty), None, "no id key -> unset");
        assert_eq!(
            read_project_id(&dir.join("absent.yaml")),
            None,
            "missing file -> unset"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_project_name_prefers_project_id_over_basename() {
        // Dir basename is "footnote-like"; project.id is "fno".
        let dir = std::env::temp_dir().join(format!("fin-rpn-pref-{}", std::process::id()));
        let cwd = dir.join("footnote-like");
        let _ = fs::create_dir_all(&cwd);
        write_settings(&cwd, "[project]\nid = \"fno\"\n");
        let home = dir.join("home"); // no settings -> not consulted before cwd
        let _ = fs::create_dir_all(&home);
        assert_eq!(resolve_project_name(None, Some(&home), &cwd), "fno");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_project_name_falls_back_to_basename() {
        // No project.id anywhere -> git/cwd basename (here, the cwd dir name).
        let dir = std::env::temp_dir().join(format!("fin-rpn-fb-{}", std::process::id()));
        let cwd = dir.join("regready-ccld-pipeline");
        let _ = fs::create_dir_all(&cwd);
        let home = dir.join("home");
        let _ = fs::create_dir_all(&home);
        assert_eq!(
            resolve_project_name(None, Some(&home), &cwd),
            "regready-ccld-pipeline"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn slug_from_remote_url_variants() {
        // Mirrors the Python _remote_url_to_slug parity cases (paths.py).
        for (url, want) in [
            ("git@github.com:org/footnote.git", Some("footnote")),
            ("https://github.com/org/footnote.git", Some("footnote")),
            ("https://github.com/org/footnote", Some("footnote")),
            ("/srv/git/repo.git", Some("repo")),
            ("git@github.com:org/footnote.git/", Some("footnote")),
            (r"C:\repos\footnote.git", None), // backslash tail -> reject
            ("", None),
            ("   ", None),
        ] {
            assert_eq!(slug_from_remote_url(url).as_deref(), want, "url={url:?}");
        }
    }

    #[test]
    fn repo_identity_agrees_across_remote_forms() {
        // Parity with `repo_identity_from_remote_url` in fno/paths.py.
        // Every clone form of one repo must produce ONE key: the writer is Rust
        // and the reader is Python, so a disagreement means the reader silently
        // stops finding coverage and the merge gate refuses a reviewed PR.
        for url in [
            "git@github.com:org-a/widget.git",
            "https://github.com/org-a/widget",
            "https://github.com/org-a/widget.git/",
            "ssh://git@github.com:22/org-a/widget.git",
            "https://user:token@github.com/org-a/widget.git",
            "GIT@GitHub.com:Org-A/Widget.git",
        ] {
            assert_eq!(
                repo_identity_from_remote_url(url).as_deref(),
                Some("github.com/org-a/widget"),
                "url={url:?}"
            );
        }
    }

    #[test]
    fn repo_identity_separates_same_named_repos() {
        // The whole point of the full identity: a last-path-segment slug keys
        // both of these as "widget", and the global log is cross-project.
        assert_ne!(
            repo_identity_from_remote_url("git@github.com:org-a/widget.git"),
            repo_identity_from_remote_url("git@github.com:org-b/widget.git"),
        );
    }

    #[test]
    fn repo_identity_rejects_unusable_remotes() {
        // No host or fewer than two path segments cannot identify a repo across
        // projects; None makes the writer omit `repo`, and no reader claims it.
        for url in [
            "/local/path/widget.git",
            "git@github.com:widget.git",
            "",
            "   ",
        ] {
            assert_eq!(repo_identity_from_remote_url(url), None, "url={url:?}");
        }
    }

    #[test]
    fn resolve_project_name_prefers_git_remote_slug_over_basename() {
        // id-unset repo whose checkout is named differently from its remote:
        // the remote slug must win so this writer agrees with the Python side
        // (else it recreates internal/<basename>/ strays).
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("fin-rpn-slug-{}", std::process::id()));
        let cwd = dir.join("athens");
        let _ = fs::create_dir_all(&cwd);
        let home = dir.join("home");
        let _ = fs::create_dir_all(&home);
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "git@github.com:org/footnote.git"]);
        assert_eq!(resolve_project_name(None, Some(&home), &cwd), "footnote");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_project_name_local_over_global() {
        // Project-local project.id wins over the global one.
        let dir = std::env::temp_dir().join(format!("fin-rpn-lg-{}", std::process::id()));
        let cwd = dir.join("repo");
        let home = dir.join("home");
        let _ = fs::create_dir_all(&cwd);
        let _ = fs::create_dir_all(&home);
        write_settings(&cwd, "[project]\nid = \"fno\"\n");
        write_settings(&home, "[project]\nid = \"other\"\n");
        assert_eq!(resolve_project_name(None, Some(&home), &cwd), "fno");
        let _ = fs::remove_dir_all(&dir);
    }

    fn write_yaml(dir: &Path, name: &str, body: &str) -> PathBuf {
        let _ = fs::create_dir_all(dir);
        let f = dir.join(name);
        fs::write(&f, body).unwrap();
        f
    }

    #[test]
    fn project_id_ignores_false_positive_block_and_inline_comments() {
        // A `project:` under another section appears BEFORE config.project, and
        // both config: and project: carry inline comments (gemini HIGH).
        let dir = std::env::temp_dir().join(format!("fin-fp-{}", std::process::id()));
        let f = write_yaml(
            &dir,
            "s.yaml",
            "[other_tool.project]\nid = \"wrong\"\n\n[project]\nid = \"right\"\n",
        );
        assert_eq!(read_project_id(&f).as_deref(), Some("right"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_config_wins_over_legacy_top_level() {
        // Legacy top-level project.id is only a fallback; config.project wins
        // (config/__init__.py:1982-1990).
        let dir = std::env::temp_dir().join(format!("fin-legacy-{}", std::process::id()));
        let win = write_yaml(&dir, "win.yaml", "[project]\nid = \"canon\"\n");
        assert_eq!(read_project_id(&win).as_deref(), Some("canon"));
        // No canonical block -> legacy top-level is the fallback.
        let fb = write_yaml(&dir, "fb.toml", "[project]\nid = \"legacy\"\n");
        assert_eq!(read_project_id(&fb).as_deref(), Some("legacy"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_rejects_invalid_chars() {
        // A hand-edited id with a path separator must not reach a path segment
        // (codex P2; mirrors validate_project_id). Falls back to None.
        let dir = std::env::temp_dir().join(format!("fin-inval-{}", std::process::id()));
        let f = write_yaml(&dir, "s.toml", "[project]\nid = \"foo/bar\"\n");
        assert_eq!(read_project_id(&f), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_id_skips_grandchild_id_key() {
        // A deeper `id:` under a nested sub-mapping is not the project id.
        let dir = std::env::temp_dir().join(format!("fin-gc-{}", std::process::id()));
        let f = write_yaml(
            &dir,
            "s.yaml",
            "[project]\nid = \"good\"\n\n[project.nested]\nid = \"deep\"\n",
        );
        assert_eq!(read_project_id(&f).as_deref(), Some("good"));
        let _ = fs::remove_dir_all(&dir);
    }

    // x-b74b: from a linked worktree (has cli/src, but cli/.venv is gitignored
    // so it is NOT checked out) the interpreter must resolve the CANONICAL
    // repo's venv, and cli/src must anchor on the worktree - neither via
    // current_exe(). Reproduces the deployed-binary anchor failure.
    #[test]
    fn worktree_resolves_canonical_venv_and_own_cli_src() {
        fn git(cwd: &Path, args: &[&str]) -> bool {
            Command::new("git")
                .current_dir(cwd)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        let tmp = tempfile::tempdir().unwrap();
        let canon = tmp.path().join("canon");
        let wt = tmp.path().join("wt"); // sibling of canon, NOT nested inside it
        fs::create_dir_all(canon.join("cli/src/fno")).unwrap();
        fs::write(canon.join("cli/src/fno/__init__.py"), "").unwrap();

        if !git(&canon, &["init", "-q"]) {
            return; // no git available - nothing to assert
        }
        for kv in [
            "user.email=t@t",
            "user.name=t",
            "commit.gpgsign=false",
            "init.defaultBranch=main",
        ] {
            git(
                &canon,
                &[
                    "config",
                    kv.split('=').next().unwrap(),
                    kv.split('=').nth(1).unwrap(),
                ],
            );
        }
        git(&canon, &["add", "-A"]);
        assert!(git(&canon, &["commit", "-qm", "init"]), "commit failed");

        // Canonical carries the venv; a linked worktree does NOT (gitignored).
        fs::create_dir_all(canon.join("cli/.venv/bin")).unwrap();
        fs::write(canon.join("cli/.venv/bin/python3"), "").unwrap();

        assert!(
            git(&canon, &["worktree", "add", "-q", wt.to_str().unwrap()]),
            "worktree add failed"
        );
        assert!(
            wt.join("cli/src/fno/__init__.py").is_file(),
            "wt has cli/src"
        );
        assert!(!wt.join("cli/.venv/bin/python3").exists(), "wt lacks venv");

        // canonicalize both sides: git's --path-format=absolute returns the
        // realpath (/private/var on macOS) while the temp path is /var.
        let real = |p: &str| fs::canonicalize(p).unwrap();
        assert_eq!(
            real(&py_interpreter(&wt)),
            real(canon.join("cli/.venv/bin/python3").to_str().unwrap())
        );
        assert_eq!(
            real(&repo_cli_src(&wt).unwrap()),
            real(wt.join("cli/src").to_str().unwrap())
        );
    }

    // codex P2: a foreign (non-footnote) project cwd that happens to carry its
    // own cli/.venv but NO fno package must NOT be selected as the interpreter -
    // `import fno` would fail. footnote_venv gates on the co-located
    // cli/src/fno/__init__.py, so this venv is rejected and resolution falls
    // through (never rooting under the foreign project).
    #[test]
    fn foreign_cwd_venv_without_fno_package_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let foreign = tmp.path().join("foreign");
        fs::create_dir_all(foreign.join("cli/.venv/bin")).unwrap();
        fs::write(foreign.join("cli/.venv/bin/python3"), "").unwrap();
        // deliberately NO cli/src/fno/__init__.py -> not a footnote checkout.
        assert_eq!(footnote_venv(&foreign), None);
        let interp = py_interpreter(&foreign);
        assert!(
            !interp.starts_with(foreign.to_str().unwrap()),
            "must not pick the foreign venv, got {interp}"
        );
    }

    // ── x-dbaf run_summary ──────────────────────────────────────────────────

    #[test]
    fn count_run_tasks_correlates_on_run_and_flags_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        fs::write(
            &events,
            "{\"type\":\"task_started\",\"run\":\"R1\",\"data\":{}}\n\
             {\"type\":\"task_started\",\"run\":\"R1\",\"data\":{}}\n\
             {\"type\":\"task_done\",\"run\":\"R1\",\"outcome\":\"SUCCESS\",\"data\":{}}\n\
             {\"type\":\"task_done\",\"run\":\"R1\",\"outcome\":\"FAILED\",\"data\":{}}\n\
             {\"type\":\"task_started\",\"run\":\"OTHER\",\"data\":{}}\n\
             not json\n",
        )
        .unwrap();
        // R1: 2 started, 2 done, 1 failed; the OTHER-run line and the junk line
        // are ignored.
        assert_eq!(count_run_tasks(&events, "R1"), (2, 2, 1));
    }

    #[test]
    fn emit_run_summary_writes_extended_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        // pre-seed one started with no matching done -> exposes the gap (AC2-FR).
        fs::write(
            &events,
            "{\"type\":\"task_started\",\"run\":\"R9\",\"data\":{}}\n",
        )
        .unwrap();
        emit_run_summary(
            &events,
            &events,
            "R9",
            Some("prj-0001"),
            true,
            "DonePRGreen",
            None,
        );
        let content = fs::read_to_string(&events).unwrap();
        let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "run_summary");
        assert_eq!(last["v"], 1);
        assert_eq!(last["run"], "R9");
        assert_eq!(last["node"], "prj-0001");
        assert_eq!(last["outcome"], "SUCCESS");
        assert_eq!(last["data"]["tasks_started"], 1);
        assert_eq!(last["data"]["tasks_done"], 0);
        assert_eq!(last["data"]["termination_reason"], "DonePRGreen");
    }

    #[test]
    fn emit_run_summary_non_ship_is_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let events = tmp.path().join("events.jsonl");
        emit_run_summary(&events, &events, "R2", None, false, "NoProgress", None);
        let content = fs::read_to_string(&events).unwrap();
        let ev: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(ev["outcome"], "FAILED");
        assert!(ev.get("node").is_none(), "no node -> omitted, not null");
    }
}
