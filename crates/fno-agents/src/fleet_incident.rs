//! Durable fleet incident state : one machine-wide circuit breaker.
//!
//! `fleet-stop.json` in the agents home is the authority. `stop` writes a
//! positive `stopped` record; `clear` writes a positive `clear` record; both
//! increment a monotonic `generation`, so "nothing happened" can never wear
//! the receipt of "incident over". Admission readers (spawn gates, the
//! active-backlog daemon, test runs) consult [`verdict`] BEFORE any bypass
//! or capacity branch, so a daemon or worker that starts mid-incident is
//! gated by the file, not by whether it heard an announcement.
//!
//! Mail stays deliberately ungated: the announcement channel must work while
//! the fleet is stopped, or the operator cannot explain the incident or the
//! recovery.
//!
//! The reach is typed: an armed record names the scopes it holds
//! ([`SCOPES`]), and every door reads the breaker through [`verdict_for`],
//! so the readout and the refusals can never disagree. `spawns` holds
//! worker spawns and the backlog dispatcher (dispatch is automatic
//! spawning, so one word covers both). `tests` holds `fno doctor test`
//! suites and the cargo build/run admission doors; the doors wait and
//! never fail, so a running cargo pauses at its next compile or test
//! binary and a running suite finishes. `merges` holds the one merge
//! primitive. `loops` holds live target and king loops at their next turn end.
//!
//! A file that exists but cannot be read or parsed is
//! [`Verdict::Unavailable`], never clear (AC1-EDGE): an unreadable breaker
//! must not read as "no incident". Absence is a real answer (`source:
//! "default"`, a pre-feature machine has no incident). Writers refuse to
//! replace an unreadable record - overwriting state we could not read would
//! destroy the evidence; the operator removes the file by hand to start a
//! fresh generation.
//!
//! Binary verb `fno-agents fleet-incident stop|clear|status|check`, matched
//! in `bin/client.rs` next to `test-run` (direct dispatch, no daemon RPC).
//! The public surface is the thin Python adapter `fno agents incident`,
//! which relays exit/stdout/stderr and decides nothing.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// The only schema version this reader understands.
pub const STATE_VERSION: u32 = 1;

/// The scopes an armed breaker can hold, in readout order. The words the
/// `--hold` flag and the typed reach readout speak.
pub const SCOPES: &[&str] = &["spawns", "tests", "merges", "loops"];

/// The old loop hook read `spawns`, so pre-scope stops also held live loops.
/// Their reach was spawns, tests, and loops; merges proceeded.
pub const LEGACY_HOLDS: &[&str] = &["spawns", "tests", "loops"];

/// Exit codes for the `check` verdict verb, distinct from every dispatch and
/// gate code in use - see the allocation table in
/// `cli/src/fno/agents/spawn_gate.py` (gate band 75-77, 79, 82-84), plus the
/// convention codes 2, 13-15, 18-19, 124, 127.
pub const EXIT_CHECK_STOPPED: i32 = 90;
pub const EXIT_CHECK_UNAVAILABLE: i32 = 91;

/// `origin` of the machine arm's tests-first hold.
pub const MACHINE_ORIGIN: &str = "machine_watch";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncidentRecord {
    pub version: u32,
    /// `stopped` | `clear`.
    pub state: String,
    pub generation: u64,
    /// RFC 3339 UTC.
    pub changed_at: String,
    pub changed_by: String,
    pub reason: String,
    /// Which scopes a `stopped` record holds: a subset of [`SCOPES`]. Absent
    /// or empty on disk means the record predates scopes and keeps the reach
    /// scopes had then - [`LEGACY_HOLDS`], never merges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<String>,
    /// Why this answer exists at all: `file` (a record was read) or
    /// `default` (no file; a pre-feature machine has no incident). A
    /// deliberate positive marker on the absent case, so a reader that
    /// forgets to distinguish them still names which one it saw.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
    /// The session or crown territory this record targets. Absent means the
    /// machine-wide breaker record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// RFC 3339 UTC expiry. An invalid value makes the record unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The compatibility door that wrote this record, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Mail-hold ownership reported by the pause-all alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<String>,
    /// Full session id whose mail policy the pause-all alias armed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_session_id: Option<String>,
}

#[derive(Default)]
pub(crate) struct RecordMetadata {
    pub(crate) target: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) mail: Option<String>,
    pub(crate) mail_session_id: Option<String>,
}

/// What an admission reader is told. `Unavailable` carries the read error.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Clear(IncidentRecord),
    Stopped(IncidentRecord),
    Unavailable(String),
}

/// The live identity and graph context for one loop asking whether it is held.
pub struct Subject<'a> {
    pub session_ids: Vec<String>,
    pub node: Option<String>,
    pub territory: Option<String>,
    pub cwd: &'a Path,
}

impl Verdict {
    /// True only for a genuine, readable stop. The one question every gate
    /// asks, kept in one place so no gate re-derives it.
    pub fn is_stopped(&self) -> bool {
        matches!(self, Verdict::Stopped(_))
    }

    /// `(generation, reason)` when a record backs the verdict.
    pub fn generation_reason(&self) -> (Option<u64>, Option<&str>) {
        match self {
            Verdict::Stopped(r) | Verdict::Clear(r) => {
                (Some(r.generation), Some(r.reason.as_str()))
            }
            Verdict::Unavailable(_) => (None, None),
        }
    }

    /// How one scope's door reads this verdict: true when the door must
    /// refuse or wait. A stop that does not hold `scope` reads as no
    /// incident to that door; an unreadable record holds every scope, so no
    /// door ever reads a breaker it cannot parse as "no incident".
    pub fn holds(&self, scope: &str) -> bool {
        match self {
            Verdict::Stopped(r) => r.holds_scope(scope),
            Verdict::Clear(_) => false,
            Verdict::Unavailable(_) => true,
        }
    }
}

impl IncidentRecord {
    /// The scopes this record holds, resolving the pre-scope default.
    pub fn held_scopes(&self) -> Vec<String> {
        if self.holds.is_empty() {
            LEGACY_HOLDS.iter().map(|s| s.to_string()).collect()
        } else {
            self.holds.clone()
        }
    }

    pub(crate) fn holds_scope(&self, scope: &str) -> bool {
        self.held_scopes().iter().any(|s| s == scope)
    }

    /// The scopes this record leaves alone, in [`SCOPES`] order. A `clear`
    /// record holds nothing, so everything proceeds.
    pub fn admits_scopes(&self) -> Vec<String> {
        if self.state != "stopped" {
            return SCOPES.iter().map(|s| s.to_string()).collect();
        }
        let held = self.held_scopes();
        SCOPES
            .iter()
            .filter(|s| !held.contains(&s.to_string()))
            .map(|s| s.to_string())
            .collect()
    }
}

/// The verdict as one scope's admission door sees it: a stop that does not
/// hold `scope` is clear to that door. The record rides unchanged for
/// generation provenance; its `state` field is the file's, not the door's.
/// Every door - the spawn gates, the backlog dispatcher, the test and cargo
/// doors, the merge primitive - reads through here, so the typed reach
/// readout and the refusals can never disagree.
pub fn verdict_for(scope: &str) -> Verdict {
    scope_verdict(verdict(), scope)
}

fn scope_verdict(verdict: Verdict, scope: &str) -> Verdict {
    match verdict {
        Verdict::Stopped(r) if !r.holds_scope(scope) => Verdict::Clear(r),
        v => v,
    }
}

fn target_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.strip_suffix(".json")?;
    let (kind, value) = if let Some(value) = name.strip_prefix("session-") {
        ("session", value.to_string())
    } else if let Some(value) = name.strip_prefix("territory-") {
        ("territory", value.replace('+', ","))
    } else {
        return None;
    };
    safe_target_value(&value).then(|| format!("{kind}:{value}"))
}

