//! Durable owner claims for one incident fix : the second half of the
//! 2026-10-04 graph-outage lesson, where three PRs fixed one bug because no
//! surface could answer "who owns this fix".
//!
//! During an outage the graph cannot answer that question. The claim lives
//! in the agents home (`fleet-claims.d/<incident>.json`), a plain file the
//! outage cannot take down, and the announcement rides the fleet-incident
//! channel - the mail lane that kept delivering while the graph was down -
//! so every lead and every worker on the incident sees the owner. A second
//! claim refuses and names the first; the owner releases the claim by
//! deleting the file the refusal names.
//!
//! Binary verb `fno-agents fleet-incident claim <incident> --pr <n>`, one
//! arm beside the breaker arms in [`crate::fleet_incident`]; the public
//! surface is the same thin Python adapter, `fno agents incident claim`.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// The only schema version this reader understands.
pub const CLAIM_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncidentClaim {
    pub version: u32,
    pub incident: String,
    pub pr: u64,
    pub claimed_by: String,
    /// RFC 3339 UTC.
    pub claimed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One claim per incident, one file per claim: the fleet-stop.d shape, so
/// two claims lock two files instead of one store.
fn claim_path(home: &crate::paths::AgentsHome, incident: &str) -> Result<PathBuf, String> {
    if !crate::fleet_incident::safe_target_value(incident) {
        return Err(format!("unsafe incident name {incident:?}"));
    }
    Ok(home.fleet_claims_dir().join(format!("{incident}.json")))
}

fn utc_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Absent is `Ok(None)`; present-but-unreadable is an error, because
/// silently replacing a claim we could not read would take the incident
/// from its owner.
fn read_claim(path: &Path) -> Result<Option<IncidentClaim>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("existing claim is unreadable: {e}")),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("existing claim is unparseable ({e}); remove it by hand to clear it"))
}

/// Temp-file write plus atomic rename in the destination directory, 0600:
/// the record carries who owns the fix.
fn write_claim(path: &Path, claim: &IncidentClaim) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let body = serde_json::to_string_pretty(claim).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".incident-claim.tmp-{}", std::process::id()));
    {
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

#[derive(Debug)]
pub(crate) struct ClaimReceipt {
    claim: IncidentClaim,
    file: PathBuf,
    announced: Option<String>,
    announce_error: Option<String>,
}

/// The whole verb in one function: refuse a second claim, else write the
/// durable record and announce the owner on the fleet-incident channel.
/// Read-modify-write under an exclusive sidecar lock, so two concurrent
/// claims cannot both see an empty slot; the announce failure is visible in
/// the receipt, never a rollback - ownership is the file, not the mail.
pub(crate) fn claim_incident(
    home: &crate::paths::AgentsHome,
    incident: &str,
    pr: u64,
    by: Option<&str>,
    note: Option<&str>,
) -> Result<ClaimReceipt, String> {
    let path = claim_path(home, incident)?;
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("cannot lock {}: {e}", lock_path.display()))?;
    let out = claim_incident_locked(&path, incident, pr, by, note);
    let _ = lock.unlock();
    out
}

fn claim_incident_locked(
    path: &Path,
    incident: &str,
    pr: u64,
    by: Option<&str>,
    note: Option<&str>,
) -> Result<ClaimReceipt, String> {
    if let Some(existing) = read_claim(path)? {
        return Err(format!(
            "incident {incident:?} is already claimed by {} (PR {}, claimed at {}); a second claim cannot take it - the owner releases it by deleting {}",
            existing.claimed_by, existing.pr, existing.claimed_at, path.display()
        ));
    }
    let claim = IncidentClaim {
        version: CLAIM_VERSION,
        incident: incident.to_string(),
        pr,
        claimed_by: crate::fleet_incident::attributed_caller(by),
        claimed_at: utc_now(),
        note: note.map(str::to_string),
    };
    write_claim(path, &claim)?;
    let mut body = format!(
        "incident fix {incident:?} claimed by {}: PR {pr}. One owner per incident; a second claim refuses and names this one. The owner releases it by deleting {}.",
        claim.claimed_by,
        path.display()
    );
    if let Some(note) = note {
        body.push_str(&format!(" Note: {note}"));
    }
    let (announced, announce_error) = match crate::announce::announce_all(
        "fno/fleet-incident",
        &format!("claim {incident}"),
        &body,
    ) {
        Ok(id) => (Some(id), None),
        Err(error) => {
            eprintln!("fleet-incident claim: announce failed: {error}");
            (None, Some(error))
        }
    };
    Ok(ClaimReceipt {
        claim,
        file: path.to_path_buf(),
        announced,
        announce_error,
    })
}

