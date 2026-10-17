//! The `mail send --raw` leaf: prompt-line injection.
//!
//! Port of the Python `_raw_send` leg plus its codex review-target helpers.
//! The front classifies and execs this verb; the leaf resolves the recipient
//! in the shared registry, gates delivery policy in-process, and drives the
//! `mail-inject` / `review-start` child lanes.

use crate::paths::AgentsHome;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;

const REVIEW_VERBS: [&str; 4] = ["code-review", "review", "review-changes", "sigma-review"];
const REVIEW_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Python `repr(str)` for refusal text: single quotes, backslash and quote
/// escaped. The refusal strings interpolate `{name!r}`, and Rust's `{:?}`
/// renders double quotes, so every interpolation goes through this.
fn py_repr(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

fn refused(check: bool, usage: bool, reason: &str) -> i32 {
    if check && !usage {
        println!("not-injectable: {reason}");
        1
    } else {
        eprintln!("refused: {reason}");
        2
    }
}

fn unmeasurable(check: bool, reason: &str) -> i32 {
    if check {
        println!("unmeasurable: {reason}");
        3
    } else {
        eprintln!("refused: {reason}");
        2
    }
}

fn str_field<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or("")
}

fn resolve_self_session_id() -> Option<String> {
    let home = AgentsHome::from_env();
    let ident = crate::spawn_context::resolve_self_identity(
        &|name| std::env::var(name).ok(),
        None,
        None,
        &home,
    );
    ident.session_id
}

/// The raw-path self-send guard: own session non-empty and the resolved
/// recipient's session id matches it exactly (tier 0, the full id). The
/// Python leg's sentinel default collapses to this on every raw call.
fn self_recipient(resolved_session_id: &str, own: Option<&str>) -> bool {
    let Some(own) = own else { return false };
    if own.is_empty() {
        return false;
    }
    crate::identity::session_handle_tier(resolved_session_id, own) == Some(0)
}

fn events_path_for(cwd: Option<&str>) -> PathBuf {
    if let Ok(value) = std::env::var("FNO_EVENTS_PATH") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    let cwd = match cwd {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    crate::paths::events_path(&cwd)
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn run(args: &[String]) -> i32 {
    let mut check = false;
    let mut self_ok = false;
    let mut review_request = false;
    let mut style_exception: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut positionals: Vec<&String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--raw" => {
                // The leaf IS the raw lane; the front already routed on it.
            }
            "--check" => check = true,
            "--self-ok" | "--to-self" => self_ok = true,
            "--review-request" => review_request = true,
            "--style-exception" => {
                index += 1;
                match args.get(index) {
                    Some(value) => style_exception = Some(value.to_string()),
                    None => {
                        eprintln!("fno-agents mail send: --style-exception needs a value");
                        return 2;
                    }
                }
            }
            "--origin" => {
                index += 1;
                match args.get(index) {
                    Some(value) => origin = Some(value.to_string()),
                    None => {
                        eprintln!("fno-agents mail send: --origin needs a value");
                        return 2;
                    }
                }
            }
            other if other.starts_with('-') => {
                eprintln!("fno-agents mail send: unknown argument {other:?}");
                return 2;
            }
            _ => positionals.push(arg),
        }
        index += 1;
    }
    if positionals.len() != 2 {
        eprintln!("usage: fno-agents mail send [--check] [--self-ok] [--review-request] [--style-exception <reason>] [--origin <origin>] <name> <payload>");
        return 2;
    }
    let name = positionals[0].as_str();
    let payload = positionals[1].as_str();
    raw_send(
        name,
        payload,
        self_ok,
        check,
        style_exception.as_deref(),
        origin.as_deref(),
        review_request,
    )
}

