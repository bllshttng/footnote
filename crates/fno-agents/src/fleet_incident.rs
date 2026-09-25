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
//! primitive.
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
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// The only schema version this reader understands.
pub const STATE_VERSION: u32 = 1;

/// The scopes an armed breaker can hold, in readout order. The words the
/// `--hold` flag and the typed reach readout speak.
pub const SCOPES: &[&str] = &["spawns", "tests", "merges"];

/// The reach a record written before scopes existed carried: it held
/// spawns and tests, and merges proceeded.
pub const LEGACY_HOLDS: &[&str] = &["spawns", "tests"];

/// Exit codes for the `check` verdict verb, distinct from every dispatch and
/// gate code in use - see the allocation table in
/// `cli/src/fno/agents/spawn_gate.py` (gate band 75-77, 79, 82-84), plus the
/// convention codes 2, 13-15, 18, 124, 127.
pub const EXIT_CHECK_STOPPED: i32 = 90;
pub const EXIT_CHECK_UNAVAILABLE: i32 = 91;

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
}

/// What an admission reader is told. `Unavailable` carries the read error.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Clear(IncidentRecord),
    Stopped(IncidentRecord),
    Unavailable(String),
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

    fn holds_scope(&self, scope: &str) -> bool {
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
    match verdict() {
        Verdict::Stopped(r) if !r.holds_scope(scope) => Verdict::Clear(r),
        v => v,
    }
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
/// Absent file -> default clear. Existing but unreadable/unparseable/wrong
/// version -> Unavailable, never clear.
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
    match record.state.as_str() {
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
        }),
    }
}

/// The machine-wide incident file, next to the registry in the agents home.
pub fn fleet_stop_path(home: &crate::paths::AgentsHome) -> PathBuf {
    home.fleet_stop_json()
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

fn write_transition(
    path: &Path,
    state: &str,
    reason: Option<&str>,
    by: Option<&str>,
    holds: Vec<String>,
) -> Result<IncidentRecord, String> {
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
    let out = write_transition_locked(path, state, reason, by, holds);
    let _ = lock.unlock();
    out
}

fn write_transition_locked(
    path: &Path,
    state: &str,
    reason: Option<&str>,
    by: Option<&str>,
    holds: Vec<String>,
) -> Result<IncidentRecord, String> {
    let reason = require_reason(reason)?;
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
    };
    write_record(path, &record)?;
    Ok(record)
}

/// Parse a `--hold` value: comma-separated scope words, resolved to
/// [`SCOPES`] order with duplicates dropped. An unknown word refuses by
/// name, so a typo can never arm a scope the operator did not mean.
fn parse_hold_list(value: &str) -> Result<Vec<String>, String> {
    let mut held: Vec<String> = Vec::new();
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
        if !held.iter().any(|s| s == word) {
            held.push(word.to_string());
        }
    }
    Ok(held)
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
        })
    );
}