/// Read all per-subject records. A missing directory means no targeted holds;
/// an unreadable directory is retained as one fail-closed answer.
pub fn targeted_records(home: &crate::paths::AgentsHome) -> Vec<(PathBuf, Verdict)> {
    let dir = targets_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            return vec![(
                dir.clone(),
                Verdict::Unavailable(format!("unreadable target directory: {error}")),
            )]
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return vec![(
                    dir.clone(),
                    Verdict::Unavailable(format!("unreadable target directory: {error}")),
                )]
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                return vec![(
                    dir.clone(),
                    Verdict::Unavailable(format!("unreadable target entry: {error}")),
                )]
            }
        };
        if file_type.is_file() && entry.path().extension().is_some_and(|ext| ext == "json") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let verdict = read_at(&path);
            (path, verdict)
        })
        .collect()
}

struct TargetStatus {
    target: Option<String>,
    state: &'static str,
    path: PathBuf,
    record: Option<IncidentRecord>,
    detail: Option<String>,
}

impl TargetStatus {
    fn from_record(path: PathBuf, verdict: Verdict) -> Self {
        let filename_target = target_from_path(&path);
        match verdict {
            Verdict::Stopped(record) | Verdict::Clear(record)
                if record.target.as_deref() != filename_target.as_deref() =>
            {
                Self {
                    target: filename_target,
                    state: "unavailable",
                    path: path.clone(),
                    record: None,
                    detail: Some(format!(
                        "target record {} does not match its filename",
                        path.display()
                    )),
                }
            }
            Verdict::Stopped(record) => Self {
                target: filename_target,
                state: "stopped",
                path,
                record: Some(record),
                detail: None,
            },
            Verdict::Clear(record) => Self {
                target: filename_target,
                state: if record.state == "stopped" {
                    "expired"
                } else {
                    "clear"
                },
                path,
                record: Some(record),
                detail: None,
            },
            Verdict::Unavailable(detail) => Self {
                target: filename_target,
                state: "unavailable",
                path,
                record: None,
                detail: Some(detail),
            },
        }
    }

    fn json(&self) -> serde_json::Value {
        let record = self.record.as_ref();
        serde_json::json!({
            "target": self.target.clone(),
            "state": self.state,
            "holds": record
                .filter(|record| record.state == "stopped")
                .map(IncidentRecord::held_scopes)
                .unwrap_or_default(),
            "expires_at": record.and_then(|record| record.expires_at.clone()),
            "changed_by": record.map(|record| record.changed_by.clone()),
            "reason": record.map(|record| record.reason.clone()),
            "generation": record.map(|record| record.generation),
            "path": self.path.display().to_string(),
            "detail": self.detail.clone(),
        })
    }

    fn print_human(&self) {
        match self.state {
            "stopped" | "expired" => {
                let record = self.record.as_ref().expect("readable target record");
                println!(
                    "target {}: {} (generation {})",
                    self.target.as_deref().unwrap_or("unknown"),
                    self.state,
                    record.generation
                );
                println!("holds: {}", record.held_scopes().join(", "));
                if let Some(expires_at) = &record.expires_at {
                    println!("expires {expires_at}");
                }
                if !record.reason.is_empty() {
                    println!("reason: {}", record.reason);
                }
            }
            "unavailable" => eprintln!(
                "target {} unavailable: {}",
                self.target.as_deref().unwrap_or("unknown"),
                self.detail.as_deref().unwrap_or("unknown read error")
            ),
            _ => {}
        }
    }
}

fn targeted_statuses(home: &crate::paths::AgentsHome) -> Vec<TargetStatus> {
    targeted_records(home)
        .into_iter()
        .map(|(path, verdict)| TargetStatus::from_record(path, verdict))
        .collect()
}

fn machine_status_json(
    record: &IncidentRecord,
    state: &str,
    targets: &[TargetStatus],
) -> serde_json::Value {
    let mut body = serde_json::to_value(record).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = body.as_object_mut() {
        let (holds, admits) = if state == "clear" {
            (
                Vec::new(),
                SCOPES.iter().map(|scope| scope.to_string()).collect(),
            )
        } else {
            reach(record)
        };
        obj.insert("state".into(), serde_json::json!(state));
        obj.insert("holds".into(), serde_json::json!(holds));
        obj.insert("admits".into(), serde_json::json!(admits));
        obj.insert(
            "targets".into(),
            serde_json::json!(targets.iter().map(TargetStatus::json).collect::<Vec<_>>()),
        );
    }
    body
}

fn print_machine_status(record: &IncidentRecord, state: &str, targets: &[TargetStatus]) {
    println!(
        "fleet incident: {} (generation {})",
        state, record.generation
    );
    if !record.reason.is_empty() {
        println!("reason: {}", record.reason);
    }
    if state == "stopped" {
        let (holds, admits) = reach(record);
        println!("holds: {}; admits: {}", holds.join(", "), admits.join(", "));
    }
    if let Some(expires_at) = &record.expires_at {
        println!("expires {expires_at}");
    }
    if !record.changed_by.is_empty() {
        println!("changed by {} at {}", record.changed_by, record.changed_at);
    }
    for target in targets {
        target.print_human();
    }
}

fn target_matches(target: &str, subject: &Subject<'_>) -> Result<bool, String> {
    let (kind, value) = target
        .split_once(':')
        .ok_or_else(|| format!("invalid target {target:?}"))?;
    match kind {
        "session" => Ok(subject.session_ids.iter().any(|id| id == value)),
        "territory" => {
            if subject
                .territory
                .as_deref()
                .is_some_and(|scope| crate::territory::canonical_scope(scope) == value)
            {
                return Ok(true);
            }
            let Some(node) = subject.node.as_deref() else {
                return Ok(false);
            };
            let entries =
                crate::territory::graph_entries(subject.cwd).map_err(|error| error.to_string())?;
            let projects = crate::king_board::project_map(subject.cwd);
            let ids = crate::territory::compile_scope_ids(value, &entries, &projects)
                .map_err(|error| format!("territory scope {value:?}: {error}"))?;
            Ok(ids.contains(node))
        }
        _ => Err(format!("unknown target kind {kind:?}")),
    }
}

/// The subject-aware loop reader. Machine holds take precedence; targeted
/// records then hold only the session or territory named by their path.
pub fn verdict_for_subject(scope: &str, subject: &Subject<'_>) -> Verdict {
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return verdict_for(scope);
    };
    verdict_for_subject_at(&home, scope, subject)
}

pub(crate) fn verdict_for_subject_at(
    home: &crate::paths::AgentsHome,
    scope: &str,
    subject: &Subject<'_>,
) -> Verdict {
    let machine = scope_verdict(read_at(&fleet_stop_path(home)), scope);
    if !matches!(&machine, Verdict::Clear(_)) {
        return machine;
    }
    let dir = targets_dir(&home);
    for (path, verdict) in targeted_records(&home) {
        if path == dir {
            return verdict;
        }
        let Some(filename_target) = target_from_path(&path) else {
            continue;
        };
        match verdict {
            Verdict::Stopped(record) => {
                if record.target.as_deref() != Some(filename_target.as_str()) {
                    match target_matches(&filename_target, subject) {
                        Ok(true) => {
                            return Verdict::Unavailable(format!(
                                "target record {} does not match its filename",
                                path.display()
                            ))
                        }
                        Ok(false) => continue,
                        Err(error) => return Verdict::Unavailable(error),
                    }
                }
                if !record.holds_scope(scope) {
                    continue;
                }
                match target_matches(&filename_target, subject) {
                    Ok(true) => return Verdict::Stopped(record),
                    Ok(false) => {}
                    Err(error) => return Verdict::Unavailable(error),
                }
            }
            Verdict::Unavailable(detail) => match target_matches(&filename_target, subject) {
                Ok(true) => return Verdict::Unavailable(detail),
                Ok(false) => {}
                Err(error) => return Verdict::Unavailable(format!("{detail}; {error}")),
            },
            Verdict::Clear(_) => {}
        }
    }
    machine
}

