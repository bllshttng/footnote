//! The backlog one-in-flight gate: binary-direct `flight-acquire`/`flight-release`.
//!
//! The Python backlog verbs carry the gate; this module is the lock itself,
//! natively, so the compatibility shell does not grow it. Three properties
//! the shim depends on:
//!
//! - A dead holder is reclaimed on the pid probe, never on the TTL. The
//!   holder is a one-shot subprocess; a killed run must not hold the scope
//!   shut for thirty minutes. The drop is race-safe without a mutex because
//!   `claims::release` is holder-bound: it re-reads and removes only while
//!   the holder still matches, so a second waiter that saw the same corpse
//!   finds the first waiter's fresh claim under a different holder and no-ops.
//! - The held receipt carries a running request count (a `.held-requests`
//!   sidecar beside the claim), the cross-process counterpart of the reap
//!   arm's `requests=N`. The count belongs to the CURRENT holder: each
//!   acquire removes the sidecar, so `requests` reads held attempts against
//!   this acquire, never an all-time tally across holders (x-9c91 change 6).
//! - Every outcome is one JSON line on stdout and exit 0; a gate-side error
//!   is exit 3, which the shim treats as fail-open (ungated, the pre-gate
//!   behavior).

use crate::claims::{self, AcquireOutcome, PidProbe};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Twelve-minute reconcile runs are measured; 30 minutes bounds a lost
/// holder when the shim omits `--ttl-ms`.
pub const FLIGHT_TTL_MS: i64 = 30 * 60 * 1000;

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn requests_path(key: &str, root: Option<&Path>) -> Option<PathBuf> {
    let lock = claims::claim_path(key, root).ok()?;
    Some(
        lock.parent()?
            .join(format!("{}.held-requests", claims::encode_key(key))),
    )
}

/// Append one held request and return the scope's running count. Report-only:
/// a lost write reads as zero, and the read-back can over-count by one.
fn count_held_request(key: &str, root: Option<&Path>) -> u64 {
    let Some(path) = requests_path(key, root) else {
        return 0;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut fh) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(fh, "{} {}", now_ms(), std::process::id());
    }
    std::fs::read_to_string(&path)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(0)
}

fn print_receipt(value: &Value) {
    let stdout = std::io::stdout();
    let _ = writeln!(&mut stdout.lock(), "{value}");
}

/// Remove the held-attempts sidecar on a fresh acquire: the count now
/// belongs to the NEW holder and starts at zero. NotFound is the normal
/// first acquire; any other failure is swallowed because the sidecar is
/// report-only - a lost reset must never fail an acquire (x-9c91 AC6-ERR).
fn reset_held_requests(key: &str, root: Option<&Path>) {
    if let Some(path) = requests_path(key, root) {
        let _ = std::fs::remove_file(&path);
    }
}

/// `flight-acquire <key> --scope <s> --ttl-ms <n> --holder <h> [--claims-root <dir>]`
/// `            [--events-dir <dir>]`
///
/// Prints one JSON receipt: `{"acquired":true,"holder":...}` or
/// `{"acquired":false,"holder":...,"held_for_s":...,"requests":...}`.
pub fn run_flight_acquire(args: &[String]) -> i32 {
    let Some(key) = args.first().cloned() else {
        eprintln!("flight-acquire: missing key");
        return 2;
    };
    let Some(holder) = arg_value(args, "--holder") else {
        eprintln!("flight-acquire: missing --holder");
        return 2;
    };
    let ttl_ms = arg_value(args, "--ttl-ms")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(FLIGHT_TTL_MS);
    let root = arg_value(args, "--claims-root").map(PathBuf::from);
    let events_dir = arg_value(args, "--events-dir").map(PathBuf::from);
    let scope = arg_value(args, "--scope").unwrap_or_else(|| "backlog single-flight".into());
    let root_ref = root.as_deref();
    // The claim is held in the name of the CALLING process (the Python verb
    // that will do the work), never this short-lived binary: the lock must
    // die with the work, not with the messenger.
    let pid = arg_value(args, "--pid").and_then(|v| v.parse::<u32>().ok());

    let mut opts = claims::AcquireOpts {
        pid,
        ttl_ms: Some(ttl_ms),
        reason: Some(format!("backlog single-flight: {scope}")),
        root: root.clone(),
        events_dir,
        ..Default::default()
    };
    match claims::acquire(&key, &holder, opts.clone()) {
        AcquireOutcome::Acquired(rec) => {
            reset_held_requests(&key, root_ref);
            print_receipt(&json!({"acquired": true, "holder": rec.holder}));
            return 0;
        }
        AcquireOutcome::HeldByOther { .. } => {
            // A dead holder must not hold the scope shut for the TTL. The
            // release is holder-bound: it re-reads and removes only while the
            // holder still matches, so a second waiter that saw the same
            // corpse cannot drop a live replacement's claim. A new acquirer
            // racing in afterwards simply wins the fresh create and this
            // invocation reports held against it.
            let (_, Some(rec)) = claims::status(&key, root_ref) else {
                return held_receipt(&key, root_ref);
            };
            let dead = rec
                .pid
                .map(|pid| matches!(claims::probe_pid(pid), PidProbe::Absent))
                .unwrap_or(false);
            if dead {
                let _ = claims::release(&key, &rec.holder, root_ref, opts.events_dir.as_deref());
                if let AcquireOutcome::Acquired(rec) = claims::acquire(&key, &holder, opts) {
                    reset_held_requests(&key, root_ref);
                    print_receipt(&json!({"acquired": true, "holder": rec.holder}));
                    return 0;
                }
            }
            held_receipt(&key, root_ref)
        }
        AcquireOutcome::Error(e) => {
            eprintln!("flight-acquire: {e}");
            3
        }
    }
}