fn raw_send(
    name: &str,
    payload: &str,
    self_ok: bool,
    check: bool,
    style_exception: Option<&str>,
    origin: Option<&str>,
    // Python threaded this into two str-receipt tail arms that are dead on
    // the bool-only Rust transports; parsed and carried, never read.
    _review_request: bool,
) -> i32 {
    let stripped = payload.trim();
    if stripped.is_empty() {
        return refused(check, true, "payload is empty");
    }
    if stripped == "/" || stripped == "$" {
        return refused(
            check,
            true,
            "payload is just a bare marker; nothing to invoke",
        );
    }
    if !stripped.starts_with('/') && !stripped.starts_with('$') {
        return refused(
            check,
            true,
            "raw is for running a command only: the payload must start with / or $. Drop --raw and send the message wrapped.",
        );
    }
    // The newline check reads the UNSTRIPPED payload: leading whitespace on a
    // second line survives trim on line one, and the refusal is about what
    // rides in as trailing content on the same submitted turn.
    if payload.contains('\n') || payload.contains('\r') {
        return refused(
            check,
            true,
            "payload must be a single line (a second line rides in as trailing content on the same submitted turn)",
        );
    }
    if let Some(code) = enforce_body_cap(stripped, check) {
        return code;
    }
    if let Some(code) = enforce_style(stripped, style_exception) {
        return code;
    }
    if crate::mail_header::text_holds_legacy_tag(stripped) {
        return refused(
            check,
            true,
            "payload contains an <fno_mail> tag. The envelope frames peer mail; a payload cannot contain one",
        );
    }

    let registry = AgentsHome::shared_registry_json();
    let lookup_name = if self_ok {
        // --to-self resolves the ambient identity to its registry handle;
        // with no ambient identity the name passes through and the registry
        // lookup produces the refusal.
        resolve_self_session_id().unwrap_or_else(|| name.to_string())
    } else {
        name.to_string()
    };
    let rows = match crate::client_verbs::load_registry_entries(&registry) {
        Ok(rows) => rows,
        Err(_) => {
            return unmeasurable(
                check,
                &format!(
                    "registry unreadable, so {} could not be resolved",
                    py_repr(name)
                ),
            )
        }
    };
    let found = match crate::client_verbs::find_agent_entry(&rows, &lookup_name) {
        Ok(found) => found,
        Err(_) => {
            return refused(
                check,
                false,
                &format!("could not resolve {} to a registered agent", py_repr(name)),
            )
        }
    };
    // Owned copy so the heal arm's re-resolve can replace it without fighting
    // the borrowed rows.
    let mut entry: Value = found.clone();
    let session_id = str_field(&entry, "harness_session_id").to_string();
    let review = parse_review_invocation(stripped);

    if session_id.is_empty() && !(self_ok || check) {
        return refused(
            check,
            false,
            &format!(
                "{} resolves to a registry row with no harness_session_id, so the self-send check cannot be decided; re-register the row (`fno agents register`), or if this IS your session address it with --to-self instead of a positional id",
                py_repr(name)
            ),
        );
    }
    let own = resolve_self_session_id();
    if !(self_ok || check) && self_recipient(&session_id, own.as_deref()) {
        return refused(
            check,
            false,
            "you addressed this session. For a plain self-note the envelope is fine:\n    fno agents mail send <own-id> \"text\"\nTo fire a verb at your own prompt line (no envelope, so the slash parses):\n    fno agents mail send '<payload>' --to-self --raw",
        );
    }

    if str_field(&entry, "delivery_policy") == "bus-only" {
        let token = if session_id.is_empty() {
            name.to_string()
        } else {
            session_id.clone()
        };
        let verdict = crate::mail_hold::gate(&token, Some(stripped), chrono::Utc::now());
        if !verdict.deliver {
            let detail = format!(
                "{} is DND (delivery-policy bus-only): prompt-line injection is forbidden for this recipient. Send wrapped mail instead - it queues durable and surfaces at their turn boundary",
                py_repr(name)
            );
            if check {
                return refused(check, false, &detail);
            }
            match crate::mail_hold::gate_parked_line(&verdict, &token, stripped) {
                Some(line) => {
                    println!("{line}");
                    return 0;
                }
                None => return refused(check, false, &detail),
            }
        }
    }

    let transport_sender = crate::identity::ambient_agent_handle();
    let sender = transport_sender
        .clone()
        .unwrap_or_else(|| "fno".to_string());
    let raw_recipient = crate::identity::canonical_handle(&session_id);
    let authored_words = crate::backlog::style_check::word_count(stripped);
    let mut failure_reasons: Vec<String> = Vec::new();
    let mut invocation_id: Option<String> = None;
    if review.is_some() && !check {
        // The one owner of the mint and the pending sidecar is the vendored
        // core behind review_invocation::owner; this lane answers from it
        // like the door and the codex leg do.
        let minted = crate::review_invocation::owner::mint_invocation_id();
        if !crate::review_invocation::owner::write_pending_invocation(&session_id, &minted, None) {
            // A stale sidecar owns the slot: consume it, then retry the write.
            let _ = crate::review_invocation::owner::adopt_pending_invocation(&session_id, None);
            let _ = crate::review_invocation::owner::write_pending_invocation(
                &session_id,
                &minted,
                None,
            );
        }
        invocation_id = Some(minted);
    }

    // MuxRef serializes as an object ({session, pane_id}); serde defaults
    // admit any object, and a bool never occurs on a real row.
    let entry_mux = entry.get("mux").is_some_and(Value::is_object);
    if entry_mux && !session_id.is_empty() && str_field(&entry, "harness") == "codex" {
        let (verdict, reason, pane) = lane_heal(&session_id);
        let (action, detail) =
            raw_send_heal_action(&verdict, reason.as_deref(), pane.as_ref(), name);
        match action.as_str() {
            "rebound" => {
                // Python re-resolves and lets a failed reload propagate; on a
                // failed reload this keeps the stale row instead - the heal
                // just said the pane is gone, and the send refuses downstream
                // either way.
                if let Ok(fresh_rows) = crate::client_verbs::load_registry_entries(&registry) {
                    if let Ok(fresh) =
                        crate::client_verbs::find_agent_entry(&fresh_rows, &lookup_name)
                    {
                        entry = fresh.clone();
                    }
                }
            }
            "refused" => return refused(check, false, &detail),
            "check" => {
                if check {
                    return unmeasurable(check, &detail);
                }
            }
            _ => {}
        }
    }

    let (lane, is_keystroke) = keystroke_lane(&entry);
    if !is_keystroke {
        if lane == "codex-daemon" {
            let review_verbs = packaged_review_verbs();
            let verb = stripped.split_whitespace().next().unwrap_or("");
            let is_review = review_verbs.iter().any(|candidate| candidate == verb);
            if check && !is_review {
                println!("injectable: codex-daemon turn/start");
                return 0;
            }
            if !is_review {
                // Python reserved body budget here; raw payloads never
                // reserve, so the reservation plumbing is skipped and
                // word_count is taken inline above.
                let (delivered, reason) = mail_inject_codex(&session_id, stripped, origin);
                if delivered {
                    record_raw(
                        &sender,
                        &raw_recipient,
                        &entry,
                        &session_id,
                        authored_words,
                        stripped,
                    );
                    println!("injected");
                    return 0;
                }
                let turn_failure = reason.as_deref().unwrap_or("no-daemon");
                return refused(
                    check,
                    false,
                    &format!(
                        "{} codex turn/start not delivered: {turn_failure}",
                        py_repr(name)
                    ),
                );
            }
            let default_base = if review_verbs.iter().any(|candidate| candidate == stripped) {
                codex_default_review_base(str_field(&entry, "cwd"))
            } else {
                None
            };
            let target = resolve_codex_review_target(stripped, default_base.as_deref());
            if let Some(subject_base) = target
                .as_deref()
                .and_then(|t| t.strip_prefix("baseBranch:"))
            {
                let (subject_ok, subject_detail) =
                    codex_review_subject_nonempty(str_field(&entry, "cwd"), subject_base);
                if !subject_ok {
                    let reason = format!(
                        "{} review/start would read an empty diff: {subject_detail}. Run the review inline in the PR worktree session: `fno do target request-self-review`, or `$fno:review <level>` there.",
                        py_repr(name)
                    );
                    if check {
                        println!("not-injectable: {reason}");
                        return 1;
                    }
                    return refused(check, false, &reason);
                }
            }
            if check {
                return match target {
                    Some(_) => {
                        println!("injectable: codex-daemon review/start RPC");
                        0
                    }
                    None => {
                        println!("not-injectable: no resolvable review target (bare verb with no origin default branch, or unparsable arguments); retry with '/review --base <branch>' or '/review --uncommitted'");
                        1
                    }
                };
            }
            let Some(target) = target else {
                return refused(
                    check,
                    false,
                    &format!(
                        "{} {verb} has no resolvable review target - a bare verb with no origin default branch, or arguments after the verb do not parse; retry with '/review --base <branch>' or explicitly request '/review --uncommitted'",
                        py_repr(name)
                    ),
                );
            };
            let audit_target_cwd: Option<String> =
                entry.get("cwd").and_then(Value::as_str).map(str::to_string);
            let receipt = review_start_codex(
                &session_id,
                &target,
                prefix_chars(stripped, 512),
                transport_sender.as_deref(),
                audit_target_cwd.as_deref(),
                self_ok,
                origin,
            );
            if receipt.delivered {
                record_raw(
                    &sender,
                    &raw_recipient,
                    &entry,
                    &session_id,
                    authored_words,
                    stripped,
                );
                println!(
                    "review/start target={target} delivery=inline turn={} review_thread={}",
                    receipt.turn_id.as_deref().unwrap_or(""),
                    receipt.review_thread_id.as_deref().unwrap_or("")
                );
                return 0;
            }
            let reason = receipt
                .reason
                .clone()
                .unwrap_or_else(|| "rpc-error".to_string());
            if reason == "not-confirmed" {
                // Exits 0 BEFORE any budget release: the request was written,
                // so a retry could double-fire the review.
                eprintln!(
                    "warning: {} codex review/start was not-confirmed after the request was sent; the review may already be running. Inspect the thread before deciding what happened; do not retry blindly",
                    py_repr(name)
                );
                return 0;
            }
            if reason == "no-daemon" {
                return refused(
                    check,
                    false,
                    &format!(
                        "{} codex review/start failed: no-daemon; run `codex app-server daemon start` and retry",
                        py_repr(name)
                    ),
                );
            }
            if reason == "stale-binary" || reason == "binary-not-found" {
                return refused(
                    check,
                    false,
                    &format!(
                        "{} codex review/start failed: {reason}; the deployed fno-agents binary is absent or rejected the invocation - run `fno doctor --fix` and retry",
                        py_repr(name)
                    ),
                );
            }
            return refused(
                check,
                false,
                &format!("{} codex review/start failed: {reason}", py_repr(name)),
            );
        }
        if check {
            println!("not-injectable: {lane} is not a prompt-line keystroke lane");
            return 1;
        }
        return refused(
            check,
            false,
            &format!(
                "{} resolves to the {lane} lane, which is not a prompt-line keystroke path; a raw slash payload would reach the model as text, not fire",
                py_repr(name)
            ),
        );
    }

    if check {
        if entry_mux {
            println!("injectable: mux-pane (a paste still needs the confirm to land)");
            return 0;
        }
        if session_id.is_empty() {
            println!(
                "not-injectable: the registry row for {} carries no harness_session_id, so the control.sock lane has no session to address; re-register it with `fno agents register`",
                py_repr(name)
            );
            return 1;
        }
        let (injectable, probe_reason) = mail_inject_probe(&session_id);
        if injectable {
            println!("injectable: control.sock (a paste can still refuse a busy prompt)");
            return 0;
        }
        if probe_reason == "probe-unavailable" {
            println!("unmeasurable: probe-unavailable (the fno-agents binary is absent, too old to carry --probe, or did not answer; run `fno doctor`)");
            return 3;
        }
        println!("not-injectable: {probe_reason}");
        return 1;
    }

    let delay_harness = if str_field(&entry, "harness").is_empty() {
        "claude"
    } else {
        str_field(&entry, "harness")
    };
    let delivered = if entry_mux {
        mail_inject_raw(
            &session_id,
            stripped,
            transport_sender.as_deref(),
            origin,
            self_ok,
            delay_harness,
            Some(&mut failure_reasons),
        )
    } else {
        mail_inject_raw(
            &session_id,
            stripped,
            transport_sender.as_deref(),
            origin,
            self_ok,
            delay_harness,
            None,
        )
    };

    let transport = if entry_mux {
        "mux_pane_send_raw"
    } else if self_ok {
        "mail_to_self_raw"
    } else {
        "mail_send_raw"
    };
    emit_review_sent(
        // str_field yields "" for a missing cwd, and events_path_for treats an
        // empty dir exactly like None (current-dir fallback), so Some is lossless.
        events_path_for(Some(str_field(&entry, "cwd"))),
        review.as_ref(),
        invocation_id.as_deref(),
        transport,
        delivered,
        failure_reasons.last().map(String::as_str),
        entry_mux,
        transport_sender.as_deref(),
        &session_id,
        self_ok,
        origin,
    );

    if delivered {
        record_raw(
            &sender,
            &raw_recipient,
            &entry,
            &session_id,
            authored_words,
            stripped,
        );
        // Python had two review_request str-receipt arms here; the Rust
        // transports are bool-only, so both are dead by construction.
        if stripped == "/compact" {
            println!("injected. ALREADY SENT - do not re-send. A handoff path (/compact <path>) would have produced a better summary: a bare /compact at ~100% context has no headroom left to summarize.");
        } else {
            println!("injected");
        }
        return 0;
    }
    println!("unconfirmed (not confirmed: either the confirm budget expired on a payload that landed, or the transport refused and nothing was sent - check the recipient before assuming either; never re-queue)");
    0
}

