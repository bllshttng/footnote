//! One public native-command door with an identity-pinned pane fallback.

use super::*;
use crate::cli_args::MuxCommandArgs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RECEIPT_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const RECEIPT_RESERVED_DETAIL: &str =
    "request reserved before submission; a retry must not submit it again";

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

fn screen_postcondition_matches(before: &str, after: &str, expected: &Regex) -> bool {
    !expected.is_match(before)
        && expected.is_match(after)
        && super::pane_submit::positive_post_submit_marker(before, after)
}

fn empty_composer_marker_matches(screen: &str, expected: &Regex) -> bool {
    let mut markers = screen
        .lines()
        .filter(|line| !line.trim().is_empty() && expected.is_match(line));
    let Some(marker) = markers.next() else {
        return false;
    };
    markers.next().is_none() && !expected.is_match(&format!("{marker}draft"))
}

fn empty_composer_pattern(raw: &str) -> Result<Regex, String> {
    if !raw.contains('^') || !raw.contains('$') {
        return Err("--empty-composer must be an anchored line regex".into());
    }
    let regex =
        Regex::new(raw).map_err(|error| format!("--empty-composer regex is invalid: {error}"))?;
    if [
        "ordinary screen text",
        "drafted prompt text",
        "contextCompaction output",
    ]
    .iter()
    .any(|sample| regex.is_match(sample))
    {
        return Err("--empty-composer regex also matches ordinary screen text".into());
    }
    Ok(regex)
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
    expected_screen: Option<String>,
    empty_composer: Option<String>,
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
    let existing = load_receipt(&receipt.request_id)?;
    let Some(existing) = existing else {
        return Err("command receipt was not reserved before submission".into());
    };
    if existing.detail != RECEIPT_RESERVED_DETAIL
        || existing.status != CommandStatus::Unknown.word()
        || existing.selector != receipt.selector
        || existing.session_id != receipt.session_id
        || existing.harness != receipt.harness
        || existing.transport != receipt.transport
        || existing.expected_identity != receipt.expected_identity
        || existing.command != receipt.command
        || existing.proof != receipt.proof
        || existing.expected_screen != receipt.expected_screen
        || existing.empty_composer != receipt.empty_composer
    {
        return Err("command receipt reservation does not match the submitted action".into());
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

fn reserve_receipt(receipt: &CommandReceipt) -> Result<bool, String> {
    let path = receipt_path(&receipt.request_id)?;
    reserve_receipt_at(&path, receipt)
}

fn reserve_receipt_at(path: &std::path::Path, receipt: &CommandReceipt) -> Result<bool, String> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(format!("cannot reserve command receipt: {error}")),
    };
    let body = serde_json::to_vec_pretty(receipt)
        .map_err(|error| format!("encode command receipt reservation: {error}"))?;
    if let Err(error) = file.write_all(&body).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(format!("write command receipt reservation: {error}"));
    }
    Ok(true)
}

fn write_refused_receipt(receipt: &CommandReceipt) -> Result<(), String> {
    let path = receipt_path(&receipt.request_id)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("cannot record refusal receipt: {error}"))?;
    let body = serde_json::to_vec_pretty(receipt)
        .map_err(|error| format!("encode refusal receipt: {error}"))?;
    file.write_all(&body)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write refusal receipt: {error}"))
}

fn command_status_exit_code(status: &str) -> i32 {
    match status {
        "verified" => EXIT_OK,
        "unknown" => EXIT_CONTROL_UNANSWERED,
        _ => EXIT_ERROR,
    }
}

fn pane_infos(sock: &Path, session: &str) -> Result<Vec<proto::PaneInfo>, String> {
    match control_roundtrip(sock, session, ControlVerb::PaneLs)
        .map_err(|error| format!("portal pane list unreadable: {error}"))?
    {
        ServerMsg::PaneList { panes } => Ok(panes),
        ServerMsg::Err { msg, .. } => Err(format!("portal pane list refused: {msg}")),
        other => Err(format!("unexpected portal pane-list reply: {other:?}")),
    }
}

struct OwnedPortal {
    sock: std::path::PathBuf,
    session: String,
    pane: u64,
    index: u8,
    identity: String,
}

