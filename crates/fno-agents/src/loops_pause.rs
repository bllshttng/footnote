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
    },
    Expired {
        who: String,
        paused_at: u64,
        expires_at: u64,
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
            } => json!({
                "paused": true,
                "state": "paused",
                "who": who,
                "paused_at": paused_at,
                "expires_at": expires_at,
            }),
            Self::Expired {
                who,
                paused_at,
                expires_at,
            } => json!({
                "paused": false,
                "state": "expired",
                "who": who,
                "paused_at": paused_at,
                "expires_at": expires_at,
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
            Self::Paused { who, .. } => format!("loops paused by {who}"),
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
    if let Some(expires_at) = expires_at {
        if expires_at <= now {
            return PauseState::Expired {
                who: who.to_string(),
                paused_at,
                expires_at,
            };
        }
    }
    PauseState::Paused {
        who: who.to_string(),
        paused_at,
        expires_at,
    }
}

fn corrupt(path: &Path, error: impl Into<String>) -> PauseState {
    PauseState::Corrupt {
        path: path.to_path_buf(),
        error: error.into(),
    }
}

/// The effective dispatch verdict: manual sentinel OR fleet incident. The
/// stop hook's `pause_message()` deliberately stays manual-only.
pub fn is_paused() -> bool {
    dispatch_pause().is_paused()
}

pub fn pause_message() -> Option<String> {
    let state = read_state();
    state.is_paused().then(|| state.message())
}

fn write_pause(who: &str, ttl_ms: Option<u64>) -> Result<PauseState, String> {
    let path = sentinel_path();
    let paused_at = now_ms();
    let expires_at = ttl_ms.map(|ttl| paused_at.saturating_add(ttl));
    let body = json!({"who": who, "paused_at": paused_at, "expires_at": expires_at});
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
    })
}

fn resume_pause() -> Result<bool, String> {
    match std::fs::remove_file(sentinel_path()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn parse_pause_options(args: &[String]) -> Result<(String, Option<u64>), String> {
    let mut who = "operator".to_string();
    let mut ttl_ms = None;
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
        } else if args[i] != "--json" {
            return Err(format!("unknown argument: {}", args[i]));
        }
        i += 1;
    }
    Ok((who, ttl_ms))
}

pub fn run_loops(args: &[String]) -> i32 {
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!("fno-agents loops: expected paused, pause-all, resume-all, or status");
        return 2;
    };
    let rest = &args[1..];
    let json_out = rest.iter().any(|arg| arg == "--json");
    let output = match action {
        // `paused` answers the combined dispatch verdict (manual OR
        // fleet); `status` stays about the manual sentinel only.
        "paused" => paused_json(),
        "status" => read_state().json(),
        "pause-all" => {
            let (who, ttl) = match parse_pause_options(rest) {
                Ok(options) => options,
                Err(error) => {
                    eprintln!("fno-agents loops pause-all: {error}");
                    return 2;
                }
            };
            match write_pause(&who, ttl) {
                Ok(state) => {
                    json!({"paused": true, "state": "paused", "who": state.who(), "expires_at": state.json()["expires_at"]})
                }
                Err(error) => json!({"error": error}),
            }
        }
        "resume-all" => match resume_pause() {
            Ok(resumed) => json!({"resumed": resumed, "state": "clear", "paused": false}),
            Err(error) => json!({"error": error}),
        },
        _ => {
            eprintln!("fno-agents loops: unknown action {action}");
            return 2;
        }
    };
    if output.get("error").is_some() {
        eprintln!("fno-agents loops {action}: {}", output["error"]);
        return 1;
    }
    if json_out {
        println!("{output}");
    } else {
        match action {
            "pause-all" => println!("paused by {}", output["who"].as_str().unwrap_or("operator")),
            "resume-all" => println!(
                "{}",
                if output["resumed"].as_bool().unwrap_or(false) {
                    "resumed"
                } else {
                    "not paused"
                }
            ),
            _ => println!("{output}"),
        }
    }
    0
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
        };
        let combined = combine(
            &paused,
            crate::fleet_incident::Verdict::Stopped(stopped_record(5)),
        );
        assert!(matches!(combined, DispatchPause::Manual { .. }));
        assert_eq!(combined.skip_reason(), "loops_paused");
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
