//! `fno-agents hook stop` - the Stop hook's native handler.
//!
//! Owns the translation the 451-line shell shim carried: payload read,
//! ownership (one native evaluation: target manifest, pending delivery, king
//! manifest), the bounded-block counters, the foreign-session guard, the
//! cargo build-dir export, the in-process decide call, harness-shaped block
//! output, and terminal cleanup (claim release, finalize, delivery retry).
//! The stop/allow decision itself stays in `loopcheck.rs`; this module is
//! transport plus the shell's translation, with the ownership code salvaged
//! from the abandoned WIP `stop_gate.rs` (its registry scan replaced by the one
//! matcher, `loop_reign::find_by_session`).

use serde::Serialize;
use serde_json::Value;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::distress;
use crate::manifest_lookup::{git_worktree_paths, parse_manifest_identity, paths_eq};
use crate::paths::{events_path, worktree_repo_root, worktree_space_dir};

/// Consecutive checker-unavailable fires tolerated for an active session
/// before a loud give-up allow (the shim's `MAX_UNAVAIL_RETRIES`).
const MAX_UNAVAIL_RETRIES: u64 = 3;

pub fn run(args: &[String]) -> i32 {
    let _ = args;
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        // An unreadable payload is not a verdict for an owner, but the shell
        // treated it as an empty read and continued; with no identity, the
        // ownership evaluation answers no-owner and the stop allows.
        eprintln!("target stop-hook: unreadable payload; allowing");
        return 0;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let parsed: Option<Value> = serde_json::from_str(payload.trim()).ok();
    let str_field = |key: &str| -> String {
        parsed
            .as_ref()
            .and_then(|v| v.get(key).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
    };
    let session_id = str_field("session_id");
    let turn_id = str_field("turn_id");
    let goal_payload = parsed.as_ref().and_then(goal_payload).cloned();
    let transport_path = str_field("transcript_path");
    let payload_cwd = str_field("cwd");
    let last_assistant_message = parsed.as_ref().and_then(|v| {
        v.get("last_assistant_message")
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    // grok sends camelCase keys with snake_case copies. Its transcript path
    // names updates.jsonl, not the chat_history.jsonl store the reader needs,
    // so normalize before the ownership read. A payload without grok's
    // `hookEventName: "stop"` is not a grok fire and none of this moves:
    // claude, codex and gemini read snake_case and resolve the transcript
    // straight from the payload.
    let mut session_id = session_id;
    let mut transcript_path = transport_path;
    let mut payload_cwd = payload_cwd;
    let mut last_assistant_message = last_assistant_message;
    let mut payload = payload;
    if let Some(grok) = parsed.as_ref().and_then(normalize_grok_envelope) {
        match grok {
            GrokFire::Skip => return 0,
            GrokFire::Turn {
                session_id: sid,
                cwd: grok_cwd,
                last_assistant_message: lam,
                transcript_path: store,
                store_note,
            } => {
                if !store_note.is_empty() {
                    eprintln!("target stop-hook: grok session store read for {sid}: {store_note}");
                }
                session_id = sid;
                transcript_path = store.display().to_string();
                payload_cwd = grok_cwd;
                last_assistant_message = lam.clone();
                // loop-check's intent channel reads snake_case off the stdin
                // JSON, so rewrite the payload with grok's final text under the
                // snake_case key. No loopcheck.rs edit: that file is over the
                // shrink-only budget, and the rewrite makes one unnecessary.
                if let Ok(mut v) = serde_json::from_str::<Value>(payload.trim()) {
                    if let Some(text) = lam {
                        v["last_assistant_message"] = Value::String(text);
                    }
                    payload = v.to_string();
                }
            }
        }
    }

    let cwd = if payload_cwd.is_empty() {
        cwd
    } else {
        std::fs::canonicalize(&payload_cwd).unwrap_or(PathBuf::from(&payload_cwd))
    };

    let fire = collect_fire(
        &session_id,
        &transcript_path,
        last_assistant_message,
        turn_id,
        goal_payload,
    );

    // ── Ownership (the salvaged stop-gate evaluation) ─────────────────────────
    match evaluate(&cwd, &fire) {
        Verdict::NoOwner => {
            emit_stop_decision(&cwd, &fire, None, "visitor", "none", "allow", "visitor", "");
            return 0;
        }
        Verdict::Broken(kind) => return broken_block(&cwd, &fire, &kind),
        Verdict::Owner {
            state,
            driver,
            owner_cwd,
        } => run_owned(&cwd, &fire, &payload, state, driver, owner_cwd),
    }
}

/// A grok Stop fire, normalized onto the snake_case vocabulary the handler
/// already reads. `Skip` is a fire that is not the session's turn gate: a
/// subagent's stop, or the session-end observe-only fire with no `promptId`.
enum GrokFire {
    Skip,
    Turn {
        session_id: String,
        cwd: String,
        last_assistant_message: Option<String>,
        transcript_path: PathBuf,
        store_note: String,
    },
}

/// Recognize a grok Stop payload and map it onto the snake_case fields. Fires
/// only on grok's camelCase `hookEventName: "stop"`; claude and codex send no
/// such key, so `None` leaves them on today's path. grok's own
/// `transcript_path` copy names updates.jsonl, so the store path comes from
/// the chat_history.jsonl lookup below instead. A subagent stop and the
/// session-end fire (no `promptId`) answer `Skip`: neither is this session's
/// turn gate, so `hook stop` exits 0 with no output and no event.
fn normalize_grok_envelope(parsed: &Value) -> Option<GrokFire> {
    if parsed.get("hookEventName").and_then(Value::as_str) != Some("stop") {
        return None;
    }
    if parsed.get("subagentType").is_some() {
        return Some(GrokFire::Skip);
    }
    if parsed.get("promptId").is_none() {
        return Some(GrokFire::Skip);
    }
    let session_id = parsed
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            std::env::var("GROK_SESSION_ID")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default();
    let cwd = parsed
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| parsed.get("workspaceRoot").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let lam = parsed
        .get("lastAssistantMessage")
        .and_then(Value::as_str)
        .map(str::to_string);
    let root = crate::grok_store::grok_sessions_root();
    let (transcript_path, store_note) = match crate::grok_store::lookup_session(&root, &session_id)
    {
        crate::pi::SessionLookup::One { file } => (file, String::new()),
        crate::pi::SessionLookup::None => (PathBuf::new(), "none".to_string()),
        crate::pi::SessionLookup::Duplicate { .. } => (PathBuf::new(), "duplicate".to_string()),
        crate::pi::SessionLookup::Unknown { .. } => (PathBuf::new(), "unreadable".to_string()),
    };
    Some(GrokFire::Turn {
        session_id,
        cwd,
        last_assistant_message: lam,
        transcript_path,
        store_note,
    })
}

/// The owned path: the foreign-session guard, the build-dir export, the
/// in-process decide call, and the decision translation.
fn run_owned(
    hook_cwd: &Path,
    fire: &Fire,
    payload: &str,
    state: PathBuf,
    driver: &'static str,
    owner_cwd: PathBuf,
) -> i32 {
    // ── Pending delivery: the DoneDelivery retry decision without decide ──────
    let pending = delivery_pending_for(hook_cwd, fire, Some(&state));
    let state_is_pending = pending.as_ref().is_some_and(|p| *p == state);

    // One read of the manifest feeds the guard below, the diagnostics, and
    // translate's claim release.
    let manifest = if state_is_pending {
        String::new()
    } else {
        std::fs::read_to_string(&state).unwrap_or_default()
    };

    // ── The foreign-session guard (PR #388 fix class) ─────────────────────────
    // The manifest's stamped harness id (claude_session_id, then
    // claude_transcript_id, then harness_session_id) must name THIS
    // transcript. Codex rollout suffixes count.
    if !state_is_pending {
        let manifest_ctid =
            first_raw_field(&manifest, &["claude_session_id", "claude_transcript_id"])
                .filter(|v| v != "null" && !v.is_empty())
                .or_else(|| {
                    first_raw_field(&manifest, &["harness_session_id"])
                        .filter(|v| v != "null" && !v.is_empty())
                })
                .unwrap_or_default();
        if !manifest_ctid.is_empty() {
            let basename = transcript_basename(&fire.transcript_path.display().to_string())
                .unwrap_or_default();
            // An empty basename (grok with an unreadable store) is not
            // foreign: the transcript_ok unavailable block owns that answer.
            if !basename.is_empty()
                && basename != manifest_ctid
                && !basename.ends_with(&format!("-{manifest_ctid}"))
            {
                // Another session's manifest; genuinely not ours to judge.
                return 0;
            }
        }
    }

    // ── An active owner with no transcript file: unavailable bounded block ────
    let transcript_ok =
        !fire.transcript_path.as_os_str().is_empty() && fire.transcript_path.is_file();
    if !transcript_ok {
        return unavailable_block(
            hook_cwd,
            fire,
            driver,
            "no transcript for an active session",
        );
    }

    let codex_owner = fire.harness.as_deref() == Some("codex")
        || first_raw_field(&manifest, &["harness"]).as_deref() == Some("codex");
    let arbitration = if codex_owner {
        arbitrate_codex_continuation(driver, fire, &manifest)
    } else {
        arbitrate_continuation(driver, fire, &manifest)
    };
    match arbitration {
        GoalArbitration::Delegated => {
            emit_stop_decision(
                hook_cwd,
                fire,
                Some(&state),
                driver,
                "goal",
                "allow",
                "delegated-to-goal",
                &manifest,
            );
            return 0;
        }
        GoalArbitration::Refusal(reason) => {
            emit_stop_decision(
                hook_cwd,
                fire,
                Some(&state),
                driver,
                "refusal",
                "refuse",
                "refusal",
                &manifest,
            );
            return emit_block_for_harness(&reason);
        }
        GoalArbitration::None => {}
    }

    // ── done_probes inherit the session cargo build-dir env ───────────
    export_session_build_dir(driver, &owner_cwd);

    // ── The pending-delivery retry decision, without calling decide ───────────
    if state_is_pending {
        let out = serde_json::json!({
            "decision": "allow",
            "termination_reason": "DoneDelivery",
            "message": "retrying generic delivery finalization"
        })
        .to_string();
        return translate(hook_cwd, &out, driver, &state, &owner_cwd, fire, "");
    }

    // ── Decide, in process ─────────────────────────────────────────────────
    // No settle sweep (it exits 2 on every fire; settlement belongs to the
    // review lifecycle), no jq, no second binary: the payload rides memory.
    let args = vec![
        "loop-check".to_string(),
        "--driver".to_string(),
        driver.to_string(),
        "--state".to_string(),
        state.display().to_string(),
        "--transcript".to_string(),
        fire.transcript_path.display().to_string(),
        "--cwd".to_string(),
        owner_cwd.display().to_string(),
        "--hook-input-stdin".to_string(),
    ];
    let parsed = match crate::loopcheck::parse_args(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("target stop-hook: loop-check args rejected: {e}");
            return unavailable_block(hook_cwd, fire, driver, "argument error");
        }
    };
    let (verb_rc, decision_json) = crate::loopcheck::decide_with_payload(&parsed, Some(payload));

    // A non-zero exit means a BROKEN checker only when the output carries no
    // verdict. The king driver sends a full decision payload on both block
    // shapes, so key on the field, never the code.
    let has_decision = serde_json::from_str::<Value>(decision_json.trim())
        .is_ok_and(|v| v.get("decision").is_some());
    if !has_decision {
        if verb_rc != 0 {
            eprintln!(
                "target stop-hook: WARNING: fno-agents loop-check exited {verb_rc} with no decision for an active session"
            );
        } else {
            eprintln!(
                "target stop-hook: WARNING: fno-agents loop-check returned unexpected output (not JSON) for an active session"
            );
        }
        if !decision_json.is_empty() {
            eprintln!("target stop-hook: loop-check output: {decision_json}");
        }
        tail_stderr_log(hook_cwd);
        return unavailable_block(hook_cwd, fire, driver, "checker produced no verdict");
    }

    translate(
        hook_cwd,
        &decision_json,
        driver,
        &state,
        &owner_cwd,
        fire,
        &manifest,
    )
}

/// The decision translation: counters, tick row, harness-shaped block, and
/// terminal cleanup. `manifest` is the state file's content when an upstream
/// step already read it (empty on the pending-retry entry, which re-reads).
fn translate(
    hook_cwd: &Path,
    decision_json: &str,
    driver: &'static str,
    state: &Path,
    owner_cwd: &Path,
    fire: &Fire,
    manifest: &str,
) -> i32 {
    let v: Value = serde_json::from_str(decision_json.trim()).unwrap_or(Value::Null);
    let decision = v.get("decision").and_then(Value::as_str).unwrap_or("allow");
    let message = v
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let termination_reason = v
        .get("termination_reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // ── Clean decision reached: self-heal the unavailable counter ─────────────
    let space = super::events_space(hook_cwd);
    let counter = space.join(format!(".loop-check-unavail-{}", fire.hook_harness_id));
    let _ = std::fs::remove_file(&counter);

    // One control-plane arm row for this fire.
    emit_tick(hook_cwd, decision, &termination_reason, driver);

    let event_decision = match decision {
        "block" | "allow" => decision,
        _ => "refuse",
    };
    let event_class = if decision == "block" {
        "actionable-block"
    } else if !termination_reason.is_empty() {
        "terminal"
    } else {
        "live-allow"
    };
    emit_stop_decision(
        hook_cwd,
        fire,
        Some(state),
        driver,
        "loop_check",
        event_decision,
        event_class,
        manifest,
    );

    // ── Block ─────────────────────────────────────────────────────────────────
    if decision == "block" {
        return emit_block_for_harness(&message);
    }

    // ── Terminal ──────────────────────────────────────────────────────────────
    if !termination_reason.is_empty() {
        if driver == "king" {
            eprintln!(
                "target stop-hook: king terminal ({termination_reason}); the king loop has no plan to stamp"
            );
            return 0;
        }
        let reason = parse_reason(&termination_reason);
        if reason.releases_claim() {
            // Release BEFORE finalize: both stamp a `do` row for the same
            // session, and sessions[] is append-only, so the release row must
            // land first to carry its ended_at window.
            let content = if manifest.is_empty() {
                std::fs::read_to_string(state).unwrap_or_default()
            } else {
                manifest.to_string()
            };
            let key = first_raw_field(&content, &["target_claim_key"]).unwrap_or_default();
            let holder = first_raw_field(&content, &["target_claim_holder"]).unwrap_or_default();
            if key.starts_with("node:") && !holder.is_empty() {
                let _ = crate::claims::release(&key, &holder, None, None);
            }
        }
        let mut finalize_state = state.to_path_buf();
        if termination_reason == "DoneDelivery" {
            // The pending retry state must EXIST before finalize runs: when the
            // terminal was decided from the manifest (not an existing pending
            // file), the state is copied to the derived pending path - the
            // shell's routine candidate copy, now made only where the delivery
            // transaction needs it.
            let copied = delivery_pending_for(hook_cwd, fire, Some(state))
                .or_else(|| pending_state_path(hook_cwd, fire, state))
                .filter(|p| std::fs::copy(state, p).is_ok());
            let Some(pending_path) = copied else {
                return emit_block_for_harness(
                    "generic delivery state could not be preserved; will retry",
                );
            };
            finalize_state = pending_path;
        }
        let transcript = fire.transcript_path.display().to_string();
        let fargs = vec![
            "--state".to_string(),
            finalize_state.display().to_string(),
            "--transcript".to_string(),
            transcript,
            "--cwd".to_string(),
            owner_cwd.display().to_string(),
            "--reason".to_string(),
            termination_reason.clone(),
        ];
        if termination_reason == "DoneDelivery" {
            if crate::finalize::run_finalize(&fargs) != 0 {
                return emit_block_for_harness("generic delivery finalization failed; will retry");
            }
            let pending = delivery_pending_for(hook_cwd, fire, Some(state));
            if let Some(p) = pending {
                if std::fs::remove_file(&p).is_err() {
                    return emit_block_for_harness(
                        "generic delivery retry state cleanup failed; will retry",
                    );
                }
            }
        }
    }

    // ── Allow ─────────────────────────────────────────────────────────────────
    if !message.is_empty() {
        eprintln!("target stop-hook: {message}");
    }
    0
}

fn parse_reason(s: &str) -> crate::loopcheck::TerminationReason {
    use crate::loopcheck::TerminationReason as T;
    // Debug-formatted names: `serde_json` serializes the enum through its
    // Display? No: loopcheck emits `format!("{reason:?}")`, so the JSON
    // carries the Debug name. Parse those.
    match s {
        "DonePRGreen" => T::DonePRGreen,
        "DoneAdvisory" => T::DoneAdvisory,
        "DoneDelivery" => T::DoneDelivery,
        "NoWork" => T::NoWork,
        "DonePlanned" => T::DonePlanned,
        "DoneBatched" => T::DoneBatched,
        "DoneUnreviewed" => T::DoneUnreviewed,
        "DoneAwaitingMerge" => T::DoneAwaitingMerge,
        "DoneAwaitingReview" => T::DoneAwaitingReview,
        "Budget" => T::Budget,
        "NoProgress" => T::NoProgress,
        "Interrupted" => T::Interrupted,
        "Aborted" => T::Aborted,
        _ => T::NoWork,
    }
}

/// The broken-resolver bounded block: today's counter files and messages, at
/// most 3 consecutive fires, then allow loudly.
fn broken_block(cwd: &Path, fire: &Fire, kind: &str) -> i32 {
    let space = super::events_space(cwd);
    let _ = std::fs::create_dir_all(&space);
    // The king counter keys on the transcript id (SESSION_ID is not derived
    // until a state file is chosen).
    let id = if kind == "king" {
        &fire.hook_harness_id
    } else {
        &fire.resolve_harness_id
    };
    let name = format!(
        ".{}-unavail-{}",
        if kind == "king" {
            "king-resolve"
        } else {
            "loop-check"
        },
        if id.is_empty() { "anon" } else { id.as_str() }
    );
    let counter = space.join(name);
    let count = std::fs::read_to_string(&counter)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    let _ = std::fs::write(&counter, count.to_string());
    if count <= MAX_UNAVAIL_RETRIES {
        let msg = if kind == "king" {
            format!(
                "king manifest resolver unavailable ({count}/{MAX_UNAVAIL_RETRIES}) for an active kings dir, keeping session running"
            )
        } else {
            format!("checker unavailable ({count}/{MAX_UNAVAIL_RETRIES}), keeping session running")
        };
        emit_stop_decision(
            cwd,
            fire,
            None,
            "unknown",
            "refusal",
            "block",
            "unavailable",
            "",
        );
        return emit_block_for_harness(&msg);
    }
    if kind == "king" {
        eprintln!(
            "target stop-hook: king manifest resolver unavailable {count} times (counter {}); allowing stop. The counter only increments, so the king gate stays OFF for the rest of this session. Delete {} to re-arm it.",
            counter.display(),
            counter.display()
        );
    } else {
        eprintln!(
            "target stop-hook: manifest resolver unavailable {count} times; allowing visitor stop"
        );
    }
    emit_stop_decision(
        cwd,
        fire,
        None,
        "unknown",
        "refusal",
        "allow",
        "unavailable",
        "",
    );
    0
}

/// The checker-unavailable bounded block for an active session.
fn unavailable_block(cwd: &Path, fire: &Fire, driver: &str, why: &str) -> i32 {
    let space = super::events_space(cwd);
    let _ = std::fs::create_dir_all(&space);
    let counter = space.join(format!(".loop-check-unavail-{}", fire.hook_harness_id));
    let count = std::fs::read_to_string(&counter)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    let _ = std::fs::write(&counter, count.to_string());
    emit_tick(cwd, "blocked", "unavailable", driver);
    if count <= MAX_UNAVAIL_RETRIES {
        emit_stop_decision(
            cwd,
            fire,
            None,
            driver,
            "refusal",
            "block",
            "unavailable",
            "",
        );
        return emit_block_for_harness(&format!(
            "checker unavailable ({count}/{MAX_UNAVAIL_RETRIES}), keeping session running"
        ));
    }
    eprintln!(
        "target stop-hook: {why}; checker unavailable {count} times; allowing stop (ship gate off for this stop)"
    );
    emit_stop_decision(
        cwd,
        fire,
        None,
        driver,
        "refusal",
        "allow",
        "unavailable",
        "",
    );
    0
}

fn tail_stderr_log(cwd: &Path) {
    let space = super::events_space(cwd);
    let log = space.join("loop-check.stderr.log");
    if let Ok(content) = std::fs::read_to_string(&log) {
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(5);
        for line in &lines[start..] {
            eprintln!("{line}");
        }
    }
}

/// The block shape the CALLING harness honors: claude reads stdout JSON at
/// exit 0; codex/gemini read only the exit code, so exit 2 stays their block
/// signal. A foreign marker makes the claude claim ambiguous, so the legacy
/// exit-2 path runs instead.
fn emit_block_for_harness(reason: &str) -> i32 {
    let claude = std::env::var("CLAUDECODE").as_deref() == Ok("1")
        || std::env::var_os("CLAUDE_PLUGIN_ROOT").is_some_and(|v| !v.is_empty());
    let foreign = [
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
    if claude && !foreign {
        let out = serde_json::json!({ "decision": "block", "reason": reason });
        println!("{out}");
        return 0;
    }
    eprintln!("target stop-hook: {reason}");
    2
}

/// One control-plane arm row for this fire.
fn emit_tick(cwd: &Path, decision: &str, reason: &str, driver: &str) {
    let project_events = events_path(cwd);
    let global_events = std::env::var_os("GLOBAL_EVENTS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fno/events.jsonl")))
        .unwrap_or_else(|| project_events.clone());
    let detail = format!(
        "driver={driver} decision={decision} reason={}",
        if reason.is_empty() { "live" } else { reason }
    );
    let data = serde_json::json!({
        "arm": "stop_hook",
        "scheduler": "hook:target-stop-hook",
        "acted": 1,
        "skip_reason": Value::Null,
        "detail": detail,
        "interval_s": 0,
    });
    crate::loopcheck::emit_to_both(&project_events, &global_events, "control_plane_tick", data);
}

///: the CARGO_BUILD_BUILD_DIR value, ported from
/// `cli/src/fno/paths.py cargo_build_dir_value`: config
/// `paths.cargo_targets_base`, else `<state_dir>/cargo-build`, then
/// `/{workspace-path-hash}` - cargo expands the template itself.
fn cargo_build_dir_value(cwd: &Path) -> String {
    let base = crate::agents_config::config_lookup(cwd, &["paths", "cargo_targets_base"])
        .and_then(|v| v.as_str().map(str::to_string))
        .map(|raw| {
            let p = PathBuf::from(shellexpand_home(&raw));
            if p.is_absolute() {
                p
            } else {
                cwd.join(p)
            }
        })
        .or_else(|| crate::agents_config::state_dir(cwd).map(|d| d.join("cargo-build")))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".fno").join("cargo-build"))
                .unwrap_or_else(|| PathBuf::from(".fno/cargo-build"))
        });
    format!("{}/{{workspace-path-hash}}", base.display())
}