/// The typed reach readout for one record: `(holds, admits)` as scope
/// words, in [`SCOPES`] order. A `clear` record holds nothing.
fn reach(record: &IncidentRecord) -> (Vec<String>, Vec<String>) {
    if record.state == "stopped" {
        (record.held_scopes(), record.admits_scopes())
    } else {
        (Vec::new(), SCOPES.iter().map(|s| s.to_string()).collect())
    }
}

fn utc_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Caller attribution: an explicit `--by`, else the session env stamps, else
/// the login name. Nothing here invents an identity it cannot see.
fn attributed_caller(explicit: Option<&str>) -> String {
    if let Some(b) = explicit {
        if !b.trim().is_empty() {
            return b.trim().to_string();
        }
    }
    for var in ["FNO_SESSION_ID", "FNO_MAIL", "USER"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return v;
            }
        }
    }
    "unknown".to_string()
}

/// Strict reader: the whole state question in one function, shared by the
/// status surface and every admission gate so the two cannot disagree.
/// Absent file -> default clear. An elapsed stop -> clear while retaining its
/// record. An unreadable, unparseable, expired-field-invalid, or wrong-version
/// file is unavailable, never clear.
pub fn read_at(path: &Path) -> Verdict {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Verdict::Clear(IncidentRecord {
                version: STATE_VERSION,
                state: "clear".to_string(),
                generation: 0,
                changed_at: String::new(),
                changed_by: String::new(),
                reason: String::new(),
                holds: Vec::new(),
                source: Some("default".to_string()),
                target: None,
                expires_at: None,
                origin: None,
                mail: None,
                mail_session_id: None,
            });
        }
        Err(e) => return Verdict::Unavailable(format!("unreadable: {e}")),
    };
    let record: IncidentRecord = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => return Verdict::Unavailable(format!("unparseable: {e}")),
    };
    if record.version != STATE_VERSION {
        return Verdict::Unavailable(format!(
            "unsupported version {} (this reader speaks {STATE_VERSION})",
            record.version
        ));
    }
    let expires_at = match record.expires_at.as_deref() {
        Some(value) => match chrono::DateTime::parse_from_rfc3339(value) {
            Ok(value) => Some(value.with_timezone(&chrono::Utc)),
            Err(error) => {
                return Verdict::Unavailable(format!("invalid expires_at {value:?}: {error}"))
            }
        },
        None => None,
    };
    match record.state.as_str() {
        "stopped" if expires_at.is_some_and(|expiry| expiry <= chrono::Utc::now()) => {
            Verdict::Clear(record)
        }
        "stopped" => Verdict::Stopped(record),
        "clear" => Verdict::Clear(record),
        other => Verdict::Unavailable(format!("unknown state {other:?}")),
    }
}

/// The verdict for this machine, from the resolved agents home.
pub fn verdict() -> Verdict {
    match crate::paths::AgentsHome::from_env_opt() {
        Some(home) => read_at(&fleet_stop_path(&home)),
        // Test-only shape (a cargo test that declared no sandbox root): no
        // state root exists, so there is no incident to observe. Production
        // always resolves a home through `from_env`.
        None => Verdict::Clear(IncidentRecord {
            version: STATE_VERSION,
            state: "clear".to_string(),
            generation: 0,
            changed_at: String::new(),
            changed_by: String::new(),
            reason: String::new(),
            holds: Vec::new(),
            source: Some("default".to_string()),
            target: None,
            expires_at: None,
            origin: None,
            mail: None,
            mail_session_id: None,
        }),
    }
}

/// The machine-wide incident file, next to the registry in the agents home.
pub fn fleet_stop_path(home: &crate::paths::AgentsHome) -> PathBuf {
    home.fleet_stop_json()
}

/// The per-subject records share the machine breaker's namespace and reader.
pub fn targets_dir(home: &crate::paths::AgentsHome) -> PathBuf {
    home.fleet_stop_json().with_file_name("fleet-stop.d")
}

fn safe_target_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._,-".contains(&byte))
}

fn target_path(home: &crate::paths::AgentsHome, target: &str) -> Result<PathBuf, String> {
    let (kind, value) = target
        .split_once(':')
        .ok_or_else(|| format!("invalid target {target:?}"))?;
    if !safe_target_value(value) {
        return Err(format!("unsafe target value {value:?}"));
    }
    let name = match kind {
        "session" => format!("session-{value}.json"),
        "territory" => format!("territory-{}.json", value.replace(',', "+")),
        _ => return Err(format!("unknown target kind {kind:?}")),
    };
    Ok(targets_dir(home).join(name))
}

pub(crate) fn parse_ttl(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let mut chars = trimmed.chars();
    let Some(unit) = chars.next_back() else {
        return Err(format!("invalid --ttl: {value:?}"));
    };
    let digits = chars.as_str().trim_end();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid --ttl: {value:?}"));
    }
    let multiplier = match unit.to_ascii_lowercase() {
        's' => 1_u64,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return Err(format!("invalid --ttl: {value:?}")),
    };
    let amount = digits
        .parse::<u64>()
        .map_err(|_| format!("invalid --ttl: {value:?}"))?;
    if amount == 0 {
        return Err(format!("TTL must be > 0: {value:?}"));
    }
    amount
        .checked_mul(multiplier)
        .and_then(|seconds| seconds.checked_mul(1_000))
        .ok_or_else(|| format!("TTL is too large: {value:?}"))
}

fn session_target_value(value: &str, home: &crate::paths::AgentsHome) -> Result<String, String> {
    let value = value.trim();
    if !safe_target_value(value) {
        return Err(format!("unsafe session id {value:?}"));
    }
    if crate::resume_wake::is_uuid_shaped(value) {
        return Ok(value.to_string());
    }
    let registry = crate::state::load_registry(&home.registry_json()).unwrap_or_default();
    let ids: BTreeSet<String> = registry
        .entries
        .iter()
        .flat_map(|row| {
            [
                row.session_id.as_deref(),
                row.harness_session_id.as_deref(),
                row.codex_session_id.as_deref(),
                row.claude_session_uuid.as_deref(),
            ]
            .into_iter()
            .flatten()
        })
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if ids.contains(value) {
        return Ok(value.to_string());
    }
    let matches: Vec<&str> = ids
        .iter()
        .filter(|id| id.starts_with(value))
        .map(String::as_str)
        .collect();
    if !matches.is_empty() {
        return Err(format!(
            "session id {value:?} is incomplete; matching full ids: {}",
            matches.join(", ")
        ));
    }
    Err(format!(
        "session id must be a full UUID or a full registered session id: {value:?}"
    ))
}

fn territory_target_value(value: &str, cwd: &Path) -> Result<String, String> {
    let value = crate::territory::canonical_scope(value);
    if !safe_target_value(&value) {
        return Err(format!("unsafe territory scope {value:?}"));
    }
    let entries = crate::territory::graph_entries(cwd).map_err(|error| error.to_string())?;
    let projects = crate::king_board::project_map(cwd);
    let (canonical, ids) = crate::territory::compile_territory(&value, &entries, &projects)?;
    if ids.is_empty() {
        return Err(format!("territory scope {canonical:?} has no nodes"));
    }
    if !safe_target_value(&canonical) {
        return Err(format!("unsafe territory scope {canonical:?}"));
    }
    Ok(canonical)
}

