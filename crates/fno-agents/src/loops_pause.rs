//! Loop pause presentation and compatibility reads for the Rust runtime.
//!
//! The fleet incident breaker owns new machine and targeted halts. This module
//! keeps the legacy sentinel read-only so an older pause survives migration;
//! read failures remain fail-closed because a broken safety switch must not
//! silently resume dispatch.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SENTINEL_NAME: &str = "loops-paused.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseState {
    Clear,
    Paused {
        who: String,
        paused_at: u64,
        expires_at: Option<u64>,
        reason: Option<String>,
    },
    Expired {
        who: String,
        paused_at: u64,
        expires_at: u64,
        reason: Option<String>,
    },
    Corrupt {
        path: PathBuf,
        error: String,
    },
}

impl PauseState {
    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused { .. } | Self::Corrupt { .. })
    }

    fn json(&self) -> Value {
        match self {
            Self::Clear => json!({"paused": false, "state": "clear"}),
            Self::Paused {
                who,
                paused_at,
                expires_at,
                reason,
            } => json!({
                "paused": true,
                "state": "paused",
                "who": who,
                "paused_at": paused_at,
                "expires_at": expires_at,
                "reason": reason,
            }),
            Self::Expired {
                who,
                paused_at,
                expires_at,
                reason,
            } => json!({
                "paused": false,
                "state": "expired",
                "who": who,
                "paused_at": paused_at,
                "expires_at": expires_at,
                "reason": reason,
            }),
            Self::Corrupt { path, error } => json!({
                "paused": true,
                "state": "corrupt",
                "path": path,
                "error": error,
            }),
        }
    }

    pub fn who(&self) -> Option<&str> {
        match self {
            Self::Paused { who, .. } | Self::Expired { who, .. } => Some(who),
            Self::Clear | Self::Corrupt { .. } => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Paused {
                who, reason: None, ..
            } => format!("loops paused by {who}"),
            Self::Paused {
                who,
                reason: Some(reason),
                ..
            } => format!("loops paused by {who}: {reason}"),
            Self::Corrupt { path, .. } => {
                format!("loops pause sentinel corrupt at {}", path.display())
            }
            Self::Clear | Self::Expired { .. } => "loops are not paused".to_string(),
        }
    }
}

/// The effective dispatch pause: what a dispatch-oriented poller
/// must obey. Combines the operator's manual sentinel with the fleet
/// incident verdict. Both block; when both are present the manual one wins
/// only for display, because it names the operator's own hand. `Clear` is
/// the only not-paused answer - an unreadable incident fails closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchPause {
    Clear,
    Manual {
        state: String,
        detail: String,
    },
    FleetIncident {
        generation: u64,
        reason: String,
        /// The scopes the record holds, as the readout speaks them. Machine
        /// reads use `spawns`; mission reads also match targeted territories.
        holds: Vec<String>,
    },
    FleetIncidentUnavailable {
        detail: String,
    },
}

impl DispatchPause {
    pub fn is_paused(&self) -> bool {
        !matches!(self, Self::Clear)
    }

    /// The tick-row skip token. One vocabulary shared by the active-backlog
    /// supervisor, the mission loop, and the daemon's stale sweep.
    pub fn skip_reason(&self) -> &'static str {
        match self {
            Self::Clear => "",
            Self::Manual { .. } => "loops_paused",
            Self::FleetIncident { .. } => "fleet_stop",
            Self::FleetIncidentUnavailable { .. } => "fleet_stop_unavailable",
        }
    }

    /// Short human detail for a tick row or event.
    pub fn detail(&self) -> String {
        match self {
            Self::Clear => String::new(),
            Self::Manual { detail, .. } => detail.clone(),
            Self::FleetIncident {
                generation, reason, ..
            } => {
                format!("fleet incident stopped at generation {generation}: {reason}")
            }
            Self::FleetIncidentUnavailable { detail } => {
                format!("fleet incident state unreadable: {detail}")
            }
        }
    }
}

/// Combine a manual sentinel state with a fleet verdict. Manual wins for
/// display when both are present; both block.
fn combine(manual: &PauseState, incident: crate::fleet_incident::Verdict) -> DispatchPause {
    if manual.is_paused() {
        let state = match manual {
            PauseState::Paused { .. } => "paused",
            PauseState::Corrupt { .. } => "corrupt",
            _ => unreachable!("is_paused true implies Paused or Corrupt"),
        };
        return DispatchPause::Manual {
            state: state.to_string(),
            detail: manual.message(),
        };
    }
    match incident {
        crate::fleet_incident::Verdict::Clear(_) => DispatchPause::Clear,
        crate::fleet_incident::Verdict::Stopped(r) if r.origin.as_deref() == Some("pause-all") => {
            DispatchPause::Manual {
                state: "paused".to_string(),
                detail: format!("loops paused by {}: {}", r.changed_by, r.reason),
            }
        }
        crate::fleet_incident::Verdict::Stopped(r) => DispatchPause::FleetIncident {
            holds: r.held_scopes(),
            generation: r.generation,
            reason: match r.target {
                Some(target) => format!("{target}: {}", r.reason),
                None => r.reason,
            },
        },
        crate::fleet_incident::Verdict::Unavailable(d) => {
            DispatchPause::FleetIncidentUnavailable { detail: d }
        }
    }
}

/// The effective dispatch pause for this machine. The incident read is
/// spawns-scoped: loop dispatch is automatic spawning, so a stop that holds
/// only tests or merges never pauses it.
pub fn dispatch_pause() -> DispatchPause {
    combine(&read_state(), crate::fleet_incident::verdict_for("spawns"))
}

/// The effective dispatch pause for one mission subject.
pub fn dispatch_pause_for_territory(subject: &crate::fleet_incident::Subject<'_>) -> DispatchPause {
    combine(
        &read_state(),
        crate::fleet_incident::verdict_for_subject("spawns", subject),
    )
}