fn shellexpand_home(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest).display().to_string();
        }
    }
    raw.to_string()
}

/// The bash hook exported this for its loop-check child; the native handler IS
/// that process, so it sets the env in place and every done_probe it spawns
/// inherits it. A session preset wins, and a value that does not carry the
/// cargo template is never exported (it cannot be a build-dir answer).
fn export_session_build_dir(driver: &str, owner_cwd: &Path) {
    if driver == "target" && std::env::var_os("CARGO_BUILD_BUILD_DIR").is_none() {
        let v = cargo_build_dir_value(owner_cwd);
        if v.ends_with("/{workspace-path-hash}") {
            std::env::set_var("CARGO_BUILD_BUILD_DIR", v);
        }
    }
}

// ── Salvaged ownership (stop_gate.rs, WIP c562f97276) ────────────────────────

/// What the ownership evaluation decided.
enum Verdict {
    NoOwner,
    Owner {
        state: PathBuf,
        driver: &'static str,
        owner_cwd: PathBuf,
    },
    Broken(&'static str),
}

/// The payload-derived identity of the stopping session, collected exactly
/// the way the shim collected it (most authoritative first).
struct Fire {
    session_id: String,
    transcript_path: PathBuf,
    hook_harness_id: String,
    resolve_ids: Vec<String>,
    resolve_harness_id: String,
    harness: Option<String>,
    last_assistant_message: Option<String>,
    turn_id: String,
    goal_payload: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalTruth {
    objective: String,
    status: String,
    continuation_owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GoalArbitration {
    None,
    Delegated,
    Refusal(String),
}

#[derive(Debug, Serialize)]
struct StopDecisionEvent {
    session_id: String,
    raw_identity_candidates: Vec<String>,
    turn_id: String,
    manifest: String,
    scope: String,
    node_id: String,
    driver: String,
    continuation_owner: String,
    decision: String,
    class: String,
    correlation_id: String,
    harness_output_contract: String,
}

fn collect_fire(
    session_id: &str,
    transcript_path: &str,
    last_assistant_message: Option<String>,
    turn_id: String,
    goal_payload: Option<Value>,
) -> Fire {
    let hook_harness_id = transcript_basename(transcript_path).unwrap_or_default();
    let hook_harness_id = if hook_harness_id.is_empty() {
        session_id.to_string()
    } else {
        hook_harness_id
    };
    // Codex names its rollout transcript rollout-<utc>-<thread-uuid>; the
    // resolver wants the bare uuid suffix, when there is one.
    let codex_uuid = uuid_suffix(&hook_harness_id).unwrap_or_default();
    let codex_env_id = std::env::var("CODEX_THREAD_ID").ok().filter(|tid| {
        !tid.is_empty()
            && (hook_harness_id == *tid || hook_harness_id.ends_with(&format!("-{tid}")))
    });
    let mut resolve_ids: Vec<String> = Vec::new();
    for candidate in [
        Some(session_id.to_string()),
        codex_env_id,
        (!codex_uuid.is_empty()).then_some(codex_uuid),
        (!hook_harness_id.is_empty()).then_some(hook_harness_id.clone()),
    ]
    .into_iter()
    .flatten()
    {
        if !candidate.is_empty() && !resolve_ids.contains(&candidate) {
            resolve_ids.push(candidate);
        }
    }
    let resolve_harness_id = resolve_ids.first().cloned().unwrap_or_default();
    Fire {
        session_id: session_id.to_string(),
        transcript_path: PathBuf::from(transcript_path),
        hook_harness_id,
        resolve_ids,
        resolve_harness_id,
        harness: detect_harness(transcript_path, !turn_id.is_empty()),
        last_assistant_message,
        turn_id,
        goal_payload,
    }
}

fn goal_payload(value: &Value) -> Option<&Value> {
    value
        .get("goal")
        .or_else(|| value.get("native_goal"))
        .or_else(|| value.get("provider_goal"))
        .or_else(|| {
            (value.get("goal_status").is_some()
                || value.get("goal_objective").is_some()
                || value.get("continuation_owner").is_some())
            .then_some(value)
        })
}

fn parse_goal_value(value: &Value) -> Result<Option<GoalTruth>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or("goal truth is unreadable: expected an object")?;
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let objective = text(&["objective", "goal"]);
    let mut status = text(&["status"]);
    if status.is_empty() {
        status = "active".into();
    }
    if !matches!(status.as_str(), "active" | "paused" | "completed" | "done") {
        return Err(format!("goal truth has unknown status {status:?}"));
    }
    if object.get("verified").and_then(Value::as_bool) == Some(false) {
        return Err("goal truth is not verified".into());
    }
    if objective.is_empty() {
        return Err("goal truth is missing objective".into());
    }
    Ok(Some(GoalTruth {
        objective,
        status: if status == "done" {
            "completed".into()
        } else {
            status
        },
        continuation_owner: text(&["continuationOwner", "continuation_owner", "owner"]),
    }))
}

fn manifest_goal(content: &str) -> Result<Option<GoalTruth>, String> {
    let objective = first_raw_field(
        content,
        &["goal_objective", "native_goal_objective", "goal"],
    );
    let status = first_raw_field(content, &["goal_status", "native_goal_status"]);
    let owner = first_raw_field(content, &["goal_continuation_owner", "goal_owner"]);
    let verified = first_raw_field(content, &["goal_verified", "native_goal_verified"]);
    if objective.is_none() && status.is_none() && owner.is_none() && verified.is_none() {
        return Ok(None);
    }
    if verified.as_deref() == Some("false") {
        return Err("manifest goal truth is not verified".into());
    }
    Ok(Some(GoalTruth {
        objective: objective
            .filter(|v| !v.is_empty())
            .ok_or("manifest goal truth is missing objective")?,
        status: status.unwrap_or_else(|| "active".into()),
        continuation_owner: owner.unwrap_or_default(),
    }))
}

fn merge_goal_truth(payload: Option<&Value>, manifest: &str) -> Result<Option<GoalTruth>, String> {
    let from_payload = payload.map(parse_goal_value).transpose()?.flatten();
    let from_manifest = manifest_goal(manifest)?;
    match (from_payload, from_manifest) {
        (Some(left), Some(right)) if left != right => Err(format!(
            "conflicting goal truth: payload {:?}, manifest {:?}",
            left, right
        )),
        (Some(goal), _) | (_, Some(goal)) => Ok(Some(goal)),
        (None, None) => Ok(None),
    }
}

fn arbitrate_continuation(driver: &str, fire: &Fire, manifest: &str) -> GoalArbitration {
    let goal = match merge_goal_truth(fire.goal_payload.as_ref(), manifest) {
        Ok(goal) => goal,
        Err(reason) => return GoalArbitration::Refusal(reason),
    };
    arbitrate_goal_truth(driver, manifest, goal)
}

fn arbitrate_codex_continuation(driver: &str, fire: &Fire, manifest: &str) -> GoalArbitration {
    if driver != "king" {
        return GoalArbitration::None;
    }
    let live = crate::reign_goal::read_codex_goal_for_stop(&fire.session_id).map(|goal| goal);
    arbitrate_codex_continuation_from_reading(driver, fire, manifest, live)
}

fn arbitrate_codex_continuation_from_reading(
    driver: &str,
    fire: &Fire,
    manifest: &str,
    live: Result<Option<crate::codex_thread::NativeGoal>, String>,
) -> GoalArbitration {
    if driver != "king" {
        return GoalArbitration::None;
    }
    let Some(expected_session) = first_raw_field(manifest, &["harness_session_id"]) else {
        return GoalArbitration::Refusal(
            "Codex Stop manifest has no exact harness session id".into(),
        );
    };
    if expected_session != fire.session_id {
        return GoalArbitration::Refusal(format!(
            "Codex Stop session mismatch: expected {expected_session:?}, got {:?}",
            fire.session_id
        ));
    }
    let live = match live {
        Ok(Some(goal)) => goal,
        Ok(None) => return GoalArbitration::None,
        Err(reason) => {
            return GoalArbitration::Refusal(format!("Codex provider goal unreadable: {reason}"))
        }
    };
    if live.thread_id != fire.session_id {
        return GoalArbitration::Refusal(format!(
            "Codex provider goal thread mismatch: expected {:?}, got {:?}",
            fire.session_id, live.thread_id
        ));
    }
    let Some(owner) = expected_continuation_owner(driver, manifest) else {
        return GoalArbitration::Refusal(
            "active Codex goal owner is not defined by the manifest".into(),
        );
    };
    let Some(scope) = first_raw_field(manifest, &["scope", "crown_scope"]) else {
        return GoalArbitration::Refusal("active Codex goal has no crown scope".into());
    };
    let expected_owner = format!("king:{}", scope.trim());
    if owner != expected_owner {
        return GoalArbitration::Refusal(format!(
            "active Codex goal owner must derive from crown scope: expected {expected_owner:?}, got {owner:?}"
        ));
    }
    let expected_objective = crate::codex_thread::reign_objective(&scope);
    if live.objective != expected_objective {
        return GoalArbitration::Refusal(format!(
            "conflicting goal truth: expected objective {expected_objective:?}, got {:?}",
            live.objective
        ));
    }
    let status = match live.status {
        crate::codex_thread::GoalStatus::Active => "active",
        crate::codex_thread::GoalStatus::Paused => "paused",
        crate::codex_thread::GoalStatus::Blocked => "blocked",
        crate::codex_thread::GoalStatus::UsageLimited => "usageLimited",
        crate::codex_thread::GoalStatus::BudgetLimited => "budgetLimited",
        crate::codex_thread::GoalStatus::Completed => "completed",
    };
    arbitrate_goal_truth(
        driver,
        manifest,
        Some(GoalTruth {
            objective: live.objective,
            status: status.to_string(),
            continuation_owner: owner,
        }),
    )
}

fn arbitrate_goal_truth(driver: &str, manifest: &str, goal: Option<GoalTruth>) -> GoalArbitration {
    let Some(goal) = goal else {
        return GoalArbitration::None;
    };
    if goal.status != "active" {
        return GoalArbitration::None;
    }
    if goal.continuation_owner.is_empty() {
        return GoalArbitration::Refusal("active goal truth is missing continuation owner".into());
    }
    let expected_owner = expected_continuation_owner(driver, manifest);
    let Some(expected_owner) = expected_owner else {
        return GoalArbitration::Refusal(
            "active goal truth cannot be verified: manifest scope/node is missing".into(),
        );
    };
    if goal.continuation_owner != expected_owner {
        return GoalArbitration::Refusal(format!(
            "conflicting goal truth: expected continuation owner {expected_owner:?}, got {:?}",
            goal.continuation_owner
        ));
    }
    if driver == "king" {
        let Some(scope) = first_raw_field(manifest, &["scope", "crown_scope"]) else {
            return GoalArbitration::Refusal(
                "active goal truth cannot be verified: manifest scope is missing".into(),
            );
        };
        let expected = crate::codex_thread::reign_objective(&scope);
        if goal.objective != expected {
            return GoalArbitration::Refusal(format!(
                "conflicting goal truth: expected objective {expected:?}, got {:?}",
                goal.objective
            ));
        }
    }
    GoalArbitration::Delegated
}

fn expected_continuation_owner(driver: &str, manifest: &str) -> Option<String> {
    if let Some(owner) = first_raw_field(manifest, &["continuation_owner"]) {
        return Some(owner);
    }
    let scope = first_raw_field(manifest, &["scope", "crown_scope"]).unwrap_or_default();
    let node_id = first_raw_field(manifest, &["node_id", "fno_id"]).unwrap_or_default();
    match driver {
        "king" if !scope.is_empty() => Some(format!("king:{scope}")),
        "target" if !node_id.is_empty() => Some(format!("target:{node_id}")),
        _ => None,
    }
}

fn emit_stop_decision(
    cwd: &Path,
    fire: &Fire,
    state: Option<&Path>,
    driver: &str,
    continuation_owner: &str,
    decision: &str,
    class: &str,
    manifest: &str,
) {
    let session_id = if fire.session_id.is_empty() {
        fire.hook_harness_id.clone()
    } else {
        fire.session_id.clone()
    };
    let turn = if fire.turn_id.is_empty() {
        "stop".to_string()
    } else {
        fire.turn_id.clone()
    };
    let correlation_id = format!("stop:{session_id}:{turn}");
    let event = StopDecisionEvent {
        session_id,
        raw_identity_candidates: fire.resolve_ids.clone(),
        turn_id: fire.turn_id.clone(),
        manifest: state
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        scope: first_raw_field(manifest, &["scope", "crown_scope"]).unwrap_or_default(),
        node_id: first_raw_field(manifest, &["node_id", "fno_id"]).unwrap_or_default(),
        driver: driver.to_string(),
        continuation_owner: continuation_owner.into(),
        decision: decision.into(),
        class: class.into(),
        correlation_id,
        harness_output_contract: harness_output_contract(fire, decision).into(),
    };
    let project_events = events_path(cwd);
    let global_events = std::env::var_os("GLOBAL_EVENTS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fno/events.jsonl")))
        .unwrap_or_else(|| project_events.clone());
    crate::loopcheck::emit_to_both(
        &project_events,
        &global_events,
        "stop_decision",
        serde_json::to_value(event).expect("Stop decision event serializes"),
    );
}