fn keystroke_lane(entry: &Value) -> (String, bool) {
    if entry.get("mux").is_some_and(Value::is_object) {
        return ("mux-pane".to_string(), true);
    }
    let harness = str_field(entry, "harness");
    if harness != "claude" {
        let shown = if harness.is_empty() {
            "unknown"
        } else {
            harness
        };
        return (format!("{shown}-daemon"), false);
    }
    ("control.sock".to_string(), true)
}

fn packaged_review_verbs() -> Vec<String> {
    // The Python leg checked for an absent sibling binary here; this leaf IS
    // the binary, so the absent arm is dead by construction.
    match crate::harness_capabilities::HarnessContract::packaged() {
        Ok(contract) => contract
            .capabilities("codex")
            .map(|caps| caps.review_verbs.clone())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The heal probe shells back out to the sibling binary's `--lane-heal` lane.
/// The binary-absent arm of the Python leg is dead here: this leaf IS that
/// binary, so spawn failure is a transient exec failure, not a missing tool.
fn lane_heal(session_id: &str) -> (String, Option<String>, Option<Value>) {
    let argv = vec![
        agents_binary(),
        "mail-inject".to_string(),
        "--harness".to_string(),
        "codex".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--lane-heal".to_string(),
    ];
    let captured = run_captured(&argv, "", false, 30.0);
    if captured.spawn_failed || captured.timed_out {
        return (
            "unmeasurable".to_string(),
            Some("spawn-failed".to_string()),
            None,
        );
    }
    let parsed = match serde_json::from_str::<Value>(captured.stdout.trim()) {
        Ok(parsed) => parsed,
        Err(_) => {
            return (
                "unmeasurable".to_string(),
                Some("unparseable-output".to_string()),
                None,
            )
        }
    };
    let verdict = parsed
        .get("verdict")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("unmeasurable")
        .to_string();
    let reason = parsed
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pane = parsed
        .get("pane")
        .filter(|value| value.is_object())
        .cloned();
    (verdict, reason, pane)
}

fn raw_send_heal_action(
    verdict: &str,
    reason: Option<&str>,
    pane: Option<&Value>,
    name: &str,
) -> (String, String) {
    // Python renders a missing reason as "None" inside the detail text.
    let shown_reason = reason.unwrap_or("None");
    match verdict {
        "rebound-thread" => ("rebound".to_string(), String::new()),
        "dead-pane" => {
            let label = pane
                .and_then(|pane| {
                    let session = pane.get("session").and_then(Value::as_str)?;
                    let pane_id = pane.get("pane_id").and_then(Value::as_str)?;
                    Some(format!("{session}:{pane_id}"))
                })
                .unwrap_or_else(|| "unknown".to_string());
            (
                "refused".to_string(),
                format!(
                    "{} mux pane {label} is gone and the thread is not loaded anywhere fno can reach ({shown_reason}); run fno agents resume {name}",
                    py_repr(name)
                ),
            )
        }
        "unmeasurable" => (
            "check".to_string(),
            format!("lane-heal could not read the pane binding ({shown_reason})"),
        ),
        _ => ("continue".to_string(), String::new()),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_review_sent(
    events_path: PathBuf,
    review: Option<&ReviewInvocation>,
    invocation_id: Option<&str>,
    transport: &str,
    delivered: bool,
    receipt: Option<&str>,
    submit_required: bool,
    initiator_session_id: Option<&str>,
    target_session_id: &str,
    self_ok: bool,
    origin: Option<&str>,
) {
    let Some(review) = review else { return };
    let Some(invocation_id) = invocation_id else {
        return;
    };
    if transport == "codex_rpc" {
        // The codex arms print their own receipts and emit nothing here.
        return;
    }
    let confirmed = delivered;
    let receipt = receipt.map(str::to_string).unwrap_or_else(|| {
        if confirmed {
            "delivered (hosted)".to_string()
        } else {
            "live-miss".to_string()
        }
    });
    let initiator = match origin {
        Some("operator") => "operator".to_string(),
        Some("scheduler") | Some("recovery") => "daemon".to_string(),
        _ => {
            if self_ok {
                "self".to_string()
            } else {
                "lead".to_string()
            }
        }
    };
    // Two-character literal "\r" (a keystroke name), not a carriage return.
    // Every Rust submit path presses enter (submit_pane sends b"\r"); the
    // transport carries no Tab press to name.
    let submit_key = if submit_required { Some("\\r") } else { None };
    let pr = explicit_review_pr_number(&review.args_raw);
    let mut fields = serde_json::Map::new();
    fields.insert("invocation_id".to_string(), json!(invocation_id));
    fields.insert("stage".to_string(), json!("sent"));
    fields.insert("verb".to_string(), json!(review.verb));
    fields.insert("args_raw".to_string(), json!(review.args_raw));
    fields.insert("level".to_string(), json!(review.level));
    fields.insert("level_source".to_string(), json!(review.level_source));
    fields.insert("flags".to_string(), json!(review.flags));
    fields.insert("transport".to_string(), json!(transport));
    fields.insert("initiator".to_string(), json!(initiator));
    if let Some(initiator_session_id) = initiator_session_id.filter(|s| !s.is_empty()) {
        fields.insert(
            "initiator_session_id".to_string(),
            json!(initiator_session_id),
        );
    }
    if !target_session_id.is_empty() {
        fields.insert("target_session_id".to_string(), json!(target_session_id));
    }
    fields.insert("submit_required".to_string(), json!(submit_required));
    if let Some(submit_key) = submit_key {
        fields.insert("submit_key".to_string(), json!(submit_key));
    }
    if submit_required {
        fields.insert("submit_confirmed".to_string(), json!(confirmed));
    }
    fields.insert("receipt".to_string(), json!(receipt));
    if let Some(pr) = pr {
        fields.insert("pr".to_string(), json!(pr));
    }
    let _ = crate::events::EventEmitter::new(events_path, "daemon")
        .emit_fields("review_invocation", fields);
}

/// The outbox record. Key order mirrors the Python envelope writer so a
/// `jq -S`-free reader sees the same field order on the wire.
fn record_raw(
    sender: &str,
    raw_recipient: &str,
    entry: &Value,
    session_id: &str,
    authored_words: usize,
    stripped: &str,
) {
    let msg_id = crate::announce::new_msg_id();
    let to_harness = str_field(entry, "harness");
    let mut obj = serde_json::Map::new();
    obj.insert("v".to_string(), json!(1));
    obj.insert("id".to_string(), json!(msg_id));
    obj.insert("ts".to_string(), json!(now_iso()));
    obj.insert("thread".to_string(), json!(msg_id));
    obj.insert("from".to_string(), json!(sender));
    obj.insert("to".to_string(), json!(raw_recipient));
    obj.insert("kind".to_string(), json!("send"));
    if !to_harness.is_empty() {
        obj.insert("to_harness".to_string(), json!(to_harness));
    }
    obj.insert("delivery".to_string(), json!("hosted"));
    obj.insert("to_kind".to_string(), json!("session"));
    obj.insert("word_count".to_string(), json!(authored_words));
    let mut meta = serde_json::Map::new();
    if !session_id.is_empty() {
        meta.insert("to_session".to_string(), json!(session_id));
    }
    if !to_harness.is_empty() {
        meta.insert("to_harness".to_string(), json!(to_harness));
    }
    if !meta.is_empty() {
        obj.insert("meta".to_string(), Value::Object(meta));
    }
    obj.insert("body".to_string(), json!(stripped));
    let live = crate::chats::bus_append_live_path();
    let chats_dir = crate::chats::chats_dir();
    if let Err(exc) = crate::announce::append_line_open(&live, &Value::Object(obj), &chats_dir) {
        eprintln!("delivery succeeded; outbox record failed; do not retry: {exc}");
    }
}

fn enforce_body_cap(stripped: &str, usage: bool) -> Option<i32> {
    let warn = cap_env_int("FNO_MAIL_BODY_WARN", 3000);
    let refuse = cap_env_int("FNO_MAIL_BODY_REFUSE", 5000);
    if warn <= 0 && refuse <= 0 {
        return None;
    }
    let size = stripped.len();
    if refuse > 0 && size > refuse as usize {
        eprintln!("error: mail body is {size} bytes (cap {refuse}). Relay mail is re-read every turn; put the detail in a node or doc and send a short pointer. Disable with FNO_MAIL_BODY_REFUSE=0 (warn-only) or both knobs 0.");
        return Some(if usage { 2 } else { 1 });
    }
    if warn > 0 && size > warn as usize {
        eprintln!("note: mail body is {size} bytes (over the {warn}-byte brevity guide); prefer a short pointer with the detail in a node/doc.");
    }
    None
}

fn cap_env_int(name: &str, default: i64) -> i64 {
    match std::env::var(name) {
        Ok(raw) => raw.trim().parse::<i64>().unwrap_or(default),
        Err(_) => default,
    }
}

fn enforce_style(stripped: &str, allow_reason: Option<&str>) -> Option<i32> {
    if std::env::var("FNO_STYLE_ENFORCE").as_deref() == Ok("0") || stripped.is_empty() {
        return None;
    }
    if allow_reason
        .map(str::trim)
        .is_some_and(|reason| !reason.is_empty())
    {
        return None;
    }
    if crate::mail_hold::body_is_control(stripped) {
        return None;
    }
    let word_cap = crate::agents_config::config_lookup(
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        &["style", "word_cap", "mail"],
    )
    .and_then(|value| value.as_integer())
    .map(|cap| cap as usize);
    let violations = crate::backlog::style_check::check(stripped, "mail", word_cap);
    if !violations.is_empty() && crate::backlog::style_check::has_exception(stripped).is_none() {
        emit_style_refusal(&violations);
        eprintln!(
            "{}",
            crate::backlog::style_check::format_violations(&violations, "mail")
        );
        return Some(1);
    }
    None
}

fn emit_style_refusal(violations: &[crate::backlog::style_check::Violation]) {
    let mut rule_ids: Vec<u32> = violations.iter().map(|v| v.rule).collect();
    rule_ids.sort_unstable();
    rule_ids.dedup();
    let mut data = serde_json::Map::new();
    data.insert("surface".to_string(), json!("mail"));
    data.insert("rule_ids".to_string(), json!(rule_ids));
    data.insert("violation_count".to_string(), json!(violations.len()));
    let home = AgentsHome::from_env();
    let ident = crate::spawn_context::resolve_self_identity(
        &|name| std::env::var(name).ok(),
        None,
        None,
        &home,
    );
    let source = if let Some(session_id) = ident.session_id {
        data.insert("session_id".to_string(), json!(session_id));
        "target"
    } else {
        "test"
    };
    let _ = crate::events::EventEmitter::new(events_path_for(None), source)
        .emit_fields("style_refusal", data);
}

struct ReviewInvocation {
    verb: String,
    args_raw: String,
    level: String,
    level_source: String,
    flags: Vec<String>,
}

fn parse_review_invocation(raw: &str) -> Option<ReviewInvocation> {
    let candidate = raw.trim();
    if candidate.is_empty() {
        return None;
    }
    let mut parts = candidate.splitn(2, char::is_whitespace);
    let token = parts.next().unwrap_or("");
    let args_raw = parts.next().map(str::trim_start).unwrap_or("").to_string();
    let name = token
        .trim_start_matches('/')
        .rsplit(':')
        .next()
        .unwrap_or("");
    if !REVIEW_VERBS.contains(&name) {
        return None;
    }
    // Python shlex with a whitespace fallback on ValueError.
    let args = match shlex_split(&args_raw) {
        Some(args) => args,
        None => args_raw.split_whitespace().map(str::to_string).collect(),
    };
    let (level, level_source) = match args.first().map(String::as_str) {
        Some("ultra") => ("ultra".to_string(), "ultra_forced".to_string()),
        Some(first) if REVIEW_LEVELS.contains(&first) => {
            (first.to_string(), "explicit".to_string())
        }
        _ => ("unset".to_string(), "fallback".to_string()),
    };
    let mut flags = Vec::new();
    for arg in &args {
        if let Some(canonical) = canonical_flag(arg) {
            flags.push(canonical);
        } else if arg.starts_with("--") {
            flags.push(arg.clone());
        }
    }
    Some(ReviewInvocation {
        verb: format!("/{name}"),
        args_raw,
        level,
        level_source,
        flags,
    })
}

fn canonical_flag(token: &str) -> Option<String> {
    const KNOWN_REVIEW_FLAGS: [&str; 3] = ["comment", "fix", "verify-fixes"];
    let mut name = token;
    if let Some(stripped) = name.strip_prefix("--") {
        name = stripped;
    } else if let Some(stripped) = name.strip_prefix('\u{2014}') {
        name = stripped;
    }
    KNOWN_REVIEW_FLAGS
        .contains(&name)
        .then(|| format!("--{name}"))
}

/// A minimal Python-shlex-compatible splitter. Outside quotes a backslash
/// escapes the next character; inside double quotes it escapes only `"` and
/// `\`, staying literal before anything else; single quotes are plain
/// characters (review args never quote with them). Returns None on an
/// unterminated quote, which the caller maps to the whitespace fallback,
/// mirroring Python's ValueError path.
fn shlex_split(input: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                started = true;
                in_quotes = !in_quotes;
            }
            '\\' => {
                started = true;
                match chars.next() {
                    None => return None,
                    Some(next) => {
                        if in_quotes && next != '"' && next != '\\' {
                            current.push('\\');
                        }
                        current.push(next);
                    }
                }
            }
            ch if ch.is_whitespace() && !in_quotes => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            ch => {
                started = true;
                current.push(ch);
            }
        }
    }
    if in_quotes {
        return None;
    }
    if started {
        tokens.push(current);
    }
    Some(tokens)
}

/// Python's `text[:cap]` is a character slice, not a byte slice.
fn prefix_chars(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

fn commit_sha_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| regex::Regex::new(r"^[0-9a-fA-F]{7,64}$").unwrap())
}

