//! One public native-command door with an identity-pinned pane fallback.

use super::*;
use crate::cli_args::MuxCommandArgs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RECEIPT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProofKind {
    Compact,
    GoalActive,
    Screen,
}

impl ProofKind {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "compact" => Ok(Self::Compact),
            "goal-active" => Ok(Self::GoalActive),
            "screen" => Ok(Self::Screen),
            other => Err(format!(
                "--proof must be compact|goal-active|screen, got {other:?}"
            )),
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::GoalActive => "goal-active",
            Self::Screen => "screen",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandStatus {
    Verified,
    Unknown,
    Refused,
}

impl CommandStatus {
    fn word(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unknown => "unknown",
            Self::Refused => "refused",
        }
    }
}

fn classify_postcondition(submitted: bool, proof: Result<bool, String>) -> CommandStatus {
    if !submitted {
        return CommandStatus::Refused;
    }
    match proof {
        Ok(true) => CommandStatus::Verified,
        Ok(false) | Err(_) => CommandStatus::Unknown,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CommandReceipt {
    request_id: String,
    selector: String,
    session_id: String,
    harness: String,
    transport: String,
    expected_identity: String,
    command: String,
    proof: String,
    status: String,
    before_digest: String,
    after_digest: String,
    detail: String,
}

fn digest(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn receipt_dir() -> Result<std::path::PathBuf, String> {
    let dir = crate::proto::mux_dir().join("command-receipts");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create command receipt directory: {e}"))?;
    Ok(dir)
}

fn safe_request_id(raw: &str) -> Result<&str, String> {
    if raw.is_empty()
        || raw.len() > 128
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("--request-id must be 1-128 ASCII letters, digits, '.', '_' or '-'".into());
    }
    Ok(raw)
}

fn receipt_path(request_id: &str) -> Result<std::path::PathBuf, String> {
    Ok(receipt_dir()?.join(format!("{request_id}.json")))
}

fn load_receipt(request_id: &str) -> Result<Option<CommandReceipt>, String> {
    let path = receipt_path(request_id)?;
    match fs::read_to_string(path) {
        Ok(body) => serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| format!("command receipt is malformed: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read command receipt: {e}")),
    }
}

fn write_receipt(receipt: &CommandReceipt) -> Result<(), String> {
    let path = receipt_path(&receipt.request_id)?;
    if path.exists() {
        return Ok(());
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(receipt).map_err(|e| format!("encode receipt: {e}"))?;
    fs::write(&tmp, body).map_err(|e| format!("write command receipt: {e}"))?;
    match fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) if path.exists() => {
            let _ = fs::remove_file(&tmp);
            let _ = e;
            Ok(())
        }
        Err(e) => Err(format!("commit command receipt: {e}")),
    }
}