fn print_usage() {
    eprintln!(
        "usage: fno-agents fleet-incident stop --reason <text> [--hold spawns,tests,merges] | clear --reason <text> | status [--json] | check [--scope spawns|tests|merges] [--json]"
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

/// Binary entry: `fleet-incident stop|clear|status|check`.
pub fn run_fleet_incident(args: &[String]) -> i32 {
    let Some(action) = args.first() else {
        print_usage();
        return 2;
    };
    let rest = &args[1..];
    match action.as_str() {
        "stop" | "clear" => {
            let mut reason: Option<String> = None;
            let mut by: Option<String> = None;
            let mut holds: Option<Vec<String>> = None;
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
                    other => {
                        eprintln!("fleet-incident: unrecognized argument {other:?}");
                        return 2;
                    }
                }
            }
            // A stop without --hold holds every scope; a re-stop with a new
            // list is how the reach changes, and it bumps the generation. A
            // clear writes no scope: the state word already says nothing is
            // held.
            let held = if action == "stop" {
                match holds {
                    Some(h) if !h.is_empty() => h,
                    _ => SCOPES.iter().map(|s| s.to_string()).collect(),
                }
            } else {
                Vec::new()
            };
            let path = fleet_stop_path(&crate::paths::AgentsHome::from_env());
            match write_transition(
                &path,
                if action == "stop" { "stopped" } else { "clear" },
                reason.as_deref(),
                by.as_deref(),
                held,
            ) {
                Ok(record) => {
                    print_receipt(&record);
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
            let path = fleet_stop_path(&crate::paths::AgentsHome::from_env());
            match read_at(&path) {
                Verdict::Clear(record) | Verdict::Stopped(record) => {
                    if as_json {
                        // The reach rides the record as typed fields, so a
                        // reader of the JSON sees what is held without
                        // parsing the reason text.
                        let mut body =
                            serde_json::to_value(&record).unwrap_or_else(|_| serde_json::json!({}));
                        if let Some(obj) = body.as_object_mut() {
                            let (holds, admits) = reach(&record);
                            obj.insert("holds".into(), serde_json::json!(holds));
                            obj.insert("admits".into(), serde_json::json!(admits));
                        }
                        println!("{body}");
                    } else {
                        println!(
                            "fleet incident: {} (generation {})",
                            record.state, record.generation
                        );
                        if !record.reason.is_empty() {
                            println!("reason: {}", record.reason);
                        }
                        if record.state == "stopped" {
                            let (holds, admits) = reach(&record);
                            println!("holds: {}; admits: {}", holds.join(", "), admits.join(", "));
                        }
                        if !record.changed_by.is_empty() {
                            println!("changed by {} at {}", record.changed_by, record.changed_at);
                        }
                    }
                    if record.state == "stopped" {
                        1
                    } else {
                        0
                    }
                }
                Verdict::Unavailable(detail) => {
                    if as_json {
                        println!(
                            "{}",
                            serde_json::json!({"state": "unavailable", "detail": detail})
                        );
                    } else {
                        eprintln!("fleet incident state UNAVAILABLE: {detail}");
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
            let mut as_json = false;
            let mut scope: &str = "spawns";
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--json" | "-J" => as_json = true,
                    "--scope" => match rest.get(i + 1) {
                        Some(v) if SCOPES.contains(&v.as_str()) => scope = v.as_str(),
                        Some(other) => {
                            eprintln!(
                                "fleet-incident: unknown check scope {other:?} (valid: {})",
                                SCOPES.join(", ")
                            );
                            return 2;
                        }
                        None => {
                            eprintln!("fleet-incident: --scope needs a value");
                            return 2;
                        }
                    },
                    other => {
                        eprintln!("fleet-incident: unrecognized argument {other:?}");
                        return 2;
                    }
                }
                i += 1;
            }
            let v = verdict_for(scope);
            if as_json {
                let (state, generation, reason) = match &v {
                    Verdict::Clear(r) => ("clear", Some(r.generation), r.reason.clone()),
                    Verdict::Stopped(r) => ("stopped", Some(r.generation), r.reason.clone()),
                    Verdict::Unavailable(d) => ("unavailable", None, d.clone()),
                };
                // The reach answers for the readable record; an unreadable
                // one fails closed, so it holds every scope and admits none.
                let (holds, admits) = match &v {
                    Verdict::Clear(r) | Verdict::Stopped(r) => reach(r),
                    Verdict::Unavailable(_) => {
                        (SCOPES.iter().map(|s| s.to_string()).collect(), Vec::new())
                    }
                };
                println!(
                    "{}",
                    serde_json::json!({
                        "state": state,
                        "generation": generation,
                        "reason": reason,
                        "scope": scope,
                        "holds": holds,
                        "admits": admits,
                    })
                );
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
            super::parse_hold_list("merges, tests,merges").unwrap(),
            vec!["merges".to_string(), "tests".to_string()]
        );
        assert_eq!(super::parse_hold_list("").unwrap(), Vec::<String>::new());
        let err = super::parse_hold_list("spawns,mergess").expect_err("typo must refuse");
        assert!(err.contains("mergess") && err.contains("spawns, tests, merges"));
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
                // The pre-scope reach: spawns and tests held, merges admitted.
                assert_eq!(
                    r.held_scopes(),
                    vec!["spawns".to_string(), "tests".to_string()]
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
                assert_eq!(admits, vec!["spawns".to_string(), "merges".to_string()]);
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