fn explicit_pr_review_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        regex::Regex::new(
            r"^(?:(?:low|medium|high|xhigh|max)(?: --comment (?P<comment_pr>[1-9][0-9]*))? )?(?:(?P<branch>[A-Za-z0-9][A-Za-z0-9._/-]*) )?HEAD (?P<head>[0-9a-fA-F]{7,64})(?: of PR (?P<pr>[1-9][0-9]*))? against origin/(?P<base>[A-Za-z0-9][A-Za-z0-9._/-]*)$",
        )
        .unwrap()
    })
}

fn explicit_review_pr_number(payload: &str) -> Option<i64> {
    let captures = explicit_pr_review_regex().captures(payload)?;
    let value = captures
        .name("pr")
        .or_else(|| captures.name("comment_pr"))?
        .as_str();
    value.parse::<i64>().ok()
}

fn resolve_codex_review_target(payload: &str, default_base: Option<&str>) -> Option<String> {
    let mut parts = payload.splitn(2, char::is_whitespace);
    let _verb = parts.next().unwrap_or("");
    let Some(remainder_raw) = parts.next() else {
        return default_base.map(|base| format!("baseBranch:{base}"));
    };
    let remainder = remainder_raw.trim();
    if let Some(captures) = explicit_pr_review_regex().captures(remainder) {
        return Some(format!("baseBranch:origin/{}", &captures["base"]));
    }
    if remainder.starts_with("HEAD ") {
        // A malformed explicit target must not fall through to the loose arms.
        return None;
    }
    let words: Vec<&str> = remainder.split_whitespace().collect();
    if words.first() == Some(&"--base") {
        return if words.len() == 2 && !words[1].starts_with("--") {
            Some(format!("baseBranch:{}", words[1]))
        } else {
            None
        };
    }
    if remainder == "--uncommitted" {
        return Some("uncommittedChanges".to_string());
    }
    if commit_sha_regex().is_match(remainder) {
        return Some(format!("commit:{remainder}"));
    }
    if remainder.starts_with("custom:") && remainder != "custom:" {
        return Some(remainder.to_string());
    }
    None
}