pub(crate) const DEFAULT_TARGET_TTL_MS: u64 = 3_600_000;

pub(crate) fn expires_after(ttl_ms: u64) -> Result<String, String> {
    let millis = i64::try_from(ttl_ms).map_err(|_| "TTL is too large".to_string())?;
    chrono::Utc::now()
        .checked_add_signed(chrono::Duration::milliseconds(millis))
        .map(|expiry| expiry.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .ok_or_else(|| "TTL is too large".to_string())
}

fn default_target_holds(target: &str) -> Vec<String> {
    if target.starts_with("session:") {
        vec!["loops".to_string()]
    } else {
        vec!["spawns".to_string(), "loops".to_string()]
    }
}

fn target_holds(target: &str, requested: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let Some(requested) = requested.filter(|holds| !holds.is_empty()) else {
        return Ok(default_target_holds(target));
    };
    if requested
        .iter()
        .any(|hold| matches!(hold.as_str(), "tests" | "merges"))
    {
        return Err(
            "tests and merges are machine doors; a targeted halt holds spawns or loops".into(),
        );
    }
    if target.starts_with("session:") && requested.iter().any(|hold| hold == "spawns") {
        return Err(
            "a session halt holds its loop; hold dispatch with --territory or no target".into(),
        );
    }
    Ok(requested)
}

/// Temp-file write plus atomic rename in the destination directory: a reader
/// mid-write sees either the old record or the new one, never a torn file.
fn write_record(path: &Path, record: &IncidentRecord) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let body = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".fleet-stop.tmp-{}", std::process::id()));
    {
        // 0600: the record carries who stopped the fleet and why.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .and_then(|_| f.flush())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })
}

/// Read the CURRENT generation honestly: absent counts as generation 0, a
/// readable record carries its own. An unreadable record refuses the write -
/// silently replacing evidence is the one move worse than a blocked stop.
fn current_generation(path: &Path) -> Result<u64, String> {
    match read_at(path) {
        Verdict::Clear(r) | Verdict::Stopped(r) => Ok(r.generation),
        Verdict::Unavailable(detail) => Err(format!(
            "existing {} is unreadable ({detail}); remove it by hand to start a fresh generation",
            path.display()
        )),
    }
}

fn require_reason(reason: Option<&str>) -> Result<String, String> {
    let reason = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| "a nonblank --reason is required".to_string())?;
    Ok(reason.to_string())
}

#[cfg(test)]
fn write_transition(
    path: &Path,
    state: &str,
    reason: Option<&str>,
    by: Option<&str>,
    holds: Vec<String>,
) -> Result<IncidentRecord, String> {
    write_transition_with_metadata(path, state, reason, by, holds, RecordMetadata::default())
}

pub(crate) fn write_transition_with_metadata(
    path: &Path,
    state: &str,
    reason: Option<&str>,
    by: Option<&str>,
    holds: Vec<String>,
    metadata: RecordMetadata,
) -> Result<IncidentRecord, String> {
    if metadata.target.is_some() {
        let dir = path
            .parent()
            .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot secure {}: {e}", dir.display()))?;
    }
    // Read-modify-write under an exclusive sidecar lock: two concurrent stops
    // must not both read generation N and both claim N+1 in their receipts.
    // The lock file is a sidecar because the state file itself is replaced by
    // rename, which would drop the lock mid-write.
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("cannot lock {}: {e}", lock_path.display()))?;
    let out = write_transition_locked(path, state, reason, by, holds, metadata);
    let _ = lock.unlock();
    out
}

fn write_transition_locked(
    path: &Path,
    state: &str,
    reason: Option<&str>,
    by: Option<&str>,
    holds: Vec<String>,
    metadata: RecordMetadata,
) -> Result<IncidentRecord, String> {
    let reason = require_reason(reason)?;
    if metadata.origin.as_deref() == Some("pause-all") {
        match read_at(path) {
            Verdict::Stopped(record) | Verdict::Clear(record)
                if record.state == "stopped" && record.origin.as_deref() != Some("pause-all") =>
            {
                return Err(
                    "fleet incident owns the breaker; use fno agents incident clear first".into(),
                );
            }
            Verdict::Unavailable(detail) => {
                return Err(format!("existing breaker is unreadable: {detail}"));
            }
            Verdict::Stopped(_) | Verdict::Clear(_) => {}
        }
    }
    // The machine arm's tests-first hold never replaces a stop someone armed.
    if metadata.origin.as_deref() == Some(MACHINE_ORIGIN)
        && !matches!(read_at(path), Verdict::Clear(_))
    {
        return Err("a stop is already armed".into());
    }
    let generation = current_generation(path)? + 1;
    let record = IncidentRecord {
        version: STATE_VERSION,
        state: state.to_string(),
        generation,
        changed_at: utc_now(),
        changed_by: attributed_caller(by),
        reason,
        holds,
        source: Some("file".to_string()),
        target: metadata.target,
        expires_at: metadata.expires_at,
        origin: metadata.origin,
        mail: metadata.mail,
        mail_session_id: metadata.mail_session_id,
    };
    write_record(path, &record)?;
    Ok(record)
}

/// Record successful mail release without creating another breaker
/// generation. The compare fields prevent a stale resume from marking a newer
/// pause as released.
pub(crate) fn mark_pause_mail_released(
    path: &Path,
    expected_generation: u64,
    owner_session_id: &str,
) -> Result<(), String> {
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("cannot open {}: {error}", lock_path.display()))?;
    lock.lock()
        .map_err(|error| format!("cannot lock {}: {error}", lock_path.display()))?;
    let result = match read_at(path) {
        Verdict::Clear(mut record) | Verdict::Stopped(mut record)
            if record.state == "clear"
                && record.origin.as_deref() == Some("pause-all")
                && record.mail.as_deref() == Some("armed")
                && record.mail_session_id.as_deref() == Some(owner_session_id)
                && record.generation == expected_generation =>
        {
            record.mail = Some("lifted".to_string());
            write_record(path, &record).map(|()| ())
        }
        Verdict::Unavailable(detail) => Err(format!("breaker became unreadable: {detail}")),
        Verdict::Clear(_) | Verdict::Stopped(_) => {
            Err("pause-all breaker changed before recording mail release".to_string())
        }
    };
    let _ = lock.unlock();
    result
}

/// Parse a `--hold` value: comma-separated scope words, resolved to
/// [`SCOPES`] order with duplicates dropped. An unknown word refuses by
/// name, so a typo can never arm a scope the operator did not mean.
fn parse_hold_list(value: &str) -> Result<Vec<String>, String> {
    let mut words: Vec<&str> = Vec::new();
    for word in value.split(',') {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        if !SCOPES.contains(&word) {
            return Err(format!(
                "unknown hold scope {word:?} (valid: {})",
                SCOPES.join(", ")
            ));
        }
        if !words.contains(&word) {
            words.push(word);
        }
    }
    Ok(SCOPES
        .iter()
        .copied()
        .filter(|s| words.contains(s))
        .map(|s| s.to_string())
        .collect())
}

/// One receipt line, JSON, on stdout: the resulting state, its generation,
/// and the typed reach it carries.
fn print_receipt(record: &IncidentRecord) {
    let (holds, admits) = reach(record);
    println!(
        "{}",
        serde_json::json!({
            "state": record.state,
            "generation": record.generation,
            "holds": holds,
            "admits": admits,
            "changed_at": record.changed_at,
            "changed_by": record.changed_by,
            "reason": record.reason,
            "target": record.target,
            "expires_at": record.expires_at,
        })
    );
}