fn held_receipt(key: &str, root: Option<&Path>) -> i32 {
    let (state, rec) = claims::status(key, root);
    let holder = rec
        .as_ref()
        .map(|r| r.holder.clone())
        .unwrap_or_else(|| "unknown".into());
    let held_for_s = rec
        .as_ref()
        .map(|r| ((claims::now_ms() - r.acquired_at).max(0) / 1000) as u64)
        .unwrap_or(0);
    let requests = if state == claims::ClaimState::Free {
        0
    } else {
        count_held_request(key, root)
    };
    print_receipt(&json!({
        "acquired": false,
        "holder": holder,
        "held_for_s": held_for_s,
        "requests": requests,
    }));
    0
}

/// `flight-release <key> --holder <h> [--claims-root <dir>]`
pub fn run_flight_release(args: &[String]) -> i32 {
    let Some(key) = args.first().cloned() else {
        eprintln!("flight-release: missing key");
        return 2;
    };
    let Some(holder) = arg_value(args, "--holder") else {
        eprintln!("flight-release: missing --holder");
        return 2;
    };
    let root = arg_value(args, "--claims-root").map(PathBuf::from);
    match claims::release(&key, &holder, root.as_deref(), None) {
        Ok(()) => {
            print_receipt(&json!({"released": true}));
            0
        }
        Err(e) => {
            eprintln!("flight-release: {e}");
            3
        }
    }
}

// keep the unused-import lint honest: Value is used by print_receipt callers
#[allow(dead_code)]
fn _value_witness(_: &Value) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn verb_args(key: &str, holder: &str, td: &TempDir) -> Vec<String> {
        vec![
            key.to_string(),
            "--holder".into(),
            holder.into(),
            "--claims-root".into(),
            td.path().to_string_lossy().into_owned(),
            "--ttl-ms".into(),
            FLIGHT_TTL_MS.to_string(),
        ]
    }

    #[test]
    fn held_requests_count_against_the_current_holder() {
        let td = TempDir::new().unwrap();
        let key = "flight:test-reset";
        let opts = claims::AcquireOpts {
            ttl_ms: Some(FLIGHT_TTL_MS),
            reason: Some("flight-gate test: A".into()),
            root: Some(td.path().to_path_buf()),
            ..Default::default()
        };
        assert!(matches!(
            claims::acquire(key, "A", opts),
            claims::AcquireOutcome::Acquired(_)
        ));
        // Two held attempts against A: the tally reads 2.
        assert_eq!(count_held_request(key, Some(td.path())), 1);
        assert_eq!(count_held_request(key, Some(td.path())), 2);
        claims::release(key, "A", Some(td.path()), None).unwrap();

        // B's acquire resets the sidecar: the count belongs to the new holder.
        let rc = run_flight_acquire(&verb_args(key, "B", &td));
        assert_eq!(rc, 0, "B must acquire");
        let path = requests_path(key, Some(td.path())).unwrap();
        assert!(!path.exists(), "a fresh acquire starts the count at zero");

        // One held attempt against B reads requests: 1, not 3.
        assert_eq!(count_held_request(key, Some(td.path())), 1);
    }

    #[test]
    fn an_undeletable_sidecar_never_fails_the_acquire() {
        let td = TempDir::new().unwrap();
        let key = "flight:test-stuck-sidecar";
        // A directory at the sidecar path cannot be remove_file'd: the
        // reset fails, and the acquire must still report acquired.
        let path = requests_path(key, Some(td.path())).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let rc = run_flight_acquire(&verb_args(key, "B", &td));
        assert_eq!(rc, 0, "acquired: true despite the lost reset");
    }
}