fn agents_binary() -> String {
    if let Ok(value) = std::env::var("FNO_AGENTS_BIN") {
        if !value.is_empty() {
            return value;
        }
    }
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "fno-agents".to_string())
}

struct Captured {
    status: Option<i32>,
    stdout: String,
    timed_out: bool,
    spawn_failed: bool,
}

/// Bounded spawn: stdin always piped (fed and closed by a writer thread),
/// stdout captured by a reader thread, stderr either inherited (the real-send
/// lanes freeze the child's teaching text) or drained by a thread so a chatty
/// child cannot deadlock on a full pipe. Kills at the deadline.
fn run_captured(
    argv: &[String],
    stdin_text: &str,
    inherit_stderr: bool,
    timeout_secs: f64,
) -> Captured {
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(if inherit_stderr {
        Stdio::inherit()
    } else {
        Stdio::piped()
    });
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return Captured {
                status: None,
                stdout: String::new(),
                timed_out: false,
                spawn_failed: true,
            }
        }
    };
    let mut stdin = child.stdin.take();
    let stdin_text = stdin_text.to_string();
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(stdin_text.as_bytes());
            let _ = stdin.flush();
        }
    });
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = String::new();
            let _ = out.read_to_string(&mut buf);
            buf
        })
    });
    let err_reader = (!inherit_stderr).then(|| {
        child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut buf = String::new();
                let _ = err.read_to_string(&mut buf);
                buf
            })
        })
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout_secs);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };
    let _ = writer.join();
    let stdout = reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if let Some(handle) = err_reader.flatten() {
        let _ = handle.join();
    }
    Captured {
        status,
        stdout,
        timed_out,
        spawn_failed: false,
    }
}