fn harness_output_contract(fire: &Fire, decision: &str) -> &'static str {
    if decision == "allow" {
        return "empty";
    }
    let claude = fire.harness.as_deref() == Some("claude")
        || std::env::var("CLAUDECODE").as_deref() == Ok("1")
        || std::env::var_os("CLAUDE_PLUGIN_ROOT").is_some_and(|v| !v.is_empty());
    let foreign = [
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()));
    if claude && !foreign {
        "json_block"
    } else {
        "exit_2_stderr"
    }
}

fn transcript_basename(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let p = Path::new(path);
    let name = p.file_name()?.to_string_lossy().into_owned();
    if name == "chat_history.jsonl" {
        // grok's store file name is constant, so the session id lives in the
        // parent directory name; that is the id the manifest guard compares.
        return p
            .parent()?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
    }
    Some(name.strip_suffix(".jsonl").unwrap_or(&name).to_string())
}

/// The 8-4-4-4-12 hex uuid at the END of `s`, after a `-` separator: the
/// shim's `sed -E 's/^.*-([0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12})$/\1/'`.
fn uuid_suffix(s: &str) -> Option<String> {
    let (head, tail) = s.split_at(s.len().checked_sub(36)?);
    let dash_ok = [8, 13, 18, 23].iter().all(|&i| tail.as_bytes()[i] == b'-');
    if head.is_empty()
        || !head.ends_with('-')
        || !dash_ok
        || !tail.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
    {
        return None;
    }
    Some(tail.to_string())
}