impl Drop for OwnedPortal {
    fn drop(&mut self) {
        let still_owned = pane_infos(&self.sock, &self.session)
            .ok()
            .is_some_and(|panes| {
                panes.iter().any(|pane| {
                    pane.pane_id == self.pane
                        && pane.fno_id.as_deref() == Some(self.identity.as_str())
                })
            });
        if !still_owned {
            eprintln!(
                "fno mux command: leaving portal {} pane {} open because its identity changed",
                self.index, self.pane
            );
            return;
        }
        match control_roundtrip(
            &self.sock,
            &self.session,
            ControlVerb::PaneKill {
                pane: self.pane,
                hand_off_to: None,
            },
        ) {
            Ok(ServerMsg::Ok) => {}
            Ok(ServerMsg::Err { msg, .. }) => eprintln!(
                "fno mux command: owned portal {} pane {} cleanup refused: {msg}",
                self.index, self.pane
            ),
            Ok(other) => eprintln!(
                "fno mux command: owned portal {} pane {} cleanup returned {other:?}",
                self.index, self.pane
            ),
            Err(error) => eprintln!(
                "fno mux command: owned portal {} pane {} cleanup failed: {error}",
                self.index, self.pane
            ),
        }
    }
}

fn open_command_portal(
    row_name: &str,
    session_id: &str,
    env_session: Option<&str>,
) -> Result<OwnedPortal, String> {
    let session = resolve_session(None, env_session);
    let sock = proto::socket_path(&session).map_err(|error| error.to_string())?;
    let before = pane_infos(&sock, &session)?;
    if before
        .iter()
        .any(|pane| pane.fno_id.as_deref() == Some(session_id))
    {
        return Err("paneless row already has a pane view; refusing to repoint it".into());
    }
    let placement = PanePlacement {
        portal_new: true,
        ..PanePlacement::default()
    };
    let landing = match control_roundtrip(
        &sock,
        &session,
        ControlVerb::ThreadPane {
            name: row_name.to_string(),
            portal: None,
            placement,
        },
    )
    .map_err(|error| format!("new portal result unknown: {error}"))?
    {
        ServerMsg::Notice { text } => text,
        ServerMsg::Err { msg, .. } => return Err(format!("new portal refused: {msg}")),
        other => return Err(format!("unexpected new portal reply: {other:?}")),
    };
    if !landing.contains("thread pane ->") {
        return Err(format!("new portal did not confirm a landing: {landing}"));
    }
    let portal_index = Regex::new(r"\(portal ([0-9]+)\)")
        .ok()
        .and_then(|regex| regex.captures(&landing))
        .and_then(|captures| captures.get(1))
        .and_then(|index| index.as_str().parse::<u8>().ok())
        .ok_or_else(|| format!("new portal reply has no owned portal index: {landing}"))?;
    let after = pane_infos(&sock, &session)?;
    let before_ids = before.iter().map(|pane| pane.pane_id).collect::<Vec<_>>();
    let added = after
        .iter()
        .filter(|pane| {
            pane.fno_id.as_deref() == Some(session_id) && !before_ids.contains(&pane.pane_id)
        })
        .collect::<Vec<_>>();
    if added.len() != 1 {
        return Err(format!(
            "portal {portal_index} landed without exactly one new pane joined to session {session_id}"
        ));
    }
    Ok(OwnedPortal {
        sock,
        session,
        pane: added[0].pane_id,
        index: portal_index,
        identity: session_id.to_string(),
    })
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
                let resume = command.trim() == "/goal resume";
                let objective = if resume {
                    let Some(scope) = scope.filter(|scope| !scope.trim().is_empty()) else {
                        return false;
                    };
                    format!("$fno:reign {}", scope.trim())
                } else {
                    let Some(objective) = command
                        .trim()
                        .strip_prefix("/goal")
                        .map(str::trim)
                        .filter(|objective| !objective.is_empty())
                    else {
                        return false;
                    };
                    objective.to_string()
                };
                let expected_owner = if !scope.unwrap_or_default().trim().is_empty() {
                    format!("king:{}", scope.unwrap_or_default().trim())
                } else if let Some(scope) = objective.strip_prefix("$fno:reign ") {
                    format!("king:{}", scope.trim())
                } else {
                    format!("target:{session_id}")
                };
                receipt.get("objective").and_then(serde_json::Value::as_str)
                    == Some(objective.as_str())
                    && owner == expected_owner
                    && (!resume
                        || receipt
                            .get("previous_status")
                            .and_then(serde_json::Value::as_str)
                            == Some("paused"))
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

pub fn command(args: MuxCommandArgs, env_session: Option<&str>) -> i32 {
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
    if proof == ProofKind::Screen && args.empty_composer.is_none() {
        eprintln!("fno mux command: --proof screen requires --empty-composer <regex>");
        return EXIT_USAGE;
    }
    if let Some(pattern) = args.expect.as_deref() {
        if let Err(error) = Regex::new(pattern) {
            eprintln!("fno mux command: --expect is not a valid regex: {error}");
            return EXIT_USAGE;
        }
    }
    if let Some(pattern) = args.empty_composer.as_deref() {
        if let Err(error) = empty_composer_pattern(pattern) {
            eprintln!("fno mux command: {error}");
            return EXIT_USAGE;
        }
    }
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
                || receipt.expected_screen != args.expect
                || receipt.empty_composer != args.empty_composer
            {
                eprintln!(
                    "fno mux command: request id {request_id:?} already belongs to a different action"
                );
                return EXIT_ERROR;
            }
            print_receipt(&receipt);
            return command_status_exit_code(&receipt.status);
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
    let harness = row.harness.clone().unwrap_or_default();
    let recipe = action_for(&harness, &args.text, proof);
    let refusal = if row.exited {
        Some("selected row is exited")
    } else if row.dnd {
        Some("selected row is held by DND")
    } else if row.answerable.is_some() {
        Some("selected row has a pending composer")
    } else if row.badge == Some(crate::proto::AgentBadge::Working) {
        Some("selected row is busy")
    } else {
        None
    };
    if let Some(reason) = refusal {
        let (before_digest, after_digest) = row
            .mux
            .as_ref()
            .and_then(|(mux_session, pane)| {
                let sock = proto::socket_path(mux_session).ok()?;
                let before = pane_text(&sock, mux_session, *pane).ok()?;
                let after = pane_text(&sock, mux_session, *pane).ok()?;
                Some((digest(&before), digest(&after)))
            })
            .unwrap_or_default();
        let receipt = CommandReceipt {
            request_id,
            selector: args.selector,
            session_id: session_id.clone(),
            harness,
            transport: recipe
                .as_ref()
                .map(|(transport, _, _)| transport.clone())
                .unwrap_or_else(|| if row.mux.is_some() { "pane" } else { "portal" }.into()),
            expected_identity: session_id,
            command: args.text,
            proof: proof.word().into(),
            expected_screen: args.expect.clone(),
            empty_composer: args.empty_composer.clone(),
            status: CommandStatus::Refused.word().into(),
            before_digest: before_digest.clone(),
            after_digest: after_digest.clone(),
            detail: format!(
                "refused before typing: {reason}; screen {}",
                if !before_digest.is_empty() && before_digest == after_digest {
                    "unchanged"
                } else {
                    "unchanged state unverified"
                }
            ),
        };
        if let Err(error) = write_refused_receipt(&receipt) {
            eprintln!("fno mux command: {error}");
            return EXIT_ERROR;
        }
        eprintln!("fno mux command: {}", receipt.detail);
        print_receipt(&receipt);
        return EXIT_ERROR;
    }
    if recipe.is_none() && row.mux.is_none() && row.attach_id.is_none() {
        let receipt = CommandReceipt {
            request_id,
            selector: args.selector,
            session_id: session_id.clone(),
            harness,
            transport: "portal".into(),
            expected_identity: session_id,
            command: args.text,
            proof: proof.word().into(),
            expected_screen: args.expect,
            empty_composer: args.empty_composer,
            status: CommandStatus::Refused.word().into(),
            before_digest: String::new(),
            after_digest: String::new(),
            detail: "refused before typing: this paneless row has no interactive attach; a read-only portal is not a command transport".into(),
        };
        if let Err(error) = write_refused_receipt(&receipt) {
            eprintln!("fno mux command: {error}");
            return EXIT_ERROR;
        }
        eprintln!("fno mux command: {}", receipt.detail);
        print_receipt(&receipt);
        return EXIT_ERROR;
    }
    if recipe.is_none() && proof != ProofKind::Screen {
        eprintln!(
            "fno mux command: {:?} proof requires a declared provider action",
            proof.word()
        );
        return EXIT_USAGE;
    }
    let transport = recipe
        .as_ref()
        .map(|(transport, _, _)| transport.as_str())
        .unwrap_or(if row.mux.is_some() { "pane" } else { "portal" });
    let reservation = CommandReceipt {
        request_id: request_id.clone(),
        selector: args.selector.clone(),
        session_id: session_id.clone(),
        harness: harness.clone(),
        transport: transport.to_string(),
        expected_identity: session_id.clone(),
        command: args.text.clone(),
        proof: proof.word().into(),
        expected_screen: args.expect.clone(),
        empty_composer: args.empty_composer.clone(),
        status: CommandStatus::Unknown.word().into(),
        before_digest: String::new(),
        after_digest: String::new(),
        detail: RECEIPT_RESERVED_DETAIL.into(),
    };
    match reserve_receipt(&reservation) {
        Ok(true) => {}
        Ok(false) => match load_receipt(&request_id) {
            Ok(Some(receipt))
                if receipt.selector == args.selector
                    && receipt.command == args.text
                    && receipt.proof == proof.word()
                    && receipt.expected_screen == args.expect
                    && receipt.empty_composer == args.empty_composer
                    && receipt.session_id == session_id =>
            {
                print_receipt(&receipt);
                return command_status_exit_code(&receipt.status);
            }
            Ok(Some(_)) => {
                eprintln!(
                        "fno mux command: request id {request_id:?} already belongs to a different action"
                    );
                return EXIT_ERROR;
            }
            Ok(None) => {
                eprintln!("fno mux command: request id {request_id:?} is already reserved");
                return EXIT_CONTROL_UNANSWERED;
            }
            Err(error) => {
                eprintln!("fno mux command: {error}");
                return EXIT_CONTROL_UNANSWERED;
            }
        },
        Err(error) => {
            eprintln!("fno mux command: {error}");
            return EXIT_ERROR;
        }
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
            expected_screen: args.expect.clone(),
            empty_composer: args.empty_composer.clone(),
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
    let (session, pane, _owned_portal) = match row.mux.clone() {
        Some((session, pane)) => (session, pane, None),
        None => match open_command_portal(&row.name, &session_id, env_session) {
            Ok(portal) => (portal.session.clone(), portal.pane, Some(portal)),
            Err(error) => {
                let mut receipt = reservation.clone();
                receipt.detail = format!("paneless portal result unknown: {error}");
                if let Err(write_error) = write_receipt(&receipt) {
                    eprintln!("fno mux command: {write_error}");
                    return EXIT_ERROR;
                }
                print_receipt(&receipt);
                return EXIT_CONTROL_UNANSWERED;
            }
        },
    };
    let sock = if let Some(portal) = _owned_portal.as_ref() {
        portal.sock.clone()
    } else {
        match proto::socket_path(&session) {
            Ok(path) => path,
            Err(error) => {
                let mut receipt = reservation.clone();
                receipt.status = CommandStatus::Refused.word().into();
                receipt.detail =
                    format!("refused before typing: mux socket is unavailable: {error}");
                if let Err(write_error) = write_receipt(&receipt) {
                    eprintln!("fno mux command: {write_error}");
                    return EXIT_ERROR;
                }
                print_receipt(&receipt);
                return EXIT_USAGE;
            }
        }
    };
    let before = match pane_text(&sock, &session, pane) {
        Ok(text) => text,
        Err(error) => {
            let mut receipt = reservation.clone();
            receipt.status = CommandStatus::Refused.word().into();
            receipt.detail = format!("refused before typing: cannot read before screen: {error}");
            if let Err(write_error) = write_receipt(&receipt) {
                eprintln!("fno mux command: {write_error}");
                return EXIT_ERROR;
            }
            print_receipt(&receipt);
            return EXIT_ERROR;
        }
    };
    let empty_composer = empty_composer_pattern(
        args.empty_composer
            .as_deref()
            .expect("screen proof validates its empty-composer regex before submission"),
    )
    .expect("screen proof validated its empty-composer regex before submission");
    if !empty_composer_marker_matches(&before, &empty_composer) {
        let mut receipt = reservation.clone();
        receipt.status = CommandStatus::Refused.word().into();
        receipt.before_digest = digest(&before);
        receipt.after_digest = receipt.before_digest.clone();
        receipt.detail =
            "refused before typing: screen did not prove the composer empty; screen unchanged"
                .into();
        if let Err(error) = write_receipt(&receipt) {
            eprintln!("fno mux command: {error}");
            return EXIT_ERROR;
        }
        eprintln!("fno mux command: {}", receipt.detail);
        print_receipt(&receipt);
        return EXIT_ERROR;
    }
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
            transport: reservation.transport.clone(),
            expected_identity: expected_identity.clone(),
            command: args.text,
            proof: proof.word().into(),
            expected_screen: args.expect.clone(),
            empty_composer: args.empty_composer.clone(),
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
            transport: reservation.transport.clone(),
            expected_identity: expected_identity.clone(),
            command: args.text,
            proof: proof.word().into(),
            expected_screen: args.expect.clone(),
            empty_composer: args.empty_composer.clone(),
            status: CommandStatus::Unknown.word().into(),
            before_digest: digest(&before),
            after_digest: digest(&before),
            detail: format!("submission reply lost: {error}"),
        };
        let _ = write_receipt(&receipt);
        print_receipt(&receipt);
        return EXIT_CONTROL_UNANSWERED;
    }
    let expected = args
        .expect
        .as_deref()
        .and_then(|pattern| Regex::new(pattern).ok())
        .expect("screen proof validates its expected regex before submission");
    let deadline = Instant::now() + Duration::from_secs(args.timeout_seconds);
    let mut after = String::new();
    let mut verified = false;
    let mut last_read_error = None;
    loop {
        match pane_text(&sock, &session, pane) {
            Ok(text) => {
                verified = screen_postcondition_matches(&before, &text, &expected);
                after = text;
                last_read_error = None;
                if verified {
                    break;
                }
            }
            Err(error) => last_read_error = Some(error.to_string()),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(Duration::from_millis(100).min(remaining));
    }
    let status = classify_postcondition(true, Ok(verified));
    let receipt = CommandReceipt {
        request_id,
        selector: args.selector,
        session_id,
        harness,
        transport: reservation.transport.clone(),
        expected_identity,
        command: args.text,
        proof: proof.word().into(),
        expected_screen: args.expect.clone(),
        empty_composer: args.empty_composer.clone(),
        status: status.word().into(),
        before_digest: digest(&before),
        after_digest: digest(&after),
        detail: if verified {
            "identity-pinned pane postcondition observed".into()
        } else if let Some(error) = last_read_error {
            format!("screen postcondition unreadable before timeout: {error}")
        } else {
            format!(
                "screen postcondition not observed within {}s",
                args.timeout_seconds
            )
        },
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

        let resumed_receipt = serde_json::json!({
            "verified": true,
            "action": "goal_set",
            "thread_id": "thread-1",
            "status": "active",
            "previous_status": "paused",
            "objective": "$fno:reign court",
            "continuation_owner": "king:court"
        });
        assert!(provider_receipt_matches(
            "thread/goal/set",
            "goal-active",
            "thread-1",
            Some("court"),
            "/goal resume",
            &resumed_receipt
        ));
        let missing_pause = serde_json::json!({
            "verified": true,
            "action": "goal_set",
            "thread_id": "thread-1",
            "status": "active",
            "objective": "$fno:reign court",
            "continuation_owner": "king:court"
        });
        assert!(!provider_receipt_matches(
            "thread/goal/set",
            "goal-active",
            "thread-1",
            Some("court"),
            "/goal resume",
            &missing_pause
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

    #[test]
    fn screen_proof_requires_a_new_post_submit_marker() {
        let expected = Regex::new("contextCompaction").unwrap();
        assert!(!screen_postcondition_matches(
            "contextCompaction already shown",
            "contextCompaction already shown",
            &expected
        ));
        assert!(screen_postcondition_matches(
            "idle",
            "contextCompaction started",
            &expected
        ));
    }

    #[test]
    fn empty_composer_proof_requires_a_specific_anchored_marker() {
        let prompt = empty_composer_pattern("(?m)^❯\\s*$").unwrap();
        assert!(empty_composer_marker_matches("status\n❯\n", &prompt));
        assert!(!empty_composer_marker_matches(
            "status\n❯ drafted text\n",
            &prompt
        ));
        assert!(!empty_composer_marker_matches("status\n", &prompt));
        assert!(empty_composer_pattern(".+").is_err());
        assert!(empty_composer_pattern("(?m)^.*$").is_err());
    }

    #[test]
    fn command_request_reservation_is_exclusive_before_submission() {
        let root = std::env::temp_dir().join(format!(
            "fno-command-receipt-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("request.json");
        let receipt = CommandReceipt {
            request_id: "request-1".into(),
            selector: "thread-full-id".into(),
            session_id: "thread-full-id".into(),
            harness: "codex".into(),
            transport: "app-server".into(),
            expected_identity: "thread-full-id".into(),
            command: "/compact".into(),
            proof: "compact".into(),
            expected_screen: None,
            empty_composer: None,
            status: CommandStatus::Unknown.word().into(),
            before_digest: String::new(),
            after_digest: String::new(),
            detail: RECEIPT_RESERVED_DETAIL.into(),
        };
        assert!(reserve_receipt_at(&path, &receipt).unwrap());
        assert!(!reserve_receipt_at(&path, &receipt).unwrap());
        let stored: CommandReceipt = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.detail, RECEIPT_RESERVED_DETAIL);
        fs::remove_dir_all(root).unwrap();
    }
}