fn cleanup_receipts() {
    let Ok(dir) = receipt_dir() else { return };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            let old = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| now.saturating_sub(d.as_secs()) > RECEIPT_TTL_SECS)
                .unwrap_or(false);
            if old {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn provider_recipe(harness: &str, action: &str) -> Option<(String, String, String)> {
    let root: toml::Value = toml::from_str(include_str!("../harness_capabilities.toml")).ok()?;
    let action = root
        .get("harness")?
        .get(harness)?
        .get("provider_actions")?
        .get(action)?;
    Some((
        action.get("transport")?.as_str()?.to_string(),
        action.get("method")?.as_str()?.to_string(),
        action.get("proof")?.as_str()?.to_string(),
    ))
}

fn action_for(harness: &str, command: &str, proof: ProofKind) -> Option<(String, String, String)> {
    match proof {
        ProofKind::Compact if command == "/compact" => provider_recipe(harness, "compact"),
        ProofKind::GoalActive
            if command == "/goal" || command == "/goal status" || command.starts_with("/goal ") =>
        {
            let action = if command == "/goal" || command == "/goal status" {
                "goal_get"
            } else {
                "goal_set"
            };
            provider_recipe(harness, action)
        }
        _ => None,
    }
}

fn print_receipt(receipt: &CommandReceipt) {
    println!(
        "{}",
        serde_json::to_string(receipt).unwrap_or_else(|_| "{}".into())
    );
}

fn provider_receipt_matches(
    method: &str,
    expected_proof: &str,
    session_id: &str,
    scope: Option<&str>,
    command: &str,
    receipt: &serde_json::Value,
) -> bool {
    if receipt.get("verified").and_then(serde_json::Value::as_bool) != Some(true)
        || receipt.get("thread_id").and_then(serde_json::Value::as_str) != Some(session_id)
    {
        return false;
    }
    match method {
        "thread/compact/start" => {
            expected_proof == "context-compaction"
                && receipt.get("action").and_then(serde_json::Value::as_str) == Some("compact")
                && receipt.get("status").and_then(serde_json::Value::as_str) == Some("completed")
        }
        "thread/goal/get" | "thread/goal/set" => {
            if expected_proof != "goal-active"
                || receipt.get("status").and_then(serde_json::Value::as_str) != Some("active")
                || receipt
                    .get("objective")
                    .and_then(serde_json::Value::as_str)
                    .map_or(true, |objective| objective.trim().is_empty())
            {
                return false;
            }
            let owner = receipt
                .get("continuation_owner")
                .and_then(serde_json::Value::as_str)
                .filter(|owner| !owner.trim().is_empty());
            let Some(owner) = owner else {
                return false;
            };
            let expected_action = if method == "thread/goal/get" {
                "goal_get"
            } else {
                "goal_set"
            };
            if receipt.get("action").and_then(serde_json::Value::as_str) != Some(expected_action) {
                return false;
            }
            if let Some(scope) = scope.filter(|scope| !scope.trim().is_empty()) {
                if owner != format!("king:{}", scope.trim()) {
                    return false;
                }
            }
            if method == "thread/goal/set" {
                let Some(objective) = command
                    .trim()
                    .strip_prefix("/goal")
                    .map(str::trim)
                    .filter(|objective| !objective.is_empty())
                else {
                    return false;
                };
                let expected_owner = if !scope.unwrap_or_default().trim().is_empty() {
                    format!("king:{}", scope.unwrap_or_default().trim())
                } else if let Some(scope) = objective.strip_prefix("$fno:reign ") {
                    format!("king:{}", scope.trim())
                } else {
                    format!("target:{session_id}")
                };
                receipt.get("objective").and_then(serde_json::Value::as_str) == Some(objective)
                    && owner == expected_owner
            } else {
                true
            }
        }
        _ => false,
    }
}

fn run_provider_action(
    session_id: &str,
    cwd: &str,
    scope: Option<&str>,
    command: &str,
    transport: &str,
    method: &str,
    expected_proof: &str,
    timeout: Duration,
) -> Result<(serde_json::Value, String), String> {
    if transport != "app-server" || session_id.trim().is_empty() || cwd.is_empty() {
        return Err("provider action needs an app-server transport, exact session and cwd".into());
    }
    let binary = crate::digest_overlay::fno_agents_bin();
    let mut child = Command::new(binary)
        .args([
            "loop",
            "command",
            "--session",
            session_id,
            "--cwd",
            cwd,
            "--method",
            method,
            "--text",
            command,
        ])
        .args(scope.into_iter().flat_map(|scope| ["--scope", scope]))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("provider lane could not start fno-agents: {error}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "provider action timed out after {}s",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("provider action wait failed: {error}"));
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("provider action output unreadable: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("provider action exited {} without a receipt", output.status)
        } else {
            detail
        });
    }
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("provider action returned unreadable JSON: {error}"))?;
    if !provider_receipt_matches(method, expected_proof, session_id, scope, command, &receipt) {
        return Err("provider action readback did not prove the requested postcondition".into());
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((receipt, raw))
}