/// The shim's harness markers: the transcript location is proof, env markers
/// are claims, and two disagreeing markers claim nothing.
fn detect_harness(transcript_path: &str, has_codex_turn: bool) -> Option<String> {
    if let Ok(h) = std::env::var("FNO_HARNESS") {
        if !h.is_empty() {
            return Some(h);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let prefix = PathBuf::from(home).join(".claude").join("projects");
        if !transcript_path.is_empty() && Path::new(transcript_path).starts_with(&prefix) {
            return Some("claude".to_string());
        }
    }
    let present: Vec<&str> = [
        ("CODEX_THREAD_ID", "codex"),
        ("CLAUDE_CODE_SESSION_ID", "claude"),
        ("GEMINI_SESSION_ID", "gemini"),
    ]
    .iter()
    .filter(|(e, _)| std::env::var_os(e).is_some_and(|v| !v.is_empty()))
    .map(|(_, n)| *n)
    .collect();
    match present[..] {
        [one] => Some(one.to_string()),
        [] if has_codex_turn => Some("codex".to_string()),
        _ => None,
    }
}

fn evaluate(cwd: &Path, fire: &Fire) -> Verdict {
    let wt_state = worktree_space_dir(cwd).join("target-state.md");
    let live_state = if wt_state.exists() {
        wt_state
    } else {
        worktree_repo_root(cwd).join(".fno").join("target-state.md")
    };

    let mut pre_manifest_no_file = false;
    let step = if live_state.exists() {
        // Presence is not ownership: a resident manifest naming a FOREIGN
        // session sends the resolver out (the shell's non-resident branch).
        if resident_matches(&live_state, &fire.hook_harness_id) {
            return Verdict::Owner {
                state: live_state,
                driver: "target",
                owner_cwd: cwd.to_path_buf(),
            };
        }
        // A resident manifest that fails the resolver is checker-broken,
        // whatever else is on disk.
        resolve_across_worktrees(cwd, &fire.resolve_ids).map_err(|_| true)
    } else {
        pre_manifest_no_file = true;
        // A stranger stop with no other worktree present cannot be a
        // worktree-ownership question, so an unreadable listing falls
        // through to the king and visitor arms instead of blocking.
        resolve_across_worktrees(cwd, &fire.resolve_ids)
            .map_err(|_| git_worktree_paths(cwd).map_or(false, |w| w.len() > 1))
    };

    let resolved = match step {
        Err(true) => return Verdict::Broken("target"),
        Err(false) => None,
        Ok(ref r) => r.clone(),
    };
    // A CLEAN miss (every worktree answered "no manifest names this stop")
    // is what gates the visitor diagnostic + distress scan; a broken
    // fall-through is not a miss and must scan nothing.
    let clean_miss = matches!(step, Ok(None));

    if let Some(identity) = resolved {
        return Verdict::Owner {
            state: identity.manifest_path,
            driver: "target",
            owner_cwd: PathBuf::from(identity.owner_cwd.clone()),
        };
    }

    // Pending delivery, then king: the shim's order. Either converts a
    // stranger into an owner before the visitor allow fires.
    if let Some(pending) = delivery_pending_for(cwd, fire, None) {
        return Verdict::Owner {
            state: pending,
            driver: "target",
            owner_cwd: cwd.to_path_buf(),
        };
    }
    match resolve_king(cwd, fire) {
        KingResolve::None => {}
        KingResolve::Found(state) => {
            return Verdict::Owner {
                state,
                driver: "king",
                owner_cwd: cwd.to_path_buf(),
            };
        }
    }

    // Visitor allow. The diagnostic names every id tried (a contract the
    // docs reference), and the distress scan reads the payload's own
    // message first so no reader subprocess runs for an ordinary stop.
    if clean_miss && !fire.resolve_harness_id.is_empty() {
        eprintln!(
            "loop-check: no manifest names session {}; visitor allowed (tried: {})",
            fire.resolve_harness_id,
            fire.resolve_ids.join(" ")
        );
    }
    if clean_miss && pre_manifest_no_file && !fire.transcript_path.as_os_str().is_empty() {
        let project_events = events_path(cwd);
        let global_events = crate::loopcheck::default_global_events_path();
        distress::scan_and_emit(
            &project_events,
            &global_events,
            cwd,
            &fire.resolve_harness_id,
            None,
            fire.harness.as_deref(),
            &fire.transcript_path,
            fire.last_assistant_message.as_deref(),
        );
    }
    Verdict::NoOwner
}

/// The shim's resident check: does the manifest at `state` name THIS stop's
/// harness id (exact or codex rollout suffix), or - when the manifest names
/// no harness at all - this stop's id as its bare session identity?
fn resident_matches(state: &Path, hook_harness_id: &str) -> bool {
    if hook_harness_id.is_empty() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(state) else {
        return false;
    };
    let resident_session = first_raw_field(&content, &["fno_id", "session_id"]).unwrap_or_default();
    let resident_harness = first_raw_field(
        &content,
        &[
            "harness_session_id",
            "claude_session_id",
            "claude_transcript_id",
        ],
    )
    .unwrap_or_default();
    if !resident_harness.is_empty()
        && (resident_harness == hook_harness_id
            || hook_harness_id.ends_with(&format!("-{resident_harness}")))
    {
        return true;
    }
    resident_harness.is_empty() && resident_session == hook_harness_id
}

/// One worktree listing, every id tried id-major (the shim calls the
/// per-id resolver repeatedly; the first id with any worktree hit wins).
fn resolve_across_worktrees(
    cwd: &Path,
    ids: &[String],
) -> Result<Option<crate::manifest_lookup::ManifestIdentity>, String> {
    // An unlistable root (a non-git cwd, an absent repo) is "no sibling
    // worktrees", not a broken checker: the search proceeds on the cwd alone
    // and a miss reads CLEAN (the visitor diagnostic fires).
    let mut candidates = git_worktree_paths(cwd).unwrap_or_default();
    if !candidates.iter().any(|path| paths_eq(path, cwd)) {
        candidates.push(cwd.to_path_buf());
    }
    for id in ids {
        if id.trim().is_empty() {
            continue;
        }
        for worktree in &candidates {
            let manifest_path = worktree.join(".fno/target-state.md");
            let Ok(content) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let mut identity = parse_manifest_identity(content.as_str());
            if identity.matches(id) {
                if identity.owner_cwd.is_empty() {
                    identity.owner_cwd = worktree.to_string_lossy().into_owned();
                }
                identity.manifest_path =
                    std::fs::canonicalize(&manifest_path).unwrap_or(manifest_path);
                return Ok(Some(identity));
            }
        }
    }
    Ok(None)
}

enum KingResolve {
    None,
    Found(PathBuf),
}

/// The king manifest resolution: the registry row - not file presence -
/// proves authority, the scope names the manifest under the space's kings
/// dir, and any unreadable step answers None (the same fail-open the Python
/// resolver's catch-all ships). The row lookup is the ONE matcher,
/// `loop_reign::find_by_session`.
fn resolve_king(cwd: &Path, fire: &Fire) -> KingResolve {
    if fire.hook_harness_id.is_empty() {
        return KingResolve::None;
    }
    let kings_dir = super::events_space(cwd).join("kings");
    let has_manifests = std::fs::read_dir(&kings_dir)
        .map(|mut it| {
            it.any(|e| {
                e.map(|e| e.path().extension().is_some_and(|x| x == "md"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if !has_manifests {
        return KingResolve::None;
    }
    let sid = if fire.resolve_harness_id.is_empty() {
        &fire.hook_harness_id
    } else {
        &fire.resolve_harness_id
    };
    let rows =
        match crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json()) {
            Ok(r) => r.entries,
            Err(_) => return KingResolve::None,
        };
    let Some(row) = crate::loop_reign::find_by_session(&rows, sid, fire.harness.as_deref()) else {
        return KingResolve::None;
    };
    use crate::AgentStatus;
    if matches!(
        row.status,
        AgentStatus::Exited
            | AgentStatus::Orphaned
            | AgentStatus::Failed
            | AgentStatus::PermanentDead
    ) {
        return KingResolve::None;
    }
    let Some(scope) = row
        .crown_scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return KingResolve::None;
    };
    if scope.contains("..") || scope.contains('/') || scope.contains('\\') {
        return KingResolve::None;
    }
    let path = kings_dir.join(format!("{scope}.md"));
    if path.is_file() {
        KingResolve::Found(path)
    } else {
        KingResolve::None
    }
}

/// The pending-delivery retry file this session would resume, if any. With a
/// resolved state file the name is derived from that manifest's ids; without
/// one, the newest harness-tagged pending file wins, then any pending file
/// whose stamped harness id names this stop.
fn delivery_pending_for(cwd: &Path, fire: &Fire, state: Option<&Path>) -> Option<PathBuf> {
    let prefix = git_delivery_prefix(&worktree_repo_root(cwd));
    match state {
        Some(state) => {
            let retry_id = retry_id_for(&std::fs::read_to_string(state).ok()?, fire);
            let pending = PathBuf::from(format!("{}.{}.md", prefix.display(), retry_id));
            pending.exists().then_some(pending)
        }
        None => {
            let entries = std::fs::read_dir(prefix.parent()?).ok()?;
            let mut named: Vec<PathBuf> = Vec::new();
            let mut all: Vec<PathBuf> = Vec::new();
            let file_name = |p: &Path| -> String {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            for entry in entries.flatten() {
                let p = entry.path();
                let name = file_name(&p);
                if !name.ends_with(".md") || !p.is_file() {
                    continue;
                }
                let Some(rest) = name.strip_prefix(&format!(
                    "{}",
                    prefix
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )) else {
                    continue;
                };
                all.push(p.clone());
                let suffix = rest.strip_prefix(&format!("{}.", fire.hook_harness_id));
                if let Some(suffix) = suffix {
                    // The glob's literal dot sat between the id and the `*`,
                    // so strip_prefix already consumed it: only the `.md`
                    // tail is left to check.
                    if suffix.ends_with(".md") {
                        named.push(p);
                    }
                }
            }
            if !fire.hook_harness_id.is_empty() {
                // Strictly-newer wins (the shim's `-nt` keeps the first of
                // two equal-mtime files in glob order).
                let mut newest: Option<(PathBuf, u64)> = None;
                for p in named {
                    let m = mtime_secs(&p).unwrap_or(0);
                    if newest.as_ref().is_none_or(|(_, best)| m > *best) {
                        newest = Some((p, m));
                    }
                }
                if let Some((p, _)) = newest {
                    return Some(p);
                }
            }
            // Alphabetical order, first harness-tagged match: the shim's glob
            // order for its second pass.
            all.sort();
            for p in all {
                let Ok(content) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let stamped =
                    first_raw_field(&content, &["harness_session_id", "claude_session_id"]);
                if !fire.hook_harness_id.is_empty()
                    && stamped.as_deref() == Some(fire.hook_harness_id.as_str())
                {
                    return Some(p);
                }
            }
            None
        }
    }
}

/// The derived pending path for a resolved state file, WITHOUT requiring the
/// file to exist - the DoneDelivery terminal creates it by copying the state.
fn pending_state_path(cwd: &Path, fire: &Fire, state: &Path) -> Option<PathBuf> {
    let prefix = git_delivery_prefix(&worktree_repo_root(cwd));
    let content = std::fs::read_to_string(state).ok()?;
    let retry_id = retry_id_for(&content, fire);
    Some(PathBuf::from(format!(
        "{}.{}.md",
        prefix.display(),
        retry_id
    )))
}

/// The `<owner>.<session>` retry identity both pending-path shapes derive
/// from the manifest: this stop's harness id, else the manifest's stamped one.
fn retry_id_for(content: &str, fire: &Fire) -> String {
    let live_session = first_raw_field(content, &["fno_id", "session_id"]);
    let live_harness = first_raw_field(content, &["harness_session_id", "claude_session_id"]);
    let owner = if !fire.hook_harness_id.is_empty() {
        fire.hook_harness_id.clone()
    } else {
        live_harness.unwrap_or_else(|| "harness".to_string())
    };
    format!(
        "{}.{}",
        owner,
        live_session.unwrap_or_else(|| "session".into())
    )
}

fn mtime_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// First non-empty, non-`null` value among `keys`, first matching line wins,
/// quotes and whitespace stripped. Mirrors the shim's `grep | sed | tr` chain.
fn first_raw_field(content: &str, keys: &[&str]) -> Option<String> {
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !keys.contains(&key.trim()) {
            continue;
        }
        let value = value.trim().trim_matches('"').trim();
        if !value.is_empty() && value != "null" {
            return Some(value.to_string());
        }
    }
    None
}

/// The one subprocess the ownership path may spend: a single `git rev-parse
/// --git-path` naming the delivery-pending prefix.
fn git_delivery_prefix(repo_root: &Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-path", "fno-delivery-finalize-pending-"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    let raw = out.unwrap_or_else(|| {
        repo_root
            .join(".fno")
            .join(".delivery-finalize-pending-")
            .to_string_lossy()
            .into_owned()
    });
    let path = PathBuf::from(&raw);
    if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_suffix_takes_a_rollout_tail() {
        let tid = "0198abcd-1234-5678-9abc-def012345678";
        let rollout = format!("rollout-2026-09-15T101530-{tid}");
        assert_eq!(uuid_suffix(&rollout).as_deref(), Some(tid));
        assert_eq!(uuid_suffix(tid), None, "no separator prefix, no strip");
        assert_eq!(uuid_suffix("session-xyz"), None);
        assert_eq!(uuid_suffix(""), None);
    }

    #[test]
    fn first_raw_field_reads_first_line_and_skips_null() {
        let doc = "fno_id: a1\nsession_id: null\nfno_id: b2\nharness_session_id: \"h1\"\n";
        assert_eq!(
            first_raw_field(doc, &["fno_id", "session_id"]).as_deref(),
            Some("a1")
        );
        assert_eq!(
            first_raw_field(doc, &["harness_session_id", "claude_session_id"]).as_deref(),
            Some("h1")
        );
        assert_eq!(first_raw_field(doc, &["claude_transcript_id"]), None);
    }

    #[test]
    fn resident_matches_accepts_exact_and_rollout_suffix() {
        let dir = std::env::temp_dir().join(format!("stop-gate-resident-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("target-state.md");
        std::fs::write(
            &state,
            "---\nfno_id: fno-a1\nharness_session_id: h-uuid-1234\ntarget_claim_key: \"node:x\"\n",
        )
        .unwrap();
        assert!(resident_matches(&state, "h-uuid-1234"));
        assert!(resident_matches(&state, "rollout-7-h-uuid-1234"));
        assert!(!resident_matches(&state, "other"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resident_match_falls_back_to_bare_session_id() {
        let dir = std::env::temp_dir().join(format!("stop-gate-resident2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("target-state.md");
        std::fs::write(&state, "---\nfno_id: h-uuid-9\n").unwrap();
        assert!(resident_matches(&state, "h-uuid-9"));
        assert!(!resident_matches(&state, "h-uuid-9-extra"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn releases_claim_covers_exactly_the_finished_terminals() {
        use crate::loopcheck::TerminationReason as T;
        assert!(T::DonePRGreen.releases_claim());
        assert!(T::DoneAdvisory.releases_claim());
        assert!(T::DoneDelivery.releases_claim());
        assert!(T::NoWork.releases_claim());
        assert!(!T::DoneBatched.releases_claim());
        assert!(!T::DoneUnreviewed.releases_claim());
        assert!(!T::DoneAwaitingMerge.releases_claim());
        assert!(!T::DonePlanned.releases_claim());
    }

    #[test]
    fn cargo_build_dir_value_ends_with_the_cargo_template() {
        let dir = std::env::temp_dir().join(format!("stop-gate-cargo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let v = cargo_build_dir_value(&dir);
        assert!(v.ends_with("/{workspace-path-hash}"), "{v}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The shell contract, native now: an unset env is resolved from
    /// config and set in place; a session preset survives untouched. The env
    /// pin holds the same lock the other env-pinning suites hold.
    #[test]
    fn export_session_build_dir_sets_unset_and_honors_preset() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("CARGO_BUILD_BUILD_DIR");
        let dir = std::env::temp_dir().join(format!("stop-gate-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        std::env::remove_var("CARGO_BUILD_BUILD_DIR");
        export_session_build_dir("target", &dir);
        let set = std::env::var_os("CARGO_BUILD_BUILD_DIR");
        assert!(
            set.as_deref()
                .map(|v| v.to_string_lossy().ends_with("/{workspace-path-hash}"))
                == Some(true),
            "unset + driver target must resolve and export: {set:?}"
        );

        std::env::set_var("CARGO_BUILD_BUILD_DIR", "/tmp/session-own-base/hash");
        export_session_build_dir("target", &dir);
        assert_eq!(
            std::env::var_os("CARGO_BUILD_BUILD_DIR").as_deref(),
            Some(std::ffi::OsStr::new("/tmp/session-own-base/hash")),
            "a session preset must survive untouched"
        );

        std::env::remove_var("CARGO_BUILD_BUILD_DIR");
        export_session_build_dir("other", &dir);
        assert_eq!(
            std::env::var_os("CARGO_BUILD_BUILD_DIR"),
            None,
            "non-target drivers get no build-dir export"
        );

        match saved {
            Some(v) => std::env::set_var("CARGO_BUILD_BUILD_DIR", v),
            None => std::env::remove_var("CARGO_BUILD_BUILD_DIR"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    fn fire_with_goal(goal: Value) -> Fire {
        collect_fire(
            "session-full",
            "/tmp/rollout-session-full.jsonl",
            None,
            "turn-42".to_string(),
            Some(goal),
        )
    }

    #[test]
    fn active_verified_goal_is_the_single_continuation_owner() {
        let fire = fire_with_goal(serde_json::json!({
            "objective": "finish the target",
            "status": "active",
            "continuation_owner": "target:node-42"
        }));
        let manifest = "fno_id: node-42\n";
        assert_eq!(
            arbitrate_continuation("target", &fire, manifest),
            GoalArbitration::Delegated
        );
    }

    #[test]
    fn codex_stop_uses_live_goal_and_ignores_stale_cached_goal_truth() {
        let fire = collect_fire(
            "session-full",
            "/tmp/rollout-session-full.jsonl",
            None,
            "turn-42".to_string(),
            None,
        );
        let manifest =
            "scope: scope-a\nfno_id: scope-a\nharness_session_id: session-full\nharness: codex\n";
        let active = crate::codex_thread::NativeGoal {
            thread_id: "session-full".to_string(),
            objective: "$fno:reign scope-a".to_string(),
            status: crate::codex_thread::GoalStatus::Active,
        };
        assert_eq!(
            arbitrate_codex_continuation_from_reading(
                "king",
                &fire,
                manifest,
                Ok(Some(active.clone()))
            ),
            GoalArbitration::Delegated
        );
        let conflicting_owner = format!("{manifest}continuation_owner: king:scope-b\n");
        assert!(matches!(
            arbitrate_codex_continuation_from_reading(
                "king",
                &fire,
                &conflicting_owner,
                Ok(Some(active.clone()))
            ),
            GoalArbitration::Refusal(reason) if reason.contains("derive from crown scope")
        ));
        assert_eq!(
            arbitrate_codex_continuation_from_reading("king", &fire, manifest, Ok(None)),
            GoalArbitration::None
        );
        let mut wrong_thread = active.clone();
        wrong_thread.thread_id = "other-thread".to_string();
        assert!(matches!(
            arbitrate_codex_continuation_from_reading(
                "king",
                &fire,
                manifest,
                Ok(Some(wrong_thread))
            ),
            GoalArbitration::Refusal(reason) if reason.contains("thread mismatch")
        ));
        assert_eq!(
            arbitrate_codex_continuation_from_reading(
                "target",
                &fire,
                manifest,
                Ok(Some(active.clone()))
            ),
            GoalArbitration::None
        );
        assert!(matches!(
            arbitrate_codex_continuation_from_reading(
                "king",
                &fire,
                manifest,
                Err("timeout".to_string())
            ),
            GoalArbitration::Refusal(reason) if reason.contains("provider goal unreadable")
        ));

        let stale_manifest = format!(
            "{manifest}goal_objective: {}\ngoal_status: active\ngoal_owner: king:scope-a\n",
            active.objective
        );
        assert!(matches!(
            arbitrate_codex_continuation_from_reading("king", &fire, &stale_manifest, Ok(None)),
            GoalArbitration::None
        ));
    }

    #[test]
    fn no_goal_preserves_the_loop_check_owner() {
        let fire = collect_fire(
            "session-full",
            "/tmp/rollout-session-full.jsonl",
            None,
            "turn-42".to_string(),
            None,
        );
        assert_eq!(
            arbitrate_continuation("target", &fire, "fno_id: node-42\n"),
            GoalArbitration::None
        );
    }

    #[test]
    fn missing_goal_owner_is_a_named_refusal() {
        let fire = fire_with_goal(serde_json::json!({
            "objective": "finish the target",
            "status": "active"
        }));
        let refusal = arbitrate_continuation("target", &fire, "fno_id: node-42\n");
        assert!(
            matches!(refusal, GoalArbitration::Refusal(reason) if reason.contains("missing continuation owner"))
        );
    }

    #[test]
    fn conflicting_goal_owner_is_a_named_refusal() {
        let fire = fire_with_goal(serde_json::json!({
            "objective": "finish the target",
            "status": "active",
            "continuation_owner": "target:other"
        }));
        let refusal = arbitrate_continuation("target", &fire, "fno_id: node-42\n");
        assert!(
            matches!(refusal, GoalArbitration::Refusal(reason) if reason.contains("conflicting goal truth"))
        );
    }

    #[test]
    fn stop_decision_event_serializes_the_full_correlation_contract() {
        let fire = fire_with_goal(serde_json::json!({
            "objective": "finish the target",
            "status": "active",
            "continuation_owner": "target:node-42"
        }));
        let event = StopDecisionEvent {
            session_id: fire.session_id.clone(),
            raw_identity_candidates: fire.resolve_ids.clone(),
            turn_id: fire.turn_id.clone(),
            manifest: "/work/.fno/target-state.md".to_string(),
            scope: "scope-42".to_string(),
            node_id: "node-42".to_string(),
            driver: "target".to_string(),
            continuation_owner: "goal".to_string(),
            decision: "allow".to_string(),
            class: "delegated-to-goal".to_string(),
            correlation_id: "stop:session-full:turn-42".to_string(),
            harness_output_contract: "empty".to_string(),
        };
        let value = serde_json::to_value(event).unwrap();
        for field in [
            "session_id",
            "raw_identity_candidates",
            "turn_id",
            "manifest",
            "scope",
            "node_id",
            "driver",
            "continuation_owner",
            "decision",
            "class",
            "correlation_id",
            "harness_output_contract",
        ] {
            assert!(value.get(field).is_some(), "missing event field {field}");
        }
        assert_eq!(value["turn_id"], "turn-42");
        assert_eq!(value["class"], "delegated-to-goal");
    }
}
