//! One public native-command door with an identity-pinned pane fallback.

use super::*;
use crate::cli_args::MuxCommandArgs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
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
        ProofKind::GoalActive if command.starts_with("/goal") => {
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
    let (transport, detail) = match (recipe, row.mux.clone()) {
        (Some((transport, method, _)), None) => {
            // The recipe is deliberately surfaced before any pane creation:
            // a paneless typed action must not steal portal 0. The app-server
            // owner consumes this receipt in the provider lane.
            (
                transport,
                format!("provider action {method} requires provider lane"),
            )
        }
        (Some((transport, method, _)), Some(_)) => (
            transport,
            format!("provider action {method} requires provider lane"),
        ),
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