fn print_usage() {
    println!(
        "usage: fno-agents fleet-incident stop --reason <text> [--hold spawns,tests,merges,loops] [--session <id>|--territory <scope>] [--ttl <dur>] | clear --reason <text> [--session <id>|--territory <scope>] | status [--json] | check [--scope spawns|tests|merges|loops] [--json]"
    );
}

/// Strict flag parse for the read-only arms (status/check): only --json is
/// accepted, so a mistyped flag refuses with usage instead of silently
/// flipping the output format a caller is parsing.
fn parse_read_flags(rest: &[String]) -> Result<bool, i32> {
    let mut as_json = false;
    for a in rest {
        match a.as_str() {
            "--json" | "-J" => as_json = true,
            other => {
                eprintln!("fleet-incident: unrecognized argument {other:?}");
                return Err(2);
            }
        }
    }
    Ok(as_json)
}

/// Strict flag parse for `check`: `--json`/`-J` and one `--scope` word,
/// default `spawns`. The scope's value is consumed here, so `--scope tests`
/// cannot leave `tests` behind as a stray positional.
fn parse_check_flags(rest: &[String]) -> Result<(bool, &str), i32> {
    let mut as_json = false;
    let mut scope: &str = "spawns";
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--json" | "-J" => as_json = true,
            "--scope" => match rest.get(i + 1) {
                Some(v) if SCOPES.contains(&v.as_str()) => {
                    scope = v.as_str();
                    i += 1;
                }
                Some(other) => {
                    eprintln!(
                        "fleet-incident: unknown check scope {other:?} (valid: {})",
                        SCOPES.join(", ")
                    );
                    return Err(2);
                }
                None => {
                    eprintln!("fleet-incident: --scope needs a value");
                    return Err(2);
                }
            },
            other => {
                eprintln!("fleet-incident: unrecognized argument {other:?}");
                return Err(2);
            }
        }
        i += 1;
    }
    Ok((as_json, scope))
}

fn check_json(verdict: &Verdict, scope: &str) -> serde_json::Value {
    let (state, generation, reason, holds, admits) = match verdict {
        Verdict::Clear(record) => (
            "clear",
            Some(record.generation),
            record.reason.clone(),
            Vec::new(),
            SCOPES.iter().map(|scope| scope.to_string()).collect(),
        ),
        Verdict::Stopped(record) => {
            let (holds, admits) = reach(record);
            (
                "stopped",
                Some(record.generation),
                record.reason.clone(),
                holds,
                admits,
            )
        }
        Verdict::Unavailable(detail) => (
            "unavailable",
            None,
            detail.clone(),
            SCOPES.iter().map(|scope| scope.to_string()).collect(),
            Vec::new(),
        ),
    };
    serde_json::json!({
        "state": state,
        "generation": generation,
        "reason": reason,
        "scope": scope,
        "holds": holds,
        "admits": admits,
    })
}