fn parse_receipt(stdout: &str) -> Option<Value> {
    serde_json::from_str(stdout.trim()).ok()
}

/// Both keystroke lanes (mux-pane and control.sock) share one spawn shape;
/// they differ only in whether failures are collected for the receipt.
fn mail_inject_raw(
    session_id: &str,
    text: &str,
    sender: Option<&str>,
    origin: Option<&str>,
    self_send: bool,
    delay_harness: &str,
    reasons: Option<&mut Vec<String>>,
) -> bool {
    let enter_delay_ms = crate::mail_inject::enter_delay_for_harness(delay_harness);
    let mut argv = vec![
        agents_binary(),
        "mail-inject".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--enter-delay-ms".to_string(),
        enter_delay_ms.to_string(),
    ];
    if let Some(sender) = sender.filter(|s| !s.is_empty()) {
        argv.push("--sender".to_string());
        argv.push(sender.to_string());
    }
    if let Some(origin) = origin.filter(|o| !o.is_empty()) {
        argv.push("--origin".to_string());
        argv.push(origin.to_string());
    }
    if self_send {
        argv.push("--self-send".to_string());
    }
    let captured = run_captured(&argv, text, true, 60.0);
    if captured.spawn_failed {
        if let Some(reasons) = reasons {
            reasons.push("no-binary".to_string());
        }
        return false;
    }
    if captured.timed_out {
        return false;
    }
    let Some(receipt) = parse_receipt(&captured.stdout) else {
        return false;
    };
    let delivered = receipt
        .get("delivered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !delivered {
        if let Some(reasons) = reasons {
            let reason = receipt
                .get("reason")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .unwrap_or("unknown");
            reasons.push(reason.to_string());
        }
    }
    delivered
}