/// The combined `loops paused --json` answer: `paused`, `source`, `state`,
/// and the incident generation/reason or unavailable detail when present.
/// The Python adapter reads only `paused`, so the added fields stay additive.
fn paused_json() -> Value {
    // One read feeds the verdict and fallback fields; a second could straddle
    // a resume and print clear for a pause this call already saw.
    let manual = read_state();
    let incident = crate::fleet_incident::verdict_for("spawns");
    match combine(&manual, incident.clone()) {
        DispatchPause::Clear => json!({"paused": false, "source": "none", "state": "clear"}),
        DispatchPause::Manual { .. } => {
            if manual.is_paused() {
                let mut v = manual.json();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("source".into(), json!("manual"));
                }
                return v;
            }
            match incident {
                crate::fleet_incident::Verdict::Stopped(record) => json!({
                    "paused": true,
                    "source": "fleet_incident",
                    "state": "fleet_stop",
                    "generation": record.generation,
                    "reason": record.reason,
                }),
                crate::fleet_incident::Verdict::Unavailable(detail) => json!({
                    "paused": true,
                    "source": "fleet_incident_unavailable",
                    "state": "fleet_stop_unavailable",
                    "detail": detail,
                }),
                crate::fleet_incident::Verdict::Clear(_) => {
                    json!({"paused": false, "source": "none", "state": "clear"})
                }
            }
        }
        DispatchPause::FleetIncident {
            generation, reason, ..
        } => json!({
            "paused": true,
            "source": "fleet_incident",
            "state": "fleet_stop",
            "generation": generation,
            "reason": reason,
        }),
        DispatchPause::FleetIncidentUnavailable { detail } => json!({
            "paused": true,
            "source": "fleet_incident_unavailable",
            "state": "fleet_stop_unavailable",
            "detail": detail,
        }),
    }
}

fn machine_status(record: &crate::fleet_incident::IncidentRecord, state: &str) -> Value {
    let millis = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|date| date.timestamp_millis().max(0) as u64)
            .unwrap_or_default()
    };
    json!({
        "paused": state == "paused",
        "state": state,
        "who": record.changed_by,
        "paused_at": millis(&record.changed_at),
        "expires_at": record.expires_at.as_deref().map(millis),
        "reason": record.reason,
    })
}

fn status_json() -> Value {
    let legacy = read_state();
    if matches!(
        &legacy,
        PauseState::Paused { .. } | PauseState::Corrupt { .. }
    ) {
        return legacy.json();
    }
    let home = crate::paths::AgentsHome::from_env();
    match crate::fleet_incident::read_at(&crate::fleet_incident::fleet_stop_path(&home)) {
        crate::fleet_incident::Verdict::Stopped(record) if record.holds_scope("loops") => {
            machine_status(&record, "paused")
        }
        crate::fleet_incident::Verdict::Stopped(_) => json!({"paused": false, "state": "clear"}),
        crate::fleet_incident::Verdict::Clear(record) if record.state == "stopped" => {
            machine_status(&record, "expired")
        }
        crate::fleet_incident::Verdict::Clear(_) => legacy.json(),
        crate::fleet_incident::Verdict::Unavailable(detail) => {
            json!({"paused": true, "state": "unavailable", "error": detail})
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn sentinel_path() -> PathBuf {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    home.join(".fno").join(SENTINEL_NAME)
}

pub fn read_state() -> PauseState {
    read_state_at(&sentinel_path(), now_ms())
}

fn read_state_at(path: &Path, now: u64) -> PauseState {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PauseState::Clear,
        Err(error) => {
            return PauseState::Corrupt {
                path: path.to_path_buf(),
                error: error.to_string(),
            }
        }
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return PauseState::Corrupt {
                path: path.to_path_buf(),
                error: error.to_string(),
            }
        }
    };
    let Some(object) = value.as_object() else {
        return corrupt(path, "sentinel must be a JSON object");
    };
    let Some(who) = object.get("who").and_then(Value::as_str) else {
        return corrupt(path, "sentinel is missing string field 'who'");
    };
    let Some(paused_at) = object.get("paused_at").and_then(Value::as_u64) else {
        return corrupt(path, "sentinel is missing integer field 'paused_at'");
    };
    let expires_at = match object.get("expires_at") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(value) => Some(value),
            None => {
                return corrupt(
                    path,
                    "sentinel field 'expires_at' must be an integer or null",
                )
            }
        },
    };
    // Absent on a sentinel written before this field existed - stays valid,
    // never corrupt, so an older writer's sentinel keeps classifying.
    let reason = match object.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return corrupt(path, "sentinel field 'reason' must be a string or null"),
    };
    if let Some(expires_at) = expires_at {
        if expires_at <= now {
            return PauseState::Expired {
                who: who.to_string(),
                paused_at,
                expires_at,
                reason,
            };
        }
    }
    PauseState::Paused {
        who: who.to_string(),
        paused_at,
        expires_at,
        reason,
    }
}

fn corrupt(path: &Path, error: impl Into<String>) -> PauseState {
    PauseState::Corrupt {
        path: path.to_path_buf(),
        error: error.into(),
    }
}

/// The effective dispatch verdict: manual sentinel OR fleet incident.
pub fn is_paused() -> bool {
    dispatch_pause().is_paused()
}

/// The stop hook's hold read for one subject: the legacy sentinel, its
/// loop-scoped fleet verdict, or a cargo build waiting on build admission.
/// A held worker whose stop hook missed any of them would count every fire
/// as NoProgress and die on a hold it was told to obey.
pub fn pause_message(subject: &crate::fleet_incident::Subject<'_>) -> Option<String> {
    pause_message_for(
        &read_state(),
        crate::fleet_incident::verdict_for_subject("loops", subject),
    )
    .or_else(|| crate::test_run::build_hold_message(subject.cwd))
}

fn pause_message_for(
    manual: &PauseState,
    incident: crate::fleet_incident::Verdict,
) -> Option<String> {
    let pause = combine(manual, incident);
    pause.is_paused().then(|| pause.detail())
}

struct PauseOptions {
    who: String,
    ttl_ms: Option<u64>,
    reason: Option<String>,
}