/// Binary entry: `fleet-incident stop|clear|status|check`.
pub fn run_fleet_incident(args: &[String]) -> i32 {
    let Some(action) = args.first() else {
        print_usage();
        return 2;
    };
    let rest = &args[1..];
    if matches!(action.as_str(), "-h" | "--help" | "help")
        || rest
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        print_usage();
        return 0;
    }
    match action.as_str() {
        "stop" | "clear" => {
            let mut reason: Option<String> = None;
            let mut by: Option<String> = None;
            let mut holds: Option<Vec<String>> = None;
            let mut raw_target: Option<(String, String)> = None;
            let mut ttl_ms: Option<u64> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--reason" => {
                        match rest.get(i + 1) {
                            Some(v) => reason = Some(v.clone()),
                            None => {
                                eprintln!("fleet-incident: --reason needs a value");
                                return 2;
                            }
                        }
                        i += 2;
                    }
                    "--by" => {
                        match rest.get(i + 1) {
                            Some(v) => by = Some(v.clone()),
                            None => {
                                eprintln!("fleet-incident: --by needs a value");
                                return 2;
                            }
                        }
                        i += 2;
                    }
                    "--hold" => {
                        match rest.get(i + 1) {
                            Some(v) => match parse_hold_list(v) {
                                Ok(h) => holds = Some(h),
                                Err(e) => {
                                    eprintln!("fleet-incident: {e}");
                                    return 2;
                                }
                            },
                            None => {
                                eprintln!("fleet-incident: --hold needs a value");
                                return 2;
                            }
                        }
                        i += 2;
                    }
                    "--session" | "--territory" => {
                        let flag = rest[i].as_str();
                        let Some(value) = rest.get(i + 1) else {
                            eprintln!("fleet-incident: {flag} needs a value");
                            return 2;
                        };
                        if raw_target.is_some() {
                            eprintln!("fleet-incident: choose one of --session or --territory");
                            return 2;
                        }
                        let kind = if flag == "--session" {
                            "session"
                        } else {
                            "territory"
                        };
                        raw_target = Some((kind.to_string(), value.clone()));
                        i += 2;
                    }
                    "--ttl" => {
                        match rest.get(i + 1) {
                            Some(value) => match parse_ttl(value) {
                                Ok(parsed) => ttl_ms = Some(parsed),
                                Err(error) => {
                                    eprintln!("fleet-incident: {error}");
                                    return 2;
                                }
                            },
                            None => {
                                eprintln!("fleet-incident: --ttl needs a value");
                                return 2;
                            }
                        }
                        i += 2;
                    }
                    other => {
                        eprintln!("fleet-incident: unrecognized argument {other:?}");
                        return 2;
                    }
                }
            }
            // A stop without --hold holds every scope; a re-stop with a new
            // list is how the reach changes, and it bumps the generation. A
            // clear lifts every scope, so an explicit --hold there is a
            // misunderstanding worth refusing, not ignoring.
            if action == "clear" && holds.is_some() {
                eprintln!("fleet-incident: clear lifts every scope; arm a reach with stop --hold");
                return 2;
            }
            if action == "clear" && ttl_ms.is_some() {
                eprintln!("fleet-incident: --ttl applies to stop only");
                return 2;
            }
            let home = crate::paths::AgentsHome::from_env();
            let cwd = match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(error) => {
                    eprintln!("fleet-incident: cannot resolve cwd: {error}");
                    return 2;
                }
            };
            let target = match raw_target {
                Some((kind, value)) => {
                    let value = match kind.as_str() {
                        "session" => session_target_value(&value, &home),
                        "territory" => territory_target_value(&value, &cwd),
                        _ => unreachable!(),
                    };
                    match value {
                        Ok(value) => Some(format!("{kind}:{value}")),
                        Err(error) => {
                            eprintln!("fleet-incident: {error}");
                            return 2;
                        }
                    }
                }
                None => None,
            };
            let path = match target.as_deref() {
                Some(target) => match target_path(&home, target) {
                    Ok(path) => path,
                    Err(error) => {
                        eprintln!("fleet-incident: {error}");
                        return 2;
                    }
                },
                None => fleet_stop_path(&home),
            };
            let held = if action == "stop" {
                match target.as_deref() {
                    Some(target) => match target_holds(target, holds) {
                        Ok(holds) => holds,
                        Err(error) => {
                            eprintln!("fleet-incident: {error}");
                            return 2;
                        }
                    },
                    None => holds
                        .unwrap_or_else(|| SCOPES.iter().map(|scope| scope.to_string()).collect()),
                }
            } else {
                Vec::new()
            };
            let expiry_ms = if action == "stop" {
                match (target.is_some(), ttl_ms) {
                    (true, None) => Some(DEFAULT_TARGET_TTL_MS),
                    (_, ttl) => ttl,
                }
            } else {
                None
            };
            let expires_at = match expiry_ms.map(expires_after).transpose() {
                Ok(expires_at) => expires_at,
                Err(error) => {
                    eprintln!("fleet-incident: {error}");
                    return 2;
                }
            };
            let machine_wide = target.is_none();
            match write_transition_with_metadata(
                &path,
                if action == "stop" { "stopped" } else { "clear" },
                reason.as_deref(),
                by.as_deref(),
                held,
                RecordMetadata {
                    target,
                    expires_at,
                    ..RecordMetadata::default()
                },
            ) {
                Ok(record) => {
                    print_receipt(&record);
                    // The doors hold new tests; the running ones follow the
                    // record here, paused or resumed, with one bus line.
                    if machine_wide {
                        match crate::test_hold::reconcile(&home) {
                            Ok(outcome) => {
                                if let Some(line) = outcome.line() {
                                    eprintln!("fleet-incident: {line}");
                                }
                            }
                            Err(error) => {
                                eprintln!("fleet-incident: test hold not applied: {error}")
                            }
                        }
                    }
                    0
                }
                Err(e) => {
                    eprintln!("fleet-incident {action} refused: {e}");
                    1
                }
            }
        }
        "status" => {
            let as_json = match parse_read_flags(rest) {
                Ok(v) => v,
                Err(code) => return code,
            };
            let home = crate::paths::AgentsHome::from_env();
            let targets = targeted_statuses(&home);
            let path = fleet_stop_path(&home);
            match read_at(&path) {
                Verdict::Clear(record) => {
                    if as_json {
                        println!("{}", machine_status_json(&record, "clear", &targets));
                    } else {
                        print_machine_status(&record, "clear", &targets);
                    }
                    0
                }
                Verdict::Stopped(record) => {
                    if as_json {
                        println!("{}", machine_status_json(&record, "stopped", &targets));
                    } else {
                        print_machine_status(&record, "stopped", &targets);
                    }
                    1
                }
                Verdict::Unavailable(detail) => {
                    if as_json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "state": "unavailable",
                                "detail": detail,
                                "targets": targets.iter().map(TargetStatus::json).collect::<Vec<_>>(),
                            })
                        );
                    } else {
                        eprintln!("fleet incident state UNAVAILABLE: {detail}");
                        for target in &targets {
                            target.print_human();
                        }
                    }
                    1
                }
            }
        }
        "check" => {
            // Admission verdict for callers that cannot link this crate (the
            // spawn gate). Asks one scope's question - the spawn gate's own
            // scope is `spawns` - so a stop that holds only tests or merges
            // reads as clear here. Exit 0 clear, 90 stopped, 91 unavailable
            // - and the JSON names which, so an exit code read alone can
            // never confuse "stopped" with "cannot tell".
            let (as_json, scope) = match parse_check_flags(rest) {
                Ok(v) => v,
                Err(code) => return code,
            };
            let v = verdict_for(scope);
            if as_json {
                println!("{}", check_json(&v, scope));
            }
            match v {
                Verdict::Clear(_) => 0,
                Verdict::Stopped(r) => {
                    eprintln!(
                        "fleet-stop: admission refused (generation {}, reason: {})",
                        r.generation, r.reason
                    );
                    EXIT_CHECK_STOPPED
                }
                Verdict::Unavailable(d) => {
                    eprintln!("fleet-stop-unavailable: incident state unreadable: {d}");
                    EXIT_CHECK_UNAVAILABLE
                }
            }
        }
        _ => {
            print_usage();
            2
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_flags_accept_both_json_spellings() {
        assert_eq!(super::parse_read_flags(&[]), Ok(false));
        assert_eq!(super::parse_read_flags(&["--json".to_string()]), Ok(true));
        assert_eq!(super::parse_read_flags(&["-J".to_string()]), Ok(true));
        assert_eq!(
            super::parse_read_flags(&["--bogus".to_string()]),
            Err(2),
            "a mistyped flag still refuses with usage"
        );
    }

    #[test]
    fn check_flags_consume_the_scope_value_and_default_to_spawns() {
        let args =
            |words: &[&str]| -> Vec<String> { words.iter().map(|w| w.to_string()).collect() };
        assert_eq!(super::parse_check_flags(&[]), Ok((false, "spawns")));
        assert_eq!(
            super::parse_check_flags(&args(&["--scope", "tests"])),
            Ok((false, "tests")),
            "the scope value is consumed, never left as a stray positional"
        );
        assert_eq!(
            super::parse_check_flags(&args(&["--json", "--scope", "merges"])),
            Ok((true, "merges"))
        );
        assert_eq!(
            super::parse_check_flags(&args(&["--scope", "bogus"])),
            Err(2),
            "an unknown scope refuses with usage"
        );
        assert_eq!(super::parse_check_flags(&args(&["--scope"])), Err(2));
    }

    #[test]
    fn expired_check_json_reports_clear_reach() {
        let path = tmp_path("expired-check");
        std::fs::write(
            &path,
            serde_json::json!({
                "version": STATE_VERSION,
                "state": "stopped",
                "generation": 3,
                "changed_at": "2000-01-01T00:00:00Z",
                "changed_by": "op",
                "reason": "temporary hold",
                "holds": ["loops"],
                "expires_at": "2000-01-01T01:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        let verdict = scope_verdict(read_at(&path), "loops");
        let payload = check_json(&verdict, "loops");
        assert_eq!(payload["state"], "clear");
        assert_eq!(payload["holds"], serde_json::json!([]));
        assert_eq!(
            payload["admits"],
            serde_json::json!(SCOPES.iter().map(|scope| *scope).collect::<Vec<_>>())
        );
        cleanup(&path);
    }

    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-fleet-incident-{}-{tag}", std::process::id()));
        // Fresh state here, ONCE: a start-of-test cleanup after this create
        // would delete the directory the test is about to write into.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("fleet-stop.json")
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }

    use crate::AgentsHomeEnvGuard;

    fn write_target_for_test(
        home: &crate::paths::AgentsHome,
        target: &str,
        holds: &[&str],
    ) -> PathBuf {
        let path = super::target_path(home, target).unwrap();
        super::write_transition_with_metadata(
            &path,
            "stopped",
            Some("test pause"),
            Some("op"),
            holds.iter().map(|hold| (*hold).to_string()).collect(),
            super::RecordMetadata {
                target: Some(target.to_string()),
                ..super::RecordMetadata::default()
            },
        )
        .unwrap();
        path
    }

    #[test]
    fn absent_file_reads_default_clear_with_source_marker() {
        let path = tmp_path("absent");
        match read_at(&path) {
            Verdict::Clear(r) => {
                assert_eq!(r.generation, 0);
                assert_eq!(r.source, Some("default".to_string()));
            }
            other => panic!("expected default clear, got {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn stop_increments_generation_and_a_fresh_read_sees_it() {
        let path = tmp_path("roundtrip");
        let stopped = write_transition(&path, "stopped", Some("wedged lock"), Some("op"), vec![])
            .expect("stop writes");
        assert_eq!(stopped.generation, 1);
        assert_eq!(stopped.state, "stopped");
        assert_eq!(stopped.changed_by, "op");
        // A FRESH read (the process-boundary proof): the record on disk, not
        // the value in hand, carries the verdict.
        match read_at(&path) {
            Verdict::Stopped(r) => {
                assert_eq!(r.generation, 1);
                assert_eq!(r.reason, "wedged lock");
                assert_eq!(r.version, STATE_VERSION);
                assert!(!r.changed_at.is_empty());
            }
            other => panic!("expected stopped after fresh read, got {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn clear_is_a_positive_marker_and_continues_the_generation() {
        let path = tmp_path("clear");
        write_transition(&path, "stopped", Some("incident"), None, vec![]).unwrap();
        let cleared = write_transition(&path, "clear", Some("incident resolved"), None, vec![])
            .expect("clear writes");
        assert_eq!(cleared.generation, 2);
        assert_eq!(cleared.state, "clear");
        match read_at(&path) {
            Verdict::Clear(r) => {
                assert_eq!(r.generation, 2);
                assert_eq!(r.source, Some("file".to_string()));
            }
            other => panic!("expected durable clear, got {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn corrupt_state_is_unavailable_never_clear() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, b"{not json").unwrap();
        match read_at(&path) {
            Verdict::Unavailable(detail) => assert!(!detail.is_empty()),
            other => panic!("expected unavailable, got {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn unknown_state_word_is_unavailable() {
        let path = tmp_path("unknown-state");
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"state":"sorta","generation":3,"changed_at":"x","changed_by":"y","reason":"z"}}"#),
        )
        .unwrap();
        assert!(matches!(read_at(&path), Verdict::Unavailable(_)));
        cleanup(&path);
    }

    #[test]
    fn wrong_version_is_unavailable() {
        let path = tmp_path("version");
        std::fs::write(
            &path,
            r#"{"version":99,"state":"clear","generation":1,"changed_at":"","changed_by":"","reason":""}"#,
        )
        .unwrap();
        assert!(matches!(read_at(&path), Verdict::Unavailable(_)));
        cleanup(&path);
    }

    #[test]
    fn blank_reason_refuses_the_write() {
        let path = tmp_path("blank-reason");
        assert!(write_transition(&path, "stopped", Some(""), None, vec![]).is_err());
        assert!(write_transition(&path, "stopped", Some("   "), None, vec![]).is_err());
        assert!(write_transition(&path, "stopped", None, None, vec![]).is_err());
        assert!(!path.exists(), "a refused write must not create state");
        cleanup(&path);
    }

    #[test]
    fn unreadable_state_refuses_the_write_instead_of_overwriting() {
        let path = tmp_path("refuse-overwrite");
        std::fs::write(&path, b"garbage").unwrap();
        let err = write_transition(&path, "stopped", Some("reason"), None, vec![])
            .expect_err("write over unreadable state must refuse");
        assert!(err.contains("unreadable"));
        cleanup(&path);
    }

    #[test]
    fn pause_all_alias_cannot_race_over_or_clear_an_incident() {
        let path = tmp_path("pause-all-incident-race");
        write_transition(&path, "stopped", Some("incident"), Some("op"), vec![]).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let metadata = || RecordMetadata {
            origin: Some("pause-all".into()),
            mail: Some("kept".into()),
            ..RecordMetadata::default()
        };

        let stopped = write_transition_with_metadata(
            &path,
            "stopped",
            Some("pause-all"),
            Some("op"),
            vec!["spawns".into(), "loops".into()],
            metadata(),
        );
        let cleared = write_transition_with_metadata(
            &path,
            "clear",
            Some("resume-all"),
            Some("op"),
            Vec::new(),
            metadata(),
        );

        assert!(stopped.unwrap_err().contains("incident owns the breaker"));
        assert!(cleared.unwrap_err().contains("incident owns the breaker"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        cleanup(&path);
    }

    #[test]
    fn verdict_is_stopped_only_for_a_readable_stop() {
        let path = tmp_path("verdict");
        assert!(!read_at(&path).is_stopped());
        write_transition(&path, "stopped", Some("reason"), None, vec![]).unwrap();
        assert!(read_at(&path).is_stopped());
        write_transition(&path, "clear", Some("done"), None, vec![]).unwrap();
        assert!(!read_at(&path).is_stopped());
        std::fs::write(&path, b"junk").unwrap();
        assert!(!read_at(&path).is_stopped());
        cleanup(&path);
    }

    #[test]
    fn hold_list_resolves_order_drops_dupes_and_refuses_unknown_words() {
        assert_eq!(
            super::parse_hold_list("loops,merges, tests,merges").unwrap(),
            vec![
                "tests".to_string(),
                "merges".to_string(),
                "loops".to_string()
            ],
            "the readout order is canonical, whatever order the operator typed"
        );
        assert_eq!(super::parse_hold_list("").unwrap(), Vec::<String>::new());
        let err = super::parse_hold_list("spawns,mergess").expect_err("typo must refuse");
        assert!(err.contains("mergess") && err.contains("spawns, tests, merges"));
    }

    #[test]
    fn ttl_expiry_refuses_duration_overflow_without_panicking() {
        assert!(super::expires_after(u64::MAX).is_err());
        assert!(super::expires_after(i64::MAX as u64).is_err());
    }

    #[test]
    fn stop_session_writes_a_bounded_loops_record() {
        let home = tempfile::TempDir::new().unwrap();
        let _env = AgentsHomeEnvGuard::set(home.path());
        let session_id = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let args = [
            "stop",
            "--session",
            session_id,
            "--reason",
            "hold for repair",
        ]
        .map(str::to_string);
        let result = super::run_fleet_incident(&args);

        assert_eq!(result, 0, "a full session id is a valid halt target");
        let targets = home.path().join("fleet-stop.d");
        let files = std::fs::read_dir(targets)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        let record: IncidentRecord =
            serde_json::from_slice(&std::fs::read(files[0].path()).unwrap()).unwrap();
        let target = format!("session:{session_id}");
        assert_eq!(record.target.as_deref(), Some(target.as_str()));
        assert_eq!(record.holds, vec!["loops"]);
        let expires_at =
            chrono::DateTime::parse_from_rfc3339(record.expires_at.as_deref().unwrap()).unwrap();
        let remaining = expires_at
            .signed_duration_since(chrono::Utc::now())
            .num_seconds();
        assert!((3_590..=3_600).contains(&remaining));
    }

    #[test]
    fn targeted_session_record_matches_only_its_subject() {
        let home = tempfile::TempDir::new().unwrap();
        let _env = AgentsHomeEnvGuard::set(home.path());
        let agents_home = crate::paths::AgentsHome::at(home.path());
        let session_id = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let target = format!("session:{session_id}");
        write_target_for_test(&agents_home, &target, &["loops"]);
        let cwd = Path::new(".");
        let matching = Subject {
            session_ids: vec![session_id.to_string()],
            node: None,
            territory: None,
            cwd,
        };
        let other = Subject {
            session_ids: vec!["11111111-2222-3333-4444-555555555555".to_string()],
            node: None,
            territory: None,
            cwd,
        };

        assert!(matches!(
            verdict_for_subject("loops", &matching),
            Verdict::Stopped(_)
        ));
        assert!(matches!(
            verdict_for_subject("loops", &other),
            Verdict::Clear(_)
        ));
    }

    #[test]
    fn session_prefix_refusal_names_every_matching_full_id() {
        let root = tempfile::TempDir::new().unwrap();
        let home = crate::paths::AgentsHome::at(root.path());
        std::fs::create_dir_all(home.root()).unwrap();
        let ids = [
            "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9",
            "0a1b2c3d-1111-2222-3333-444444444444",
        ];
        let mut entries = Vec::new();
        for (index, id) in ids.iter().enumerate() {
            let mut entry = crate::state::RegistryEntry::default();
            entry.name = format!("worker-{index}");
            entry.cwd = root.path().display().to_string();
            entry.session_id = Some((*id).to_string());
            entry.harness_session_id = Some((*id).to_string());
            entries.push(entry);
        }
        let registry = crate::state::Registry {
            entries,
            ..crate::state::Registry::default()
        };
        std::fs::write(home.registry_json(), serde_json::to_vec(&registry).unwrap()).unwrap();

        let result = super::session_target_value("0a1b2c3d", &home);
        assert!(result.is_err(), "a short session prefix must be refused");
        let error = result.unwrap_err();
        assert!(error.contains(ids[0]), "{error}");
        assert!(error.contains(ids[1]), "{error}");
    }

    #[test]
    fn targeted_territory_record_matches_its_canonical_scope() {
        let home = tempfile::TempDir::new().unwrap();
        let _env = AgentsHomeEnvGuard::set(home.path());
        let agents_home = crate::paths::AgentsHome::at(home.path());
        let target = "territory:x-child,x-epic";
        write_target_for_test(&agents_home, target, &["spawns", "loops"]);
        let cwd = Path::new(".");
        let matching = Subject {
            session_ids: Vec::new(),
            node: None,
            territory: Some("x-child, x-epic".to_string()),
            cwd,
        };
        let other = Subject {
            session_ids: Vec::new(),
            node: None,
            territory: Some("x-other".to_string()),
            cwd,
        };

        assert!(matches!(
            verdict_for_subject("loops", &matching),
            Verdict::Stopped(_)
        ));
        assert!(matches!(
            verdict_for_subject("loops", &other),
            Verdict::Clear(_)
        ));
    }

    #[test]
    fn corrupt_targeted_record_fails_closed_only_for_its_filename_target() {
        let home = tempfile::TempDir::new().unwrap();
        let _env = AgentsHomeEnvGuard::set(home.path());
        let agents_home = crate::paths::AgentsHome::at(home.path());
        let session_id = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let path = super::target_path(&agents_home, &format!("session:{session_id}")).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        let cwd = Path::new(".");
        let matching = Subject {
            session_ids: vec![session_id.to_string()],
            node: None,
            territory: None,
            cwd,
        };
        let other = Subject {
            session_ids: vec!["11111111-2222-3333-4444-555555555555".to_string()],
            node: None,
            territory: None,
            cwd,
        };

        assert!(matches!(
            verdict_for_subject("loops", &matching),
            Verdict::Unavailable(_)
        ));
        assert!(matches!(
            verdict_for_subject("loops", &other),
            Verdict::Clear(_)
        ));
    }

    #[test]
    fn targeted_status_marks_an_expired_record() {
        let home = tempfile::TempDir::new().unwrap();
        let agents_home = crate::paths::AgentsHome::at(home.path());
        let target = "session:0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        let path = super::target_path(&agents_home, target).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "version": STATE_VERSION,
                "state": "stopped",
                "generation": 3,
                "changed_at": "2000-01-01T00:00:00Z",
                "changed_by": "op",
                "reason": "temporary hold",
                "holds": ["loops"],
                "target": target,
                "expires_at": "2000-01-01T01:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        let status = TargetStatus::from_record(path.clone(), read_at(&path));
        assert_eq!(status.state, "expired");
        let json = status.json();
        assert_eq!(json["state"], "expired");
        assert_eq!(json["target"], target);
        assert_eq!(json["holds"], serde_json::json!(["loops"]));
    }

    #[test]
    fn a_record_without_scopes_keeps_the_legacy_reach() {
        let path = tmp_path("legacy");
        std::fs::write(
            &path,
            r#"{"version":1,"state":"stopped","generation":4,"changed_at":"x","changed_by":"y","reason":"pre-scope stop"}"#,
        )
        .unwrap();
        match read_at(&path) {
            Verdict::Stopped(r) => {
                // The pre-scope reach held dispatch, tests, and live loops.
                assert_eq!(
                    r.held_scopes(),
                    vec![
                        "spawns".to_string(),
                        "tests".to_string(),
                        "loops".to_string()
                    ]
                );
                assert_eq!(r.admits_scopes(), vec!["merges".to_string()]);
            }
            other => panic!("expected the legacy record, got {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn a_scoped_stop_holds_only_its_scopes() {
        let path = tmp_path("scoped");
        write_transition(
            &path,
            "stopped",
            Some("demo recording"),
            None,
            vec!["tests".to_string()],
        )
        .unwrap();
        let v = read_at(&path);
        assert!(v.holds("tests"));
        assert!(!v.holds("spawns"), "a tests-only stop admits spawns");
        assert!(!v.holds("merges"), "a tests-only stop admits merges");
        cleanup(&path);
    }

    #[test]
    fn an_expired_stop_reads_clear_and_keeps_its_record() {
        let path = tmp_path("expired");
        std::fs::write(
            &path,
            serde_json::json!({
                "version": STATE_VERSION,
                "state": "stopped",
                "generation": 3,
                "changed_at": "2000-01-01T00:00:00Z",
                "changed_by": "op",
                "reason": "temporary hold",
                "expires_at": "2000-01-01T01:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        match read_at(&path) {
            Verdict::Clear(record) => assert_eq!(record.state, "stopped"),
            other => panic!("expected the expired stop to read clear, got {other:?}"),
        }
        assert!(path.exists(), "expiry changes the verdict, not the record");
        cleanup(&path);
    }

    #[test]
    fn an_unparseable_expiry_is_unavailable() {
        let path = tmp_path("invalid-expiry");
        std::fs::write(
            &path,
            serde_json::json!({
                "version": STATE_VERSION,
                "state": "stopped",
                "generation": 3,
                "changed_at": "2000-01-01T00:00:00Z",
                "changed_by": "op",
                "reason": "temporary hold",
                "expires_at": "not-a-timestamp"
            })
            .to_string(),
        )
        .unwrap();

        assert!(matches!(read_at(&path), Verdict::Unavailable(_)));
        cleanup(&path);
    }

    #[test]
    fn the_reach_readout_resolves_every_state() {
        let path = tmp_path("reach");
        write_transition(
            &path,
            "stopped",
            Some("demo recording"),
            None,
            vec!["tests".to_string()],
        )
        .unwrap();
        match read_at(&path) {
            Verdict::Stopped(r) => {
                let (holds, admits) = super::reach(&r);
                assert_eq!(holds, vec!["tests".to_string()]);
                assert_eq!(
                    admits,
                    vec![
                        "spawns".to_string(),
                        "merges".to_string(),
                        "loops".to_string()
                    ]
                );
            }
            other => panic!("expected a stopped record, got {other:?}"),
        }
        // A clear record holds nothing and admits everything.
        write_transition(&path, "clear", Some("done"), None, vec![]).unwrap();
        match read_at(&path) {
            Verdict::Clear(r) => {
                let (holds, admits) = super::reach(&r);
                assert!(holds.is_empty());
                assert_eq!(
                    admits,
                    super::SCOPES
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                );
            }
            other => panic!("expected a clear record, got {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn the_reach_readout_matches_the_doors() {
        // The typed readout goes stale the moment a door stops reading its
        // scope - or reads none. Each door must read through `verdict_for`
        // with its own scope word; if a door retires, its scope leaves the
        // readout in the same change.
        assert!(
            include_str!("spawn_gate.rs").contains(r#"verdict_for("spawns")"#),
            "the spawn gate must read the breaker through verdict_for(\"spawns\")"
        );
        assert!(
            include_str!("test_run.rs").contains(r#"verdict_for("tests")"#),
            "the test doors must read the breaker through verdict_for(\"tests\")"
        );
        assert!(
            include_str!("authorized_merge.rs").contains(r#"verdict_for("merges")"#),
            "the merge primitive must refuse while merges are held; if it stops \
             reading the breaker, move merges out of the readout in the same change"
        );
    }
}