/// Strict flag parse: one positional incident name, a required positive
/// `--pr`, optional `--by` and `--note`. Unknown flags refuse with usage so
/// a typo can never write a claim under a misread name.
pub(crate) fn run_claim(args: &[String]) -> i32 {
    let mut incident: Option<String> = None;
    let mut pr: Option<u64> = None;
    let mut by: Option<String> = None;
    let mut note: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--pr" | "--by" | "--note" => {
                let Some(value) = args.get(i + 1) else {
                    eprintln!("fleet-incident claim: {flag} needs a value");
                    return 2;
                };
                match flag {
                    "--pr" => match value.parse::<u64>() {
                        Ok(parsed) if parsed > 0 => pr = Some(parsed),
                        _ => {
                            eprintln!("fleet-incident claim: --pr must be a positive PR number, got {value:?}");
                            return 2;
                        }
                    },
                    "--by" => by = Some(value.clone()),
                    "--note" => note = Some(value.clone()),
                    _ => unreachable!(),
                }
                i += 2;
            }
            other if other.starts_with('-') => {
                eprintln!("fleet-incident claim: unrecognized argument {other:?}");
                return 2;
            }
            other => {
                if incident.is_some() {
                    eprintln!("fleet-incident claim: one incident per claim (got {incident:?} and {other:?})");
                    return 2;
                }
                incident = Some(other.to_string());
                i += 1;
            }
        }
    }
    let Some(incident) = incident else {
        eprintln!("usage: fno-agents fleet-incident claim <incident> --pr <n> [--by <who>] [--note <text>]");
        return 2;
    };
    let Some(pr) = pr else {
        eprintln!("fleet-incident claim: --pr <n> is required: the claim names the PR that carries the fix");
        return 2;
    };
    let home = crate::paths::AgentsHome::from_env();
    match claim_incident(&home, &incident, pr, by.as_deref(), note.as_deref()) {
        Ok(receipt) => {
            println!(
                "{}",
                serde_json::json!({
                    "claimed": receipt.claim.incident,
                    "pr": receipt.claim.pr,
                    "by": receipt.claim.claimed_by,
                    "at": receipt.claim.claimed_at,
                    "file": receipt.file.display().to_string(),
                    "announced": receipt.announced,
                    "announce_error": receipt.announce_error,
                })
            );
            0
        }
        Err(message) => {
            eprintln!("fleet-incident claim refused: {message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentsHomeEnvGuard;

    fn fixture() -> (tempfile::TempDir, crate::paths::AgentsHome) {
        let dir = tempfile::TempDir::new().unwrap();
        let home = crate::paths::AgentsHome::at(dir.path().join("agents"));
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::create_dir_all(dir.path().join("bus")).unwrap();
        (dir, home)
    }

    fn live_registry(home: &crate::paths::AgentsHome) {
        let row = serde_json::json!({
            "name": "quill", "harness": "claude", "status": "live",
            "harness_session_id": "aaaa1111-1111-1111-1111-111111111111",
            "short_id": "aaaa1111",
            "cwd": "/tmp/quill", "log_path": "/tmp/quill.log",
        });
        std::fs::write(
            home.registry_json(),
            serde_json::to_string(&serde_json::json!({"schema_version": 1, "agents": [row]}))
                .unwrap(),
        )
        .unwrap();
    }

    fn bus_lines(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("bus").join("messages.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_claim_verb_names_one_owner_end_to_end() {
        let (dir, home) = fixture();
        let _guard = AgentsHomeEnvGuard::set(home.root());
        live_registry(&home);

        // Refusals that write nothing come first: the strict parse and the
        // unsafe incident name both refuse before any state exists.
        assert_eq!(run_claim(&args(&["graph-outage"])), 2, "--pr is required");
        assert_eq!(run_claim(&args(&["--pr", "0", "graph-outage"])), 2);
        assert_eq!(
            run_claim(&args(&["--pr", "12", "a", "b"])),
            2,
            "one incident per claim"
        );
        assert_eq!(run_claim(&args(&["--bogus", "x"])), 2);
        let error = claim_incident(&home, "../escape", 3050, None, None).unwrap_err();
        assert!(error.contains("unsafe"), "{error}");
        assert!(
            !home.fleet_claims_dir().exists(),
            "a refused name creates no state"
        );

        // The first claim writes the durable record and announces the owner
        // on the fleet-incident channel, so every lead and worker sees it.
        let first =
            claim_incident(&home, "graph-outage", 3050, Some("quill"), None).expect("claims");
        assert_eq!(first.claim.pr, 3050);
        assert_eq!(first.claim.claimed_by, "quill");
        let on_disk: IncidentClaim =
            serde_json::from_slice(&std::fs::read(&first.file).unwrap()).unwrap();
        assert_eq!(on_disk, first.claim, "the file is the ownership truth");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&first.file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let lines = bus_lines(dir.path());
        assert_eq!(lines.len(), 1, "one claim is one bus line: {lines:?}");
        assert_eq!(lines[0]["from"], "fno/fleet-incident");
        assert_eq!(lines[0]["to"], "fleet:all");
        assert_eq!(lines[0]["meta"]["subject"], "claim graph-outage");
        let body = lines[0]["body"].as_str().unwrap();
        assert!(body.contains("quill") && body.contains("3050"), "{body}");
        assert_eq!(
            first.announced,
            Some(lines[0]["id"].as_str().unwrap().to_string())
        );

        // A second claim refuses and names the owner; nothing is written.
        let before = std::fs::read(&first.file).unwrap();
        let error = claim_incident(&home, "graph-outage", 3051, Some("finch"), None).unwrap_err();
        assert!(error.contains("quill"), "names the owner: {error}");
        assert!(error.contains("3050"), "names the owner's PR: {error}");
        assert!(error.contains("deleting"), "names the release: {error}");
        assert_eq!(
            std::fs::read(&first.file).unwrap(),
            before,
            "a refused claim writes nothing"
        );

        // An unreadable claim refuses instead of overwriting the evidence.
        std::fs::write(&first.file, b"garbage").unwrap();
        let error = claim_incident(&home, "graph-outage", 3050, None, None).unwrap_err();
        assert!(error.contains("unparseable"), "{error}");
        assert_eq!(std::fs::read(&first.file).unwrap(), b"garbage");
    }
}