fn parse_pause_options(args: &[String]) -> Result<PauseOptions, String> {
    let mut who = "operator".to_string();
    let mut ttl_ms = None;
    let mut reason = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--who" {
            let Some(next) = args.get(i + 1) else {
                return Err("--who requires a value".to_string());
            };
            who = next.clone();
            i += 1;
        } else if let Some(inline) = args[i].strip_prefix("--who=") {
            who = inline.to_string();
        } else if args[i] == "--ttl-ms" {
            let Some(next) = args.get(i + 1) else {
                return Err("--ttl-ms requires a value".to_string());
            };
            let parsed = next
                .parse::<u64>()
                .map_err(|_| format!("invalid --ttl-ms: {next}"))?;
            if parsed == 0 {
                return Err("--ttl-ms must be > 0".to_string());
            }
            ttl_ms = Some(parsed);
            i += 1;
        } else if let Some(inline) = args[i].strip_prefix("--ttl-ms=") {
            let parsed = inline
                .parse::<u64>()
                .map_err(|_| format!("invalid --ttl-ms: {inline}"))?;
            if parsed == 0 {
                return Err("--ttl-ms must be > 0".to_string());
            }
            ttl_ms = Some(parsed);
        } else if args[i] == "--ttl" {
            let Some(next) = args.get(i + 1) else {
                return Err("--ttl requires a value".to_string());
            };
            ttl_ms = Some(crate::fleet_incident::parse_ttl(next)?);
            i += 1;
        } else if let Some(inline) = args[i].strip_prefix("--ttl=") {
            ttl_ms = Some(crate::fleet_incident::parse_ttl(inline)?);
        } else if args[i] == "--reason" {
            let Some(next) = args.get(i + 1) else {
                return Err("--reason requires a value".to_string());
            };
            reason = Some(next.clone());
            i += 1;
        } else if let Some(inline) = args[i].strip_prefix("--reason=") {
            reason = Some(inline.to_string());
        } else if args[i] != "--json" {
            return Err(format!("unknown argument: {}", args[i]));
        }
        i += 1;
    }
    Ok(PauseOptions {
        who,
        ttl_ms,
        reason,
    })
}

/// One `fno agents mail hold` call, bounded to 10s. Resolve beside this
/// runtime first so a deployed adapter cannot select a stale `fno` on PATH.
enum MailLeg {
    Ok(String),
    NoIdentity(String),
    Failed(String),
}

fn fno_cli_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.join("fno")))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("fno"))
}

fn mail_hold_binary() -> PathBuf {
    std::env::var_os("FNO_LOOPS_MAIL_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| fno_cli_binary())
}

fn run_mail_command(cmd: std::process::Command) -> MailLeg {
    match crate::bounded_cmd::output_with_timeout_result(cmd, 10) {
        Ok(output) => match output.status.code() {
            Some(0) => MailLeg::Ok(String::from_utf8_lossy(&output.stdout).trim().to_string()),
            Some(3) => {
                MailLeg::NoIdentity(String::from_utf8_lossy(&output.stderr).trim().to_string())
            }
            _ => {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                MailLeg::Failed(crate::evidence::truncate_chars(&detail, 200))
            }
        },
        Err(error) => MailLeg::Failed(crate::evidence::truncate_chars(&error.to_string(), 200)),
    }
}

fn run_mail_hold(extra: &[&str]) -> MailLeg {
    let mut cmd = std::process::Command::new(mail_hold_binary());
    cmd.args(["agents", "mail", "hold"]).args(extra);
    run_mail_command(cmd)
}

fn run_mail_hold_for(session_id: &str, extra: &[&str]) -> MailLeg {
    let binary = std::env::var_os("FNO_LOOPS_MAIL_BIN")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("fno-agents"));
    let mut cmd = std::process::Command::new(binary);
    cmd.args(["mail-hold", "--session", session_id]).args(extra);
    run_mail_command(cmd)
}

fn is_full_mail_session_id(session_id: &str) -> bool {
    crate::resume_wake::is_uuid_shaped(session_id) || session_id.starts_with("ses_")
}