fn mail_inject_codex(session_id: &str, text: &str, origin: Option<&str>) -> (bool, Option<String>) {
    let mut argv = vec![
        agents_binary(),
        "mail-inject".to_string(),
        "--harness".to_string(),
        "codex".to_string(),
        "--session".to_string(),
        session_id.to_string(),
    ];
    if let Some(origin) = origin.filter(|o| !o.is_empty()) {
        argv.push("--origin".to_string());
        argv.push(origin.to_string());
    }
    let captured = run_captured(&argv, text, false, 60.0);
    if captured.spawn_failed || captured.timed_out {
        return (false, None);
    }
    let Some(receipt) = parse_receipt(&captured.stdout) else {
        return (false, None);
    };
    let delivered = receipt
        .get("delivered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = receipt
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    (delivered, reason)
}

struct ReviewReceipt {
    delivered: bool,
    reason: Option<String>,
    turn_id: Option<String>,
    review_thread_id: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn review_start_codex(
    session_id: &str,
    target: &str,
    audit_payload: String,
    audit_sender: Option<&str>,
    audit_target_cwd: Option<&str>,
    self_send: bool,
    origin: Option<&str>,
) -> ReviewReceipt {
    let mut argv = vec![
        agents_binary(),
        "review-start".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--target".to_string(),
        target.to_string(),
        "--delivery".to_string(),
        "inline".to_string(),
        "--audit-payload".to_string(),
        audit_payload,
    ];
    if let Some(sender) = audit_sender {
        argv.push("--audit-sender".to_string());
        argv.push(sender.to_string());
    }
    if let Some(cwd) = audit_target_cwd {
        argv.push("--audit-target-cwd".to_string());
        argv.push(cwd.to_string());
    }
    if let Some(origin) = origin {
        argv.push("--audit-origin".to_string());
        argv.push(origin.to_string());
    }
    if self_send {
        argv.push("--audit-self-send".to_string());
    }
    let captured = run_captured(&argv, "", false, 60.0);
    if captured.timed_out {
        return ReviewReceipt {
            delivered: false,
            reason: Some("not-confirmed".to_string()),
            turn_id: None,
            review_thread_id: None,
        };
    }
    if captured.spawn_failed {
        // Python's binary-absent arm maps here as spawn-failed: this leaf IS
        // the binary, so a failed exec of our own exe is transient.
        return ReviewReceipt {
            delivered: false,
            reason: Some("spawn-failed".to_string()),
            turn_id: None,
            review_thread_id: None,
        };
    }
    let Some(receipt) = parse_receipt(&captured.stdout) else {
        // rc 2 with empty stdout is the usage arm of a deployed binary that
        // predates the audit flags: stale binary, not a daemon problem.
        let reason = if captured.status == Some(2) && captured.stdout.is_empty() {
            "stale-binary"
        } else {
            "rpc-error"
        };
        return ReviewReceipt {
            delivered: false,
            reason: Some(reason.to_string()),
            turn_id: None,
            review_thread_id: None,
        };
    };
    let delivered = receipt
        .get("delivered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if delivered {
        let turn_id = receipt
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let review_thread_id = receipt
            .get("review_thread_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let complete = captured.status == Some(0)
            && turn_id.as_deref().is_some_and(|id| !id.is_empty())
            && review_thread_id.as_deref().is_some_and(|id| !id.is_empty());
        if complete {
            return ReviewReceipt {
                delivered: true,
                reason: None,
                turn_id,
                review_thread_id,
            };
        }
        return ReviewReceipt {
            delivered: false,
            reason: Some("not-confirmed".to_string()),
            turn_id: None,
            review_thread_id: None,
        };
    }
    let reason = receipt
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    if let Some(reason) = reason {
        return ReviewReceipt {
            delivered: false,
            reason: Some(reason),
            turn_id: None,
            review_thread_id: None,
        };
    }
    ReviewReceipt {
        delivered: false,
        reason: Some("rpc-error".to_string()),
        turn_id: None,
        review_thread_id: None,
    }
}

fn mail_inject_probe(recipient: &str) -> (bool, String) {
    let argv = vec![
        agents_binary(),
        "mail-inject".to_string(),
        "--probe".to_string(),
        "--session".to_string(),
        recipient.to_string(),
    ];
    let captured = run_captured(&argv, "", false, 60.0);
    if captured.spawn_failed || captured.timed_out {
        return (false, "probe-unavailable".to_string());
    }
    let Some(receipt) = parse_receipt(&captured.stdout) else {
        return (false, "probe-unavailable".to_string());
    };
    (
        receipt
            .get("injectable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        receipt
            .get("reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .unwrap_or("unknown")
            .to_string(),
    )
}

/// rc 0 with empty stdout returns Some("") - the callers that Python gated on
/// truthiness apply the emptiness filter themselves.
fn git_out(cwd: &str, args: &[&str]) -> Option<String> {
    let dir = if cwd.is_empty() { "." } else { cwd };
    let mut argv: Vec<String> = vec!["git".to_string(), "-C".to_string(), dir.to_string()];
    argv.extend(args.iter().map(|arg| arg.to_string()));
    let captured = run_captured(&argv, "", false, 2.0);
    if captured.spawn_failed || captured.timed_out {
        return None;
    }
    if captured.status == Some(0) {
        Some(captured.stdout.trim().to_string())
    } else {
        None
    }
}

fn codex_default_review_base(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    git_out(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .filter(|base| !base.is_empty())
}

fn codex_review_subject_nonempty(cwd: &str, base_ref: &str) -> (bool, String) {
    if cwd.is_empty() {
        return (
            false,
            "the recipient session's cwd is unknown, so the review subject cannot be measured; re-register the row (`fno agents register`) or fire from the PR worktree session".to_string(),
        );
    }
    let Some(head) = git_out(cwd, &["rev-parse", "HEAD"]).filter(|head| !head.is_empty()) else {
        return (
            false,
            format!("the recipient session's checkout at {cwd} has no resolvable HEAD"),
        );
    };
    let head8 = head.get(..8).unwrap_or(&head).to_string();
    let merge_base = git_out(cwd, &["merge-base", &head, base_ref])
        .filter(|base| !base.is_empty())
        .or_else(|| {
            let stripped_base = base_ref.strip_prefix("origin/").unwrap_or(base_ref);
            git_out(cwd, &["merge-base", &head, stripped_base]).filter(|base| !base.is_empty())
        });
    let Some(merge_base) = merge_base else {
        return (
            false,
            format!(
                "the recipient session's checkout at {cwd} (HEAD {head8}) cannot resolve {base_ref} to a merge-base; fetch the base or fire from a checkout that tracks it"
            ),
        );
    };
    // Git failure (None) refuses; an empty diff (Some("")) falls through to
    // the changed-file count and refuses there with its own text.
    let Some(names) = git_out(
        cwd,
        &["diff", "--name-only", &format!("{merge_base}..{head}")],
    ) else {
        return (
            false,
            format!(
                "the recipient session's checkout at {cwd} (HEAD {head8}) could not be measured against {base_ref} (git timed out or failed); measure it by hand before firing the review"
            ),
        );
    };
    let count = names.lines().filter(|line| !line.is_empty()).count();
    if count == 0 {
        return (
            false,
            format!(
                "the recipient session's checkout at {cwd} (HEAD {head8}) has 0 changed files against {base_ref}; the review would read an empty diff, complete cleanly, and attest nothing"
            ),
        );
    }
    (
        true,
        format!("{count} changed files against {base_ref} at HEAD {head8}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Payloads go through the full verb+args shape the leaf receives: verb,
    /// one whitespace, then args. `main` as the default base keeps the
    /// explicit-target arm distinguishable from the fallback.
    fn target(payload: &str) -> Option<String> {
        resolve_codex_review_target(payload, Some("main"))
    }

    #[test]
    fn explicit_pr_receipt_resolves_to_its_base_branch() {
        let payload = "/fno:review high HEAD 1ed3352 of PR 42 against origin/next";
        assert_eq!(target(payload).as_deref(), Some("baseBranch:origin/next"));
        // explicit_review_pr_number receives args_raw, the verb-stripped
        // remainder parse_review_invocation stores, never the full payload.
        let args_raw = "high HEAD 1ed3352 of PR 42 against origin/next";
        assert_eq!(explicit_review_pr_number(args_raw), Some(42));
    }

    #[test]
    fn malformed_explicit_target_refuses_rather_than_falling_through() {
        let payload = "/fno:review HEAD 0xzz of PR 42 against origin/main";
        assert_eq!(target(payload), None);
        // No `pr` capture survives a malformed receipt. Feeding args_raw
        // makes the refusal come from the malformed head hash itself, not
        // from the verb breaking the anchored match.
        let args_raw = "HEAD 0xzz of PR 42 against origin/main";
        assert_eq!(explicit_review_pr_number(args_raw), None);
    }

    #[test]
    fn wrong_arity_base_flag_refuses() {
        assert_eq!(target("/fno:review --base"), None);
        // A `--base` value that is itself a flag must refuse, not resolve.
        assert_eq!(target("/fno:review --base --uncommitted"), None);
    }

    #[test]
    fn loose_arms_resolve_uncommitted_sha_and_custom() {
        assert_eq!(
            target("/fno:review --uncommitted").as_deref(),
            Some("uncommittedChanges")
        );
        assert_eq!(
            target("/fno:review abc1234").as_deref(),
            Some("commit:abc1234")
        );
        assert_eq!(
            target("/fno:review custom:diff://HEAD~1").as_deref(),
            Some("custom:diff://HEAD~1")
        );
        // Bare `custom:` carries no target.
        assert_eq!(target("/fno:review custom:"), None);
    }

    #[test]
    fn comment_pr_flag_yields_the_review_number() {
        assert_eq!(
            explicit_review_pr_number(
                "high --comment 3419 HEAD 1ed335280631e8e57dc78d8e576cfff6c8869d5e against origin/main"
            ),
            Some(3419)
        );
    }
}
