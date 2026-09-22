//! Global pause-all sentinel owned by the Rust runtime.
//!
//! The sentinel is deliberately global to the user account: every repository
//! must observe the same operator pause. Read failures are fail-closed because
//! a broken safety switch must not silently resume dispatch.

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
    Manual { state: String, detail: String },
    FleetIncident { generation: u64, reason: String },
    FleetIncidentUnavailable { detail: String },
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
            Self::FleetIncident { generation, reason } => {
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
        crate::fleet_incident::Verdict::Stopped(r) => DispatchPause::FleetIncident {
            generation: r.generation,
            reason: r.reason,
        },
        crate::fleet_incident::Verdict::Unavailable(d) => {
            DispatchPause::FleetIncidentUnavailable { detail: d }
        }
    }
}

/// The effective dispatch pause for this machine.
pub fn dispatch_pause() -> DispatchPause {
    combine(&read_state(), crate::fleet_incident::verdict())
}

/// The combined `loops paused --json` answer: `paused`, `source`, `state`,
/// and the incident generation/reason or unavailable detail when present.
/// The Python adapter reads only `paused`, so the added fields stay additive.
fn paused_json() -> Value {
    // One read feeds both the verdict and the manual fields: a second read
    // could straddle a resume and print paused:false for a sentinel this
    // same call just saw paused.
    let manual = read_state();
    match combine(&manual, crate::fleet_incident::verdict()) {
        DispatchPause::Clear => json!({"paused": false, "source": "none", "state": "clear"}),
        DispatchPause::Manual { .. } => {
            let mut v = manual.json();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("source".into(), json!("manual"));
            }
            v
        }
        DispatchPause::FleetIncident { generation, reason } => json!({
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

/// The stop hook's hold read for a session in `cwd`: the manual sentinel,
/// the fleet incident, or a cargo build of `cwd` waiting on build admission.
/// A held worker whose stop hook missed any of them would count every fire
/// as NoProgress and die on a hold it was told to obey.
pub fn pause_message(cwd: &Path) -> Option<String> {
    pause_message_for(&read_state(), crate::fleet_incident::verdict())
        .or_else(|| crate::test_run::build_hold_message(cwd))
}

fn pause_message_for(
    manual: &PauseState,
    incident: crate::fleet_incident::Verdict,
) -> Option<String> {
    let pause = combine(manual, incident);
    pause.is_paused().then(|| pause.detail())
}

fn write_pause(who: &str, ttl_ms: Option<u64>, reason: Option<&str>) -> Result<PauseState, String> {
    let path = sentinel_path();
    let paused_at = now_ms();
    let expires_at = ttl_ms.map(|ttl| paused_at.saturating_add(ttl));
    let body =
        json!({"who": who, "paused_at": paused_at, "expires_at": expires_at, "reason": reason});
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| error.to_string())?;
    let tmp = path.with_file_name(format!("{SENTINEL_NAME}.tmp"));
    std::fs::write(
        &tmp,
        serde_json::to_vec(&body).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if let Err(error) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    Ok(PauseState::Paused {
        who: who.to_string(),
        paused_at,
        expires_at,
        reason: reason.map(str::to_string),
    })
}

fn resume_pause() -> Result<bool, String> {
    match std::fs::remove_file(sentinel_path()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

struct PauseOptions {
    who: String,
    ttl_ms: Option<u64>,
    reason: Option<String>,
}

/// Parse a `--ttl` duration like `30m`, `2h`, `1d`, `45s` into milliseconds.
/// Ports `_parse_ttl_ms` from `cli/src/fno/loops.py`.
fn parse_ttl(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let mut chars = trimmed.chars();
    let Some(unit) = chars.next_back() else {
        return Err(format!("invalid --ttl: {value:?}"));
    };
    let digits = chars.as_str().trim_end();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid --ttl: {value:?}"));
    }
    let mult: u64 = match unit.to_ascii_lowercase() {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return Err(format!("invalid --ttl: {value:?}")),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("invalid --ttl: {value:?}"))?;
    if n == 0 {
        return Err(format!("TTL must be > 0: {value:?}"));
    }
    n.checked_mul(mult)
        .and_then(|ms| ms.checked_mul(1000))
        .ok_or_else(|| format!("--ttl too large: {value:?}"))
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
            ttl_ms = Some(
                next.parse::<u64>()
                    .map_err(|_| format!("invalid --ttl-ms: {next}"))?,
            );
            i += 1;
        } else if let Some(inline) = args[i].strip_prefix("--ttl-ms=") {
            ttl_ms = Some(
                inline
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --ttl-ms: {inline}"))?,
            );
        } else if args[i] == "--ttl" {
            let Some(next) = args.get(i + 1) else {
                return Err("--ttl requires a value".to_string());
            };
            ttl_ms = Some(parse_ttl(next)?);
            i += 1;
        } else if let Some(inline) = args[i].strip_prefix("--ttl=") {
            ttl_ms = Some(parse_ttl(inline)?);
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

/// One `fno agents mail hold` call, bounded to 10s. The binary comes from
/// `FNO_LOOPS_MAIL_BIN` (default `fno`) so a test can point it at a stub
/// without editing PATH, which parallel tests share.
enum MailLeg {
    Ok(String),
    NoIdentity,
    Failed(String),
}

fn run_mail_hold(extra: &[&str]) -> MailLeg {
    let binary = std::env::var("FNO_LOOPS_MAIL_BIN").unwrap_or_else(|_| "fno".to_string());
    let mut cmd = std::process::Command::new(&binary);
    cmd.args(["agents", "mail", "hold"]).args(extra);
    match crate::bounded_cmd::output_with_timeout_result(cmd, 10) {
        Ok(output) => match output.status.code() {
            Some(0) => MailLeg::Ok(String::from_utf8_lossy(&output.stdout).trim().to_string()),
            Some(3) => MailLeg::NoIdentity,
            _ => {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                MailLeg::Failed(crate::evidence::truncate_chars(&detail, 200))
            }
        },
        Err(error) => MailLeg::Failed(crate::evidence::truncate_chars(&error.to_string(), 200)),
    }
}

const NO_IDENTITY_DETAIL: &str = "no session identity - no mail to hold";

/// Compute the `(action, output)` pair with no I/O beyond the sentinel file
/// and the `fno agents mail hold` child. `Err(code)` is a bad-arguments or
/// unknown-action early exit that never reaches the sentinel.
fn decide_loops(args: &[String]) -> Result<(String, Value), i32> {
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!("fno-agents loops: expected paused, pause-all, resume-all, status, or table");
        return Err(2);
    };
    let rest = &args[1..];
    let output = match action {
        // `paused` answers the combined dispatch verdict (manual OR
        // fleet); `status` stays about the manual sentinel only.
        "paused" => paused_json(),
        "status" => read_state().json(),
        "pause-all" => {
            let options = match parse_pause_options(rest) {
                Ok(options) => options,
                Err(error) => {
                    eprintln!("fno-agents loops pause-all: {error}");
                    return Err(2);
                }
            };
            match write_pause(&options.who, options.ttl_ms, options.reason.as_deref()) {
                Ok(state) => {
                    let (mail_state, mail_detail) = match options.ttl_ms {
                        Some(ttl_ms) => {
                            let minutes = ttl_ms.div_ceil(60_000).max(1);
                            match run_mail_hold(&["--for", &minutes.to_string()]) {
                                MailLeg::Ok(detail) => ("held", detail),
                                MailLeg::NoIdentity => ("skipped", NO_IDENTITY_DETAIL.to_string()),
                                MailLeg::Failed(detail) => ("failed", detail),
                            }
                        }
                        None => (
                            "skipped",
                            "pass --ttl so the mail hold lifts by itself".to_string(),
                        ),
                    };
                    json!({
                        "paused": true,
                        "state": "paused",
                        "who": state.who(),
                        "reason": options.reason,
                        "expires_at": state.json()["expires_at"],
                        "silenced": [
                            {"leg": "loops", "state": "paused"},
                            {"leg": "mail", "state": mail_state, "detail": mail_detail},
                        ],
                    })
                }
                Err(error) => json!({"error": error}),
            }
        }
        "resume-all" => match resume_pause() {
            Ok(resumed) => {
                let (mail_state, mail_detail) = match run_mail_hold(&["--off"]) {
                    MailLeg::Ok(detail) => ("lifted", detail),
                    MailLeg::NoIdentity => ("skipped", NO_IDENTITY_DETAIL.to_string()),
                    MailLeg::Failed(detail) => ("failed", detail),
                };
                json!({
                    "resumed": resumed,
                    "state": "clear",
                    "paused": false,
                    "lifted": [
                        {"leg": "loops", "state": "resumed"},
                        {"leg": "mail", "state": mail_state, "detail": mail_detail},
                    ],
                })
            }
            Err(error) => json!({"error": error}),
        },
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
                let reason = output["reason"].as_str();
                let expires = output["expires_at"].as_u64();
                let loops_line = match (reason, expires) {
                    (Some(r), Some(e)) => format!("loops: paused by {who} ({r}), expires {e}"),
                    (Some(r), None) => format!("loops: paused by {who} ({r})"),
                    (None, Some(e)) => format!("loops: paused by {who}, expires {e}"),
                    (None, None) => format!("loops: paused by {who}"),
                };
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = crate::paths::AgentsHome::from_env();
    let journals = crate::tick_ledger::journals(&home);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut rows = crate::tick_ledger::read_arms(&journals, now);
    let threshold = crate::agents_config::notify_arm_starved_after_s(&cwd);
    crate::tick_ledger::mark_starved(&journals, &mut rows, now, threshold);
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
            source: Some("file".into()),
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
            source: Some("file".into()),
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
            }
        );
        assert_eq!(combined.skip_reason(), "fleet_stop");
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
    }
}