fn mail_session_id_from_whoami(payload: &Value) -> Option<String> {
    let session = payload.get("session");
    let raw = session.and_then(|value| value.get("raw"));
    [
        payload.get("harness_session_id").and_then(Value::as_str),
        session
            .and_then(|value| value.get("harness_session_id"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("harness_session_id"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("codex_thread_id"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("codex_session_id"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("claude_session_uuid"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("claude_session_id"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("gemini_session_id"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("opencode_session_id"))
            .and_then(Value::as_str),
        raw.and_then(|value| value.get("cc_session_id"))
            .and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .find(|id| is_full_mail_session_id(id))
    .map(str::to_string)
}

fn current_mail_session_id() -> Result<String, String> {
    if let Ok(session_id) = std::env::var("FNO_SESSION_ID") {
        if is_full_mail_session_id(&session_id) {
            return Ok(session_id);
        }
    }
    let mut cmd = std::process::Command::new(fno_cli_binary());
    cmd.args(["whoami", "--json"]);
    let output = crate::bounded_cmd::output_with_timeout_result(cmd, 10)
        .map_err(|error| format!("cannot resolve current session: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(crate::evidence::truncate_chars(&detail, 200));
    }
    let payload: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("fno whoami returned bad JSON: {error}"))?;
    mail_session_id_from_whoami(&payload)
        .ok_or_else(|| "whoami did not return a full harness session id".to_string())
}

const NO_IDENTITY_DETAIL: &str = "no session identity - no mail to hold";

fn no_identity_detail(detail: String) -> String {
    if detail.is_empty() {
        NO_IDENTITY_DETAIL.to_string()
    } else {
        crate::evidence::truncate_chars(&detail, 200)
    }
}

fn remaining_hold_minutes(detail: &str) -> Option<u64> {
    let label = detail
        .rsplit_once("lifts in ")?
        .1
        .split_whitespace()
        .next()?;
    let label = label.strip_prefix('~').unwrap_or(label);
    label
        .strip_suffix('s')
        .and_then(|amount| amount.parse::<u64>().ok())
        .map(|seconds| seconds.div_ceil(60).max(1))
        .or_else(|| {
            label
                .strip_suffix('m')
                .and_then(|amount| amount.parse::<u64>().ok())
                .map(|minutes| minutes.max(1))
        })
}

fn mail_before_pause(
    ttl_ms: u64,
    session_id: &str,
    previously_armed_by: Option<&str>,
) -> (String, String, bool, Option<String>) {
    let requested_minutes = ttl_ms.div_ceil(60_000).max(1);
    let previously_armed = previously_armed_by == Some(session_id);
    match run_mail_hold(&["--status"]) {
        MailLeg::NoIdentity(detail) => ("skipped".into(), no_identity_detail(detail), false, None),
        MailLeg::Failed(detail) => ("failed".into(), detail, false, None),
        MailLeg::Ok(status) if status.contains(": no hold - ") => {
            arm_mail(requested_minutes, session_id)
        }
        MailLeg::Ok(status) if previously_armed => {
            let minutes = remaining_hold_minutes(&status)
                .map(|remaining| remaining.max(requested_minutes))
                .unwrap_or(requested_minutes);
            arm_mail(minutes, session_id)
        }
        MailLeg::Ok(status) => ("kept".into(), status, false, None),
    }
}

fn arm_mail(minutes: u64, session_id: &str) -> (String, String, bool, Option<String>) {
    match run_mail_hold(&["--for", &minutes.to_string()]) {
        MailLeg::Ok(detail) => ("armed".into(), detail, true, Some(session_id.to_string())),
        MailLeg::NoIdentity(detail) => ("skipped".into(), no_identity_detail(detail), false, None),
        MailLeg::Failed(detail) => ("failed".into(), detail, false, None),
    }
}

fn release_mail(owned_session: Option<&str>) -> (String, String) {
    let Some(session_id) = owned_session else {
        return ("left".into(), "not armed by pause-all".into());
    };
    match run_mail_hold_for(session_id, &["--off"]) {
        MailLeg::Ok(detail) => (
            "lifted".into(),
            if detail.is_empty() {
                format!("released hold for {session_id}")
            } else {
                detail
            },
        ),
        MailLeg::NoIdentity(detail) => ("skipped".into(), no_identity_detail(detail)),
        MailLeg::Failed(detail) => ("failed".into(), detail),
    }
}

fn loops_usage() {
    println!("usage: fno-agents loops paused|status [--json] | pause-all [--who W] [--ttl D|--ttl-ms N] [--reason R] [--json] | resume-all [--json] | table [--json|--markdown]\nFor a smaller halt: fno agents incident stop --session <full-id>|--territory <scope> --reason <why>");
}

/// Compute the `(action, output)` pair through breaker state and the mail
/// child. `Err(code)` is a bad-arguments or unknown-action early exit.
fn decide_loops(args: &[String]) -> Result<(String, Value), i32> {
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!("fno-agents loops: expected paused, pause-all, resume-all, status, or table");
        return Err(2);
    };
    let rest = &args[1..];
    let output = match action {
        // `paused` answers dispatch; `status` keeps the legacy JSON shape.
        "paused" => paused_json(),
        "status" => status_json(),
        "pause-all" => {
            let options = match parse_pause_options(rest) {
                Ok(options) => options,
                Err(error) => {
                    eprintln!("fno-agents loops pause-all: {error}");
                    return Err(2);
                }
            };
            let legacy = read_state();
            if legacy.is_paused() {
                return Ok((
                    action.to_string(),
                    json!({
                        "error": format!(
                            "legacy pause sentinel is active; run fno agents loops resume-all before pause-all: {}",
                            legacy.message()
                        )
                    }),
                ));
            }
            let home = crate::paths::AgentsHome::from_env();
            let path = crate::fleet_incident::fleet_stop_path(&home);
            let previous = match crate::fleet_incident::read_at(&path) {
                crate::fleet_incident::Verdict::Stopped(record)
                | crate::fleet_incident::Verdict::Clear(record) => record,
                crate::fleet_incident::Verdict::Unavailable(detail) => {
                    return Ok((action.to_string(), json!({"error": detail})))
                }
            };
            let previous_pause =
                previous.state == "stopped" && previous.origin.as_deref() == Some("pause-all");
            if previous.state == "stopped" && !previous_pause {
                return Ok((
                    action.to_string(),
                    json!({"error": "fleet incident owns the breaker; inspect with fno agents incident status and clear with fno agents incident clear"}),
                ));
            }
            if previous.state == "clear"
                && previous.origin.as_deref() == Some("pause-all")
                && previous.mail.as_deref() == Some("armed")
            {
                return Ok((
                    action.to_string(),
                    json!({
                        "error": "a pause-all mail release is pending; run fno agents loops resume-all to retry before pausing again"
                    }),
                ));
            }
            let ttl_defaulted = options.ttl_ms.is_none();
            let ttl_ms = options
                .ttl_ms
                .unwrap_or(crate::fleet_incident::DEFAULT_TARGET_TTL_MS);
            let expires_at = match crate::fleet_incident::expires_after(ttl_ms) {
                Ok(expires_at) => expires_at,
                Err(error) => {
                    eprintln!("fno-agents loops pause-all: {error}");
                    return Err(2);
                }
            };
            let reason = options
                .reason
                .unwrap_or_else(|| format!("pause-all by {}", options.who));
            let previous_mail_owner = (previous_pause && previous.mail.as_deref() == Some("armed"))
                .then(|| previous.mail_session_id.as_deref())
                .flatten();
            let (mail_state, mail_detail, armed_now, mail_session_id) =
                match current_mail_session_id() {
                    Ok(session_id) => mail_before_pause(ttl_ms, &session_id, previous_mail_owner),
                    Err(detail) => (
                        "skipped".into(),
                        crate::evidence::truncate_chars(&detail, 200),
                        false,
                        None,
                    ),
                };
            match crate::fleet_incident::write_transition_with_metadata(
                &path,
                "stopped",
                Some(&reason),
                Some(&options.who),
                vec!["spawns".into(), "loops".into()],
                crate::fleet_incident::RecordMetadata {
                    target: None,
                    expires_at: Some(expires_at),
                    origin: Some("pause-all".into()),
                    mail: Some(mail_state.clone()),
                    mail_session_id: mail_session_id.clone(),
                },
            ) {
                Ok(record) => json!({
                    "paused": true,
                    "state": "paused",
                    "who": record.changed_by,
                    "reason": record.reason,
                    "expires_at": record.expires_at,
                    "ttl_defaulted": ttl_defaulted,
                    "silenced": [
                        {"leg": "loops", "state": "paused"},
                        {"leg": "mail", "state": mail_state, "detail": mail_detail},
                    ],
                }),
                Err(error) => {
                    if armed_now {
                        if let Some(owner) = mail_session_id.as_deref() {
                            let _ = run_mail_hold_for(owner, &["--off"]);
                        }
                    }
                    json!({"error": error})
                }
            }
        }
        "resume-all" => {
            let home = crate::paths::AgentsHome::from_env();
            let path = crate::fleet_incident::fleet_stop_path(&home);
            let record = match crate::fleet_incident::read_at(&path) {
                crate::fleet_incident::Verdict::Stopped(record)
                | crate::fleet_incident::Verdict::Clear(record) => record,
                crate::fleet_incident::Verdict::Unavailable(detail) => {
                    return Ok((action.to_string(), json!({"error": detail})))
                }
            };
            let mut resumed = false;
            let mail_owner = (record.origin.as_deref() == Some("pause-all")
                && record.mail.as_deref() == Some("armed"))
            .then(|| record.mail_session_id.clone())
            .flatten();
            let mut mail_generation = record.generation;
            if record.state == "stopped" {
                if record.origin.as_deref() != Some("pause-all") {
                    return Ok((
                        action.to_string(),
                        json!({"error": "fleet incident owns the breaker; clear it with fno agents incident clear"}),
                    ));
                }
                let cleared = match crate::fleet_incident::write_transition_with_metadata(
                    &path,
                    "clear",
                    Some("resume-all"),
                    Some(&record.changed_by),
                    Vec::new(),
                    crate::fleet_incident::RecordMetadata {
                        origin: Some("pause-all".into()),
                        mail: record.mail.clone(),
                        mail_session_id: record.mail_session_id.clone(),
                        ..crate::fleet_incident::RecordMetadata::default()
                    },
                ) {
                    Ok(record) => record,
                    Err(error) => return Ok((action.to_string(), json!({"error": error}))),
                };
                mail_generation = cleared.generation;
                resumed = true;
            }
            let legacy_sentinel_removed = match std::fs::remove_file(sentinel_path()) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Ok((action.to_string(), json!({"error": error.to_string()}))),
            };
            let (mail_state, mut mail_detail) = release_mail(mail_owner.as_deref());
            if mail_state == "lifted" {
                if let Some(owner) = mail_owner.as_deref() {
                    if let Err(error) = crate::fleet_incident::mark_pause_mail_released(
                        &path,
                        mail_generation,
                        owner,
                    ) {
                        let note = format!("could not record mail release: {error}");
                        mail_detail = if mail_detail.is_empty() {
                            note
                        } else {
                            format!("{mail_detail}; {note}")
                        };
                    }
                }
            }
            json!({
                "resumed": resumed || legacy_sentinel_removed,
                "legacy_sentinel_removed": legacy_sentinel_removed,
                "state": "clear",
                "paused": false,
                "lifted": [
                    {"leg": "loops", "state": if resumed || legacy_sentinel_removed { "resumed" } else { "not paused" }},
                    {"leg": "mail", "state": mail_state, "detail": mail_detail},
                ],
            })
        }
        _ => {
            eprintln!("fno-agents loops: unknown action {action}");
            return Err(2);
        }
    };
    Ok((action.to_string(), output))
}

/// Test-friendly variant that returns `(exit_code, output)` without printing.
pub fn run_loops_capture(args: &[String]) -> (i32, Value) {
    match decide_loops(args) {
        Ok((_, output)) if output.get("error").is_some() => (1, output),
        Ok((_, output)) => (0, output),
        Err(code) => (code, json!({"error": "bad arguments"})),
    }
}

/// Entry point called from `bin/client.rs` direct dispatch. Prints to
/// stdout, returns exit code.
pub fn run_loops(args: &[String]) -> i32 {
    let json_out = args
        .get(1..)
        .unwrap_or(&[])
        .iter()
        .any(|arg| arg == "--json");
    if args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        loops_usage();
        return 0;
    }
    if args.first().map(String::as_str) == Some("table") {
        return run_loops_table(json_out, args.iter().any(|arg| arg == "--markdown"));
    }
    let (action, output) = match decide_loops(args) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    let action = action.as_str();
    if output.get("error").is_some() {
        eprintln!("fno-agents loops {action}: {}", output["error"]);
        return 1;
    }
    if json_out {
        println!("{output}");
    } else {
        match action {
            "pause-all" => {
                let who = output["who"].as_str().unwrap_or("operator");
                let reason = output["reason"].as_str().unwrap_or("pause-all");
                let expires = output["expires_at"].as_str().unwrap_or("");
                let loops_line = format!("loops: paused by {who} ({reason}), expires {expires}");
                let mail_leg = output["silenced"]
                    .as_array()
                    .and_then(|legs| legs.iter().find(|l| l["leg"] == "mail"));
                let mail_state = mail_leg
                    .and_then(|l| l["state"].as_str())
                    .unwrap_or("skipped");
                let mail_detail = mail_leg.and_then(|l| l["detail"].as_str()).unwrap_or("");
                let mail_word = if mail_state == "failed" {
                    "NOT held"
                } else {
                    mail_state
                };
                let mail_line = format!("mail: {mail_word} - {mail_detail}");
                if mail_state == "failed" {
                    println!("{mail_line}");
                    println!("{loops_line}");
                } else {
                    println!("{loops_line}");
                    println!("{mail_line}");
                }
            }
            "resume-all" => {
                let resumed = output["resumed"].as_bool().unwrap_or(false);
                println!("loops: {}", if resumed { "resumed" } else { "not paused" });
                if let Some(mail_leg) = output["lifted"]
                    .as_array()
                    .and_then(|legs| legs.iter().find(|l| l["leg"] == "mail"))
                {
                    let state = mail_leg["state"].as_str().unwrap_or("skipped");
                    let detail = mail_leg["detail"].as_str().unwrap_or("");
                    println!("mail: {state} - {detail}");
                }
            }
            "status" => {
                if output["state"] == "paused" {
                    let who = output["who"].as_str().unwrap_or("unknown");
                    let expiry = output["expires_at"]
                        .as_u64()
                        .and_then(|millis| {
                            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis as i64)
                        })
                        .map(|date| format!(", expires {}", date.to_rfc3339()))
                        .unwrap_or_default();
                    println!("paused by {who}{expiry}");
                } else if output["state"] == "expired" {
                    let who = output["who"].as_str().unwrap_or("unknown");
                    let expiry = output["expires_at"]
                        .as_u64()
                        .and_then(|millis| {
                            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis as i64)
                        })
                        .map(|date| date.to_rfc3339())
                        .unwrap_or_else(|| "unknown".into());
                    println!("expired (was paused by {who} until {expiry})");
                } else if output["state"] == "unavailable" || output["state"] == "corrupt" {
                    println!("pause state unavailable; failing closed: {}", output);
                } else {
                    println!("not paused");
                }
            }
            _ => println!("{output}"),
        }
    }
    0
}

/// The receipt event name one loop's rows land under. The fold that reads the
/// journals maps the heal receipt type onto the `heal` arm; every other arm
/// ticks `control_plane_tick`.
fn receipt_event(arm: &str) -> &'static str {
    if arm == "heal" {
        "pr_heal_tick"
    } else {
        "control_plane_tick"
    }
}

/// The daemon facts a CLI-side read holds: the supervisor lock's holder pid
/// is the liveness truth. `uptime_s: u64::MAX` says "never young" - the same
/// convention the arm_watch tick uses when it IS the daemon.
fn table_daemon_facts(home: &crate::paths::AgentsHome) -> crate::tick_ledger::DaemonFacts {
    match crate::paths::supervisor_lock_holder(home) {
        Some((pid, _))
            if !matches!(
                crate::claims::probe_pid(pid as i32),
                crate::claims::PidProbe::Absent
            ) =>
        {
            crate::tick_ledger::DaemonFacts::Up {
                uptime_s: u64::MAX,
                drifted: false,
            }
        }
        _ => crate::tick_ledger::DaemonFacts::Down,
    }
}

/// One `fno agents loops table` read: the fold `fno agents status` runs,
/// widened with the starved mark and the launchd label fold, printed as one
/// row per scheduled loop. Exit 1 only when a row reads STALE or FAIL:
/// `unarmed`, `starved`, `PAUSED` and `UNOBSERVED` exit 0, because none of
/// them is a loop that stopped while it was supposed to be running.
pub fn run_loops_table(json_out: bool, markdown: bool) -> i32 {
    let home = crate::paths::AgentsHome::from_env();
    let journals = crate::tick_ledger::journals(&home);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut rows = crate::tick_ledger::read_arms_starved(&journals, now);
    let trace = crate::tick_ledger::read_tick_trace_live(&journals, &rows, now);
    let daemon = table_daemon_facts(&home);
    crate::arm_repair::explain(&mut rows, &daemon, &trace);
    let launchd = crate::tick_ledger::launchd_fold_live();
    let any_red = rows.iter().any(|r| {
        !crate::tick_ledger::row_is_unarmed(r)
            && !r.starved
            && r.cause.as_deref() != Some("upstream_down")
            && (r.stale || r.failing)
    });
    if json_out {
        let launchd_json = match &launchd {
            Some(fold) => json!({
                "applicable": true,
                "labels": fold.labels,
                "dead": fold.dead,
            }),
            None => json!({"applicable": false, "labels": [], "dead": []}),
        };
        println!(
            "{}",
            json!({
                "arms": rows,
                "launchd": launchd_json,
                "any_red": any_red,
            })
        );
    } else if markdown {
        print!("{}", markdown_doc(&launchd));
        return 0;
    } else {
        println!("control-plane loops, one row per scheduled loop (regenerate the doc: fno agents loops table --markdown):");
        for row in &rows {
            println!("{}", row.line);
        }
        match &launchd {
            Some(fold) => {
                println!("launchd labels, loaded / last exit:");
                for f in &fold.labels {
                    let exit = match f.last_exit {
                        Some(e) => e.to_string(),
                        None => "-".to_string(),
                    };
                    let state = if f.loaded { "loaded" } else { "not loaded" };
                    println!("  {:<28} {:<10} exit {exit}", f.label, state);
                }
                if fold.dead.is_empty() {
                    println!("launchd: no dead labels");
                } else {
                    println!(
                        "launchd: DEAD labels: {}",
                        fold.dead
                            .iter()
                            .map(|f| format!("{} (exit {})", f.label, f.last_exit.unwrap_or(0)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            None => println!("launchd: not applicable on this host"),
        }
        println!("detail per loop: run the row's reader verb, or fno agents loops table --json");
    }
    if any_red {
        1
    } else {
        0
    }
}

/// The generated docs/loops.md body: the same KNOWN_ARMS table, static so a
/// regenerated file is byte-identical while the loops sit still. One row per
/// loop, then one paragraph per loop: where it starts (the trigger), where
/// it ends (the receipt event), and what to run when it looks wrong (the
/// reader verb).
fn markdown_doc(launchd: &Option<crate::tick_ledger::LaunchdFold>) -> String {
    let mut out = String::from(
        "# Scheduled loops\n\nThe control plane runs every scheduled loop below. One row names its scheduler, its arming key, its journal receipt, and the verb that reads it in detail. This file is generated. Regenerate it with `fno agents loops table --markdown` after changing `KNOWN_ARMS` in `crates/fno-agents/src/tick_ledger.rs`. When a row reads STALE or FAIL, `fno agents loops table` exits 1. Its plain form prints the same rows against the live journals.\n\n",
    );
    out.push_str("| loop | scheduler | interval (s) | armed by | ends with receipt | reads with |\n|---|---|---|---|---|---|\n");
    for spec in crate::tick_ledger::KNOWN_ARMS {
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {} | `{}` | {} |\n",
            spec.arm,
            spec.scheduler,
            spec.default_interval_s,
            spec.arm_key
                .map(|k| format!("`{k}`"))
                .unwrap_or_else(|| "always".to_string()),
            receipt_event(spec.arm),
            spec.reader
                .map(|r| format!("`{r}`"))
                .unwrap_or_else(|| "`fno agents loops table`".to_string()),
        ));
    }
    out.push_str("\nThe launchd labels the pr-watch installer and the autocorrect installer own, as the table reports them: `");
    out.push_str(&crate::tick_ledger::LAUNCHD_LABELS.join("`, `"));
    out.push_str(
        "`. A label the fold shows as `not loaded` cannot run. A nonzero last exit is one run that failed. `fno doctor` lists it under `launch_agents`.\n",
    );
    for spec in crate::tick_ledger::KNOWN_ARMS {
        let reader = spec
            .reader
            .map(|r| format!("`{r}`"))
            .unwrap_or_else(|| "`fno agents loops table`".to_string());
        out.push_str(&format!(
            "\n### {}\n\nStart: {} fires, every {}s. End: a `{}` receipt lands in the journal. If it looks wrong, run {}. {}.\n",
            spec.arm,
            spec.scheduler,
            spec.default_interval_s,
            receipt_event(spec.arm),
            reader,
            match spec.arm_key {
                Some("slot_cutover.enabled") => {
                    "If the row reads `unarmed`, add `[slot_cutover] enabled = true` to the daemon's `config.toml`".to_string()
                }
                Some(k) => format!("If the row reads `unarmed`, arm it with `fno config set {k} true`"),
                None => "If the row reads red, its `cause=` suffix names the next read".to_string(),
            }
        ));
    }
    if launchd.is_none() {
        out.push_str("\nThe launchd fold is not applicable on this host, and the table fabricates no alarm for it.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn the_slot_cutover_guide_does_not_use_the_curated_config_setter() {
        let markdown = markdown_doc(&None);
        assert!(
            markdown.contains("add `[slot_cutover] enabled = true` to the daemon's `config.toml`")
        );
        assert!(!markdown.contains("fno config set slot_cutover.enabled true"));
    }

    #[test]
    fn the_launchd_parse_folds_labels_and_the_dead_list() {
        let listing = "PID\tStatus\tLabel\n\
                       -\t0\tsh.fno.groom\n\
                       -\t78\tsh.fno.pr-watcher\n\
                       412\t0\tsh.fno.mux\n\
                       -\t127\tcom.user.autocorrect-watcher\n\
                       -\t-\tsh.fno.idle\n";
        let fold = crate::tick_ledger::parse_launchctl_list(listing);
        assert!(fold.applicable);
        let groom = fold
            .labels
            .iter()
            .find(|f| f.label == "sh.fno.groom")
            .unwrap();
        assert!(groom.loaded);
        assert_eq!(groom.last_exit, Some(0));
        let dead: Vec<&str> = fold.dead.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            dead,
            vec!["sh.fno.pr-watcher", "com.user.autocorrect-watcher"],
            "the autocorrect labels the sh.fno. prefix filter missed now count"
        );
        let watcher_row = serde_json::to_value(fold.dead[0].clone()).unwrap();
        assert_eq!(
            watcher_row["exit"], 78,
            "doctor reads entry['exit'], never last_exit"
        );
        assert!(watcher_row.get("last_exit").is_none());
        let unmeasured = fold
            .labels
            .iter()
            .find(|f| f.label == "sh.fno.sync-backlog")
            .unwrap();
        let row = serde_json::to_value(unmeasured).unwrap();
        assert!(
            row["exit"].is_null(),
            "a label launchctl gave no exit for serializes null"
        );
    }

    #[test]
    fn missing_sentinel_is_clear() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_state_at(&tmp.path().join("missing"), 1),
            PauseState::Clear
        );
    }

    #[test]
    fn valid_and_expired_sentinels_are_classified() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("loops-paused.json");
        fs::write(
            &path,
            r#"{"who":"op","paused_at":10,"expires_at":20,"extra":true}"#,
        )
        .unwrap();
        assert!(matches!(
            read_state_at(&path, 19),
            PauseState::Paused { .. }
        ));
        assert!(matches!(
            read_state_at(&path, 20),
            PauseState::Expired { .. }
        ));
    }

    #[test]
    fn malformed_or_incomplete_sentinels_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("loops-paused.json");
        fs::write(&path, "not json").unwrap();
        assert!(read_state_at(&path, 1).is_paused());
        fs::write(&path, r#"{"who":"op"}"#).unwrap();
        assert!(read_state_at(&path, 1).is_paused());
    }

    fn stopped_record(gen: u64) -> crate::fleet_incident::IncidentRecord {
        crate::fleet_incident::IncidentRecord {
            version: crate::fleet_incident::STATE_VERSION,
            state: "stopped".into(),
            generation: gen,
            changed_at: "2026-09-13T01:07:00Z".into(),
            changed_by: "op".into(),
            reason: "load 385".into(),
            holds: vec!["spawns".into(), "tests".into()],
            source: Some("file".into()),
            target: None,
            expires_at: None,
            origin: None,
            mail: None,
            mail_session_id: None,
        }
    }

    #[test]
    fn combine_blocks_on_manual_when_the_fleet_is_clear() {
        let paused = PauseState::Paused {
            who: "op".into(),
            paused_at: 1,
            expires_at: None,
            reason: None,
        };
        let clear_record = crate::fleet_incident::IncidentRecord {
            version: crate::fleet_incident::STATE_VERSION,
            state: "clear".into(),
            generation: 2,
            changed_at: String::new(),
            changed_by: String::new(),
            reason: String::new(),
            holds: Vec::new(),
            source: Some("file".into()),
            target: None,
            expires_at: None,
            origin: None,
            mail: None,
            mail_session_id: None,
        };
        let combined = combine(&paused, crate::fleet_incident::Verdict::Clear(clear_record));
        assert_eq!(
            combined,
            DispatchPause::Manual {
                state: "paused".into(),
                detail: "loops paused by op".into(),
            }
        );
        assert_eq!(combined.skip_reason(), "loops_paused");
    }

    #[test]
    fn combine_prefers_the_manual_display_when_both_hold() {
        let paused = PauseState::Paused {
            who: "op".into(),
            paused_at: 1,
            expires_at: None,
            reason: None,
        };
        let combined = combine(
            &paused,
            crate::fleet_incident::Verdict::Stopped(stopped_record(5)),
        );
        assert!(matches!(combined, DispatchPause::Manual { .. }));
        assert_eq!(combined.skip_reason(), "loops_paused");
    }

    #[test]
    fn pause_message_names_a_fleet_incident() {
        let mut record = stopped_record(7);
        record.reason = "load".into();
        let message = pause_message_for(
            &PauseState::Clear,
            crate::fleet_incident::Verdict::Stopped(record),
        )
        .expect("a fleet stop pauses the stop hook");
        assert!(message.contains("generation 7"), "{message}");
        assert!(message.contains("load"), "{message}");
    }

    #[test]
    fn pause_message_is_none_when_clear_and_names_an_unreadable_incident() {
        let mut clear = stopped_record(3);
        clear.state = "clear".into();
        assert_eq!(
            pause_message_for(
                &PauseState::Clear,
                crate::fleet_incident::Verdict::Clear(clear)
            ),
            None
        );
        let message = pause_message_for(
            &PauseState::Clear,
            crate::fleet_incident::Verdict::Unavailable("unparseable: x".into()),
        )
        .expect("an unreadable incident pauses");
        assert!(message.contains("unreadable"), "{message}");
    }

    #[test]
    fn combine_fails_closed_on_an_unavailable_fleet_record() {
        let combined = combine(
            &PauseState::Clear,
            crate::fleet_incident::Verdict::Unavailable("unreadable: boom".into()),
        );
        assert!(combined.is_paused());
        assert_eq!(combined.skip_reason(), "fleet_stop_unavailable");
        assert!(combined.detail().contains("boom"));
    }

    #[test]
    fn fleet_incident_verdict_names_the_fleet_stop() {
        let combined = combine(
            &PauseState::Clear,
            crate::fleet_incident::Verdict::Stopped(stopped_record(7)),
        );
        assert_eq!(
            combined,
            DispatchPause::FleetIncident {
                generation: 7,
                reason: "load 385".into(),
                holds: vec!["spawns".into(), "tests".into()],
            }
        );
        assert_eq!(combined.skip_reason(), "fleet_stop");
    }

    #[test]
    fn pause_all_breaker_keeps_the_legacy_loop_skip_token() {
        let mut record = stopped_record(8);
        record.holds = vec!["spawns".into(), "loops".into()];
        record.origin = Some("pause-all".into());
        record.reason = "pause-all by operator".into();
        let combined = combine(
            &PauseState::Clear,
            crate::fleet_incident::Verdict::Stopped(record),
        );

        assert_eq!(combined.skip_reason(), "loops_paused");
        assert_eq!(
            combined,
            DispatchPause::Manual {
                state: "paused".into(),
                detail: "loops paused by op: pause-all by operator".into(),
            }
        );
    }

    #[test]
    fn machine_loop_status_keeps_the_epoch_millisecond_shape() {
        let mut record = stopped_record(9);
        record.changed_at = "2026-09-13T01:07:00Z".into();
        record.expires_at = Some("2026-09-13T02:07:00Z".into());
        let status = machine_status(&record, "paused");

        assert_eq!(status["paused"], true);
        assert_eq!(status["state"], "paused");
        assert_eq!(status["who"], "op");
        assert_eq!(status["paused_at"], 1_789_261_620_000_u64);
        assert_eq!(status["expires_at"], 1_789_265_220_000_u64);
    }

    #[test]
    fn mail_owner_uses_the_full_harness_session_id() {
        let id = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        assert_eq!(
            mail_session_id_from_whoami(&json!({"harness_session_id": id})),
            Some(id.to_string())
        );
        assert_eq!(
            mail_session_id_from_whoami(&json!({
                "session": {"raw": {"codex_thread_id": "ses_MixedCase"}}
            })),
            Some("ses_MixedCase".to_string())
        );
        assert_eq!(
            mail_session_id_from_whoami(&json!({"fno_id":"short"})),
            None
        );
    }

    /// The done probe: end-to-end through the env-resolved readers. Pins HOME
    /// and FNO_AGENTS_HOME so neither the sentinel nor the fleet record
    /// touches real state, and holds the shared env lock so a parallel
    /// env-pinned test cannot observe the mutation.
    #[test]
    fn loops_paused_reports_fleet_incident() {
        let guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let saved_home = std::env::var_os("HOME");
        let saved_agents = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("HOME", tmp.path());
        let agents = tmp.path().join("agents-home");
        std::fs::create_dir_all(&agents).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", &agents);
        std::fs::write(
            crate::fleet_incident::fleet_stop_path(&crate::paths::AgentsHome::at(&agents)),
            serde_json::to_string(&stopped_record(7)).unwrap(),
        )
        .unwrap();

        let value = paused_json();
        let mut alias = stopped_record(8);
        alias.holds = vec!["spawns".into(), "loops".into()];
        alias.origin = Some("pause-all".into());
        std::fs::write(
            crate::fleet_incident::fleet_stop_path(&crate::paths::AgentsHome::at(&agents)),
            serde_json::to_string(&alias).unwrap(),
        )
        .unwrap();
        let alias_value = paused_json();

        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match saved_agents {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        drop(guard);

        assert_eq!(value["paused"], true);
        assert_eq!(value["source"], "fleet_incident");
        assert_eq!(value["generation"], 7);
        assert_eq!(value["state"], "fleet_stop");
        assert_eq!(alias_value["paused"], true);
        assert_eq!(alias_value["source"], "fleet_incident");
        assert_eq!(alias_value["generation"], 8);
    }
}