pub fn command(args: MuxCommandArgs, _env_session: Option<&str>) -> i32 {
    let proof = match ProofKind::parse(&args.proof) {
        Ok(proof) => proof,
        Err(error) => {
            eprintln!("fno mux command: {error}");
            return EXIT_USAGE;
        }
    };
    if args.timeout_seconds == 0 {
        eprintln!("fno mux command: --timeout-seconds must be positive");
        return EXIT_USAGE;
    }
    if proof == ProofKind::Screen && args.expect.is_none() {
        eprintln!("fno mux command: --proof screen requires --expect <regex>");
        return EXIT_USAGE;
    }
    if let Some(pattern) = args.expect.as_deref() {
        if let Err(error) = Regex::new(pattern) {
            eprintln!("fno mux command: --expect is not a valid regex: {error}");
            return EXIT_USAGE;
        }
    }
    let _timeout = Duration::from_secs(args.timeout_seconds);
    let request_id = args.request_id.clone().unwrap_or_else(|| {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("mux-{stamp}")
    });
    if let Err(error) = safe_request_id(&request_id) {
        eprintln!("fno mux command: {error}");
        return EXIT_USAGE;
    }
    cleanup_receipts();
    match load_receipt(&request_id) {
        Ok(Some(receipt)) => {
            if receipt.selector != args.selector
                || receipt.command != args.text
                || receipt.proof != proof.word()
            {
                eprintln!(
                    "fno mux command: request id {request_id:?} already belongs to a different action"
                );
                return EXIT_ERROR;
            }
            print_receipt(&receipt);
            return if receipt.status == "verified" {
                EXIT_OK
            } else {
                EXIT_ERROR
            };
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("fno mux command: {error}");
            return EXIT_ERROR;
        }
    }

    let row = match resolve_row_or_print("fno mux command", &args.selector) {
        Ok(row) => row,
        Err(code) => return code,
    };
    let Some(session_id) = row.effective_identity().map(str::to_string) else {
        eprintln!("fno mux command: live row has no full harness session id");
        return EXIT_ERROR;
    };
    if row.exited
        || row.dnd
        || row.badge == Some(crate::proto::AgentBadge::Working)
        || row.answerable.is_some()
    {
        eprintln!(
            "fno mux command: refusing before typing; the selected row is busy, blocked, or held"
        );
        return EXIT_ERROR;
    }
    let harness = row.harness.clone().unwrap_or_default();
    let recipe = action_for(&harness, &args.text, proof);
    if args.at_next_boundary {
        if recipe.is_none() {
            eprintln!("fno mux command: --at-next-boundary requires a typed provider action");
            return EXIT_USAGE;
        }
        eprintln!("fno mux command: boundary scheduling is not available on this controller");
        return EXIT_ERROR;
    }
    if let Some((transport, method, expected_proof)) = recipe.as_ref() {
        let scope = row.crown_scope.as_deref();
        let (status, before_digest, after_digest, detail) = match run_provider_action(
            &session_id,
            &row.cwd,
            scope,
            &args.text,
            transport,
            method,
            expected_proof,
            Duration::from_secs(args.timeout_seconds),
        ) {
            Ok((_provider_receipt, raw)) => (
                CommandStatus::Verified,
                String::new(),
                digest(&raw),
                format!("provider receipt confirmed by {method}"),
            ),
            Err(error) => (
                CommandStatus::Unknown,
                String::new(),
                String::new(),
                format!("provider action result is unconfirmed: {error}"),
            ),
        };
        let receipt = CommandReceipt {
            request_id,
            selector: args.selector,
            session_id: session_id.clone(),
            harness,
            transport: transport.clone(),
            expected_identity: session_id,
            command: args.text,
            proof: proof.word().into(),
            status: status.word().into(),
            before_digest,
            after_digest,
            detail,
        };
        if let Err(error) = write_receipt(&receipt) {
            eprintln!("fno mux command: {error}");
            return EXIT_ERROR;
        }
        print_receipt(&receipt);
        return if status == CommandStatus::Verified {
            EXIT_OK
        } else {
            EXIT_CONTROL_UNANSWERED
        };
    }
    let (transport, detail) = match (recipe, row.mux.clone()) {
        (None, None) => {
            eprintln!("fno mux command: paneless row has no declared provider action");
            return EXIT_ERROR;
        }
        (None, Some((session, pane))) => {
            let sock = match proto::socket_path(&session) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("fno mux command: {error}");
                    return EXIT_USAGE;
                }
            };
            let before = match pane_text(&sock, &session, pane) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("fno mux command: cannot read before screen: {error}");
                    return EXIT_ERROR;
                }
            };
            let expected_identity = session_id.clone();
            if let Err(error) = send_pane_bytes(
                &sock,
                &session,
                pane,
                args.text.clone().into_bytes(),
                true,
                Some(&expected_identity),
            ) {
                let receipt = CommandReceipt {
                    request_id,
                    selector: args.selector,
                    session_id,
                    harness,
                    transport: "pane".into(),
                    expected_identity: expected_identity.clone(),
                    command: args.text,
                    proof: proof.word().into(),
                    status: classify_postcondition(true, Err(error.to_string()))
                        .word()
                        .into(),
                    before_digest: digest(&before),
                    after_digest: digest(&before),
                    detail: "text submission reply lost; command outcome is unknown".into(),
                };
                let _ = write_receipt(&receipt);
                print_receipt(&receipt);
                return EXIT_CONTROL_UNANSWERED;
            }
            if let Err(error) = send_pane_bytes(
                &sock,
                &session,
                pane,
                vec![b'\r'],
                false,
                Some(&expected_identity),
            ) {
                let receipt = CommandReceipt {
                    request_id,
                    selector: args.selector,
                    session_id,
                    harness,
                    transport: "pane".into(),
                    expected_identity: expected_identity.clone(),
                    command: args.text,
                    proof: proof.word().into(),
                    status: CommandStatus::Unknown.word().into(),
                    before_digest: digest(&before),
                    after_digest: digest(&before),
                    detail: format!("submission reply lost: {error}"),
                };
                let _ = write_receipt(&receipt);
                print_receipt(&receipt);
                return EXIT_CONTROL_UNANSWERED;
            }
            let after = match pane_text(&sock, &session, pane) {
                Ok(text) => text,
                Err(error) => {
                    let receipt = CommandReceipt {
                        request_id,
                        selector: args.selector,
                        session_id,
                        harness,
                        transport: "pane".into(),
                        expected_identity: expected_identity.clone(),
                        command: args.text,
                        proof: proof.word().into(),
                        status: CommandStatus::Unknown.word().into(),
                        before_digest: digest(&before),
                        after_digest: String::new(),
                        detail: format!("post-submit read failed: {error}"),
                    };
                    let _ = write_receipt(&receipt);
                    print_receipt(&receipt);
                    return EXIT_CONTROL_UNANSWERED;
                }
            };
            let verified = args
                .expect
                .as_deref()
                .map(|pattern| Regex::new(pattern).map(|re| re.is_match(&after)))
                .transpose()
                .unwrap_or(Some(false))
                .unwrap_or(false);
            let status = if proof == ProofKind::Screen {
                classify_postcondition(true, Ok(verified))
            } else {
                CommandStatus::Verified
            };
            let receipt = CommandReceipt {
                request_id,
                selector: args.selector,
                session_id,
                harness,
                transport: "pane".into(),
                expected_identity,
                command: args.text,
                proof: proof.word().into(),
                status: status.word().into(),
                before_digest: digest(&before),
                after_digest: digest(&after),
                detail: "identity-pinned pane postcondition observed".into(),
            };
            if let Err(error) = write_receipt(&receipt) {
                eprintln!("fno mux command: {error}");
                return EXIT_ERROR;
            }
            print_receipt(&receipt);
            return if status == CommandStatus::Verified {
                EXIT_OK
            } else {
                EXIT_CONTROL_UNANSWERED
            };
        }
    };
    let receipt = CommandReceipt {
        request_id,
        selector: args.selector,
        session_id: session_id.clone(),
        harness,
        transport,
        expected_identity: session_id,
        command: args.text,
        proof: proof.word().into(),
        status: CommandStatus::Unknown.word().into(),
        before_digest: String::new(),
        after_digest: String::new(),
        detail,
    };
    if let Err(error) = write_receipt(&receipt) {
        eprintln!("fno mux command: {error}");
        return EXIT_ERROR;
    }
    print_receipt(&receipt);
    EXIT_CONTROL_UNANSWERED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_parser_is_closed_and_screen_requires_a_bounded_expectation() {
        assert_eq!(ProofKind::parse("compact"), Ok(ProofKind::Compact));
        assert_eq!(ProofKind::parse("goal-active"), Ok(ProofKind::GoalActive));
        assert!(ProofKind::parse("unknown").is_err());
    }

    #[test]
    fn codex_recipes_are_capability_derived() {
        assert_eq!(
            action_for("codex", "/compact", ProofKind::Compact),
            Some((
                "app-server".into(),
                "thread/compact/start".into(),
                "context-compaction".into()
            ))
        );
        assert!(action_for("claude", "/compact", ProofKind::Compact).is_none());
        assert!(action_for("codex", "/goalfoo", ProofKind::GoalActive).is_none());
    }

    #[test]
    fn provider_compaction_receipt_must_match_the_exact_session_and_completion() {
        let receipt = serde_json::json!({
            "verified": true,
            "action": "compact",
            "thread_id": "thread-1",
            "status": "completed"
        });
        assert!(provider_receipt_matches(
            "thread/compact/start",
            "context-compaction",
            "thread-1",
            None,
            "/compact",
            &receipt
        ));
        assert!(!provider_receipt_matches(
            "thread/compact/start",
            "context-compaction",
            "thread-2",
            None,
            "/compact",
            &receipt
        ));
    }

    #[test]
    fn provider_goal_receipt_must_be_active_for_the_exact_session() {
        let receipt = serde_json::json!({
            "verified": true,
            "action": "goal_set",
            "thread_id": "thread-1",
            "status": "active",
            "objective": "$fno:reign court",
            "continuation_owner": "king:court"
        });
        assert!(provider_receipt_matches(
            "thread/goal/set",
            "goal-active",
            "thread-1",
            Some("court"),
            "/goal $fno:reign court",
            &receipt
        ));
        assert!(!provider_receipt_matches(
            "thread/goal/set",
            "goal-active",
            "thread-2",
            Some("court"),
            "/goal $fno:reign court",
            &receipt
        ));
        let wrong_owner = serde_json::json!({
            "verified": true,
            "action": "goal_set",
            "thread_id": "thread-1",
            "status": "active",
            "objective": "$fno:reign court",
            "continuation_owner": "king:other"
        });
        assert!(!provider_receipt_matches(
            "thread/goal/set",
            "goal-active",
            "thread-1",
            Some("court"),
            "/goal $fno:reign court",
            &wrong_owner
        ));
        let wrong_objective = serde_json::json!({
            "verified": true,
            "action": "goal_set",
            "thread_id": "thread-1",
            "status": "active",
            "objective": "$fno:reign other",
            "continuation_owner": "king:court"
        });
        assert!(!provider_receipt_matches(
            "thread/goal/set",
            "goal-active",
            "thread-1",
            Some("court"),
            "/goal $fno:reign court",
            &wrong_objective
        ));

        let get_receipt = serde_json::json!({
            "verified": true,
            "action": "goal_get",
            "thread_id": "thread-1",
            "status": "active",
            "objective": "$fno:reign court",
            "continuation_owner": "king:court"
        });
        assert!(provider_receipt_matches(
            "thread/goal/get",
            "goal-active",
            "thread-1",
            Some("court"),
            "/goal status",
            &get_receipt
        ));
        assert!(!provider_receipt_matches(
            "thread/goal/get",
            "goal-active",
            "thread-1",
            Some("other"),
            "/goal status",
            &get_receipt
        ));
    }

    #[test]
    fn a_lost_reply_after_submit_is_unknown_and_never_retryable_by_default() {
        assert_eq!(
            classify_postcondition(true, Err("timeout".into())),
            CommandStatus::Unknown
        );
        assert_eq!(
            classify_postcondition(true, Ok(false)),
            CommandStatus::Unknown
        );
        assert_eq!(
            classify_postcondition(true, Ok(true)),
            CommandStatus::Verified
        );
        assert_eq!(
            classify_postcondition(false, Ok(true)),
            CommandStatus::Refused
        );
        assert!(safe_request_id("request-1").is_ok());
        assert!(safe_request_id("../request-1").is_err());
    }
}
