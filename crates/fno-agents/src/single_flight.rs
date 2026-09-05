//! One in flight per identical `fno` invocation.
//!
//! Measured on the operator's box: seven concurrent `fno agents truth` children
//! from five different parents inside fifteen seconds, several carrying the same
//! handle list. Each one pays a 1-to-2.4 second Python cold start, so a slow
//! roster read makes the next scheduled read overlap it, which makes the read
//! slower, which makes more of them overlap. The one-minute load average climbed
//! to 319 against a ceiling of 96 while that loop wound up.
//!
//! The remedy is a latch, not a faster reader. An in-flight invocation for a
//! given argument set is JOINED, never duplicated.
//!
//! Two things this builds on, both already in the crate in a narrower scope:
//!
//! - [`crate::claims`] is a complete cross-process claim (atomic lockfile, pid
//!   probe, TTL classify). The latch is built on it, so the machine keeps ONE
//!   locking discipline.
//! - `daemon.rs`'s `gc_in_flight` is an `AtomicBool` one-in-flight gate whose
//!   own comment records this exact feedback loop, fixed for one process. This
//!   widens the same rule from the process to the machine.
//!
//! **The key is the normalized argv, never the verb.** Three `fno do pr wait`
//! children measured live were PRs 1463, 1462 and 1394, from three different
//! sessions: distinct arguments, distinct work. A latch keyed on the verb would
//! have wedged three unrelated sessions.
//!
//! **A latch that can wedge a caller is worse than the fan-out it prevents.** A
//! join that exhausts its budget spawns anyway and says so. The failure mode is
//! one extra child and a named event, never a hang.
//!
//! ## Callers not yet on it, and the one that needs care
//!
//! Three spawn sites ride this today: both `truth_probe` sites and the
//! `discovered-json` helper in `bin/client.rs`. The other `Command::new("fno")`
//! sites in this crate are unported because porting all of them at once is a
//! rewrite the evidence did not support. One of them has since been measured,
//! so it is named rather than left to be rediscovered.
//!
//! `loopcheck::current_law_status` shells `fno backlog decisions <subject>
//! --lane law --state live --json`. Measured 2026-09-05 at load 137: FOUR
//! concurrent children with byte-identical argv from one parent. Its standing
//! subject is a constant, so every loopcheck on the machine spawns the same
//! child, and the fleet runs loopcheck on every stop hook.
//!
//! It is not ported here because it is the stop gate, and the same function
//! also reads the SCOPED per-head subject that `fno do pr coverage-waive`
//! writes. An operator who waives and immediately re-runs expects the next read
//! to see it, and a cached answer delays that by up to the TTL. Port the
//! standing subject, leave the scoped one uncached, and prove the waive-then-
//! rerun path in the same change.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::claims;

/// How long a written answer counts as fresh. Matches the Pydantic default for
/// `agents.single_flight_ttl_seconds`.
pub const DEFAULT_TTL_SECONDS: u64 = 10;

/// How long a joiner waits for the holder's answer before spawning its own.
/// Matches `agents.single_flight_join_budget_seconds`. Comfortably over the
/// 23.2 s worst-measured roster read, so a loaded box joins instead of timing
/// out - the load is exactly when the latch has to hold.
pub const DEFAULT_JOIN_BUDGET_SECONDS: u64 = 30;

/// Gap between record reads while joining. The thing being waited on takes
/// seconds, so a tighter poll would only burn the CPU this module exists to
/// give back.
const JOIN_POLL: Duration = Duration::from_millis(100);

/// The event every path emits, including the ones that spawn.
pub const GATE_EVENT: &str = "single_flight_gate";

/// What the latch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlightKind {
    /// Held the claim and ran the work.
    Spawn,
    /// A record younger than the TTL answered; nothing ran.
    Cache,
    /// Someone else was in flight and their answer arrived.
    Join,
    /// Someone else was in flight and their answer never arrived inside the
    /// budget, so this caller ran its own.
    Timeout,
}

impl FlightKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FlightKind::Spawn => "spawn",
            FlightKind::Cache => "cache",
            FlightKind::Join => "join",
            FlightKind::Timeout => "timeout",
        }
    }
}

/// One trip through the latch.
#[derive(Debug)]
pub struct Flight {
    pub kind: FlightKind,
    /// The child's stdout, from whichever of the four paths produced it.
    /// `None` when the work itself did not answer.
    pub stdout: Option<Vec<u8>>,
    pub waited_ms: u64,
}

/// Build the flight key for an invocation from its argv.
///
/// A token carrying commas is a LIST: it is split, trimmed, emptied, sorted and
/// rejoined, so `a,b` and `b,a` are one flight. Every other token is compared
/// verbatim, so `do pr wait 1463` and `do pr wait 1462` stay two.
pub fn flight_key<S: AsRef<str>>(argv: &[S]) -> String {
    let mut out = String::from("flight:");
    for (i, token) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&normalize_token(token.as_ref()));
    }
    out
}

fn normalize_token(token: &str) -> String {
    if !token.contains(',') {
        return token.to_string();
    }
    let mut parts: Vec<&str> = token
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    parts.sort_unstable();
    parts.dedup();
    parts.join(",")
}

/// Run `work` once per `key`, joining any run already in flight.
///
/// `ttl` is how long a written answer stays fresh. `budget` bounds the join AND
/// serves as the claim's TTL: a run may legitimately outlive the freshness
/// window (a roster read was measured at 23.2 s), and a claim that expired
/// mid-flight would let the next caller reclaim it as stale and spawn the very
/// duplicate this exists to delete.
///
/// `work` returns the child's stdout, or `None` when it did not answer. Its own
/// retry policy belongs INSIDE it: a crashed run that retries while still
/// holding the claim hands the retried answer to a joiner, where a retry outside
/// the flight would have the joiner start a second run of its own.
pub fn run_or_join<F>(key: &str, ttl: Duration, budget: Duration, work: F) -> Flight
where
    F: FnOnce() -> Option<Vec<u8>>,
{
    run_or_join_at(None, key, ttl, budget, work)
}

/// [`run_or_join`] against an explicit claims root.
///
/// The root is resolved ONCE here and handed to the claim, rather than read
/// from the environment twice. A test can pin it without touching process env,
/// which this crate's threaded test binary shares with every other test in it.
pub fn run_or_join_at<F>(
    root: Option<&Path>,
    key: &str,
    ttl: Duration,
    budget: Duration,
    work: F,
) -> Flight
where
    F: FnOnce() -> Option<Vec<u8>>,
{
    let started = Instant::now();
    let root = match root
        .map(Path::to_path_buf)
        .or_else(claims::global_claims_root)
    {
        Some(r) => r,
        None => {
            return finish(
                key,
                FlightKind::Spawn,
                work(),
                started,
                Some("no-flight-root"),
            )
        }
    };
    let path = record_path(&root, key);

    if let Some(bytes) = read_record(&path, Some(ttl), None) {
        return finish(key, FlightKind::Cache, Some(bytes), started, None);
    }

    let holder = holder();
    let opts = claims::AcquireOpts {
        // Floored at the claim layer's own minimum, which is a minute - well
        // over any join budget. That floor costs nothing here: a joiner never
        // waits past `budget` regardless, and a holder that DIES is reclaimed
        // on the pid probe rather than on the TTL, so the long lease only ever
        // covers a holder that is still running.
        ttl_ms: Some((budget.as_millis() as i64).max(claims::MIN_TTL_MS)),
        reason: Some("single-flight".into()),
        root: Some(root.clone()),
        ..Default::default()
    };
    match claims::acquire(key, &holder, opts) {
        claims::AcquireOutcome::Acquired(_) => {
            let out = work();
            if let Some(bytes) = out.as_deref() {
                write_record(&path, bytes);
            }
            let _ = claims::release(key, &holder, Some(&root), None);
            finish(key, FlightKind::Spawn, out, started, None)
        }
        claims::AcquireOutcome::HeldByOther { .. } => {
            let joined_at = claims::now_ms();
            while started.elapsed() < budget {
                std::thread::sleep(JOIN_POLL);
                if let Some(bytes) = read_record(&path, None, Some(joined_at)) {
                    return finish(key, FlightKind::Join, Some(bytes), started, None);
                }
            }
            finish(key, FlightKind::Timeout, work(), started, None)
        }
        // An unreadable claim is not evidence that nobody is in flight, but
        // refusing to answer is worse than one extra child. Run, and name it.
        claims::AcquireOutcome::Error(e) => finish(
            key,
            FlightKind::Spawn,
            work(),
            started,
            Some(&format!("claim-error: {e}")),
        ),
    }
}

/// `<claims root>/.fno/flight/<encoded key>.json`, beside the claims dir the
/// latch already locks in.
fn record_path(root: &Path, key: &str) -> PathBuf {
    root.join(".fno/flight")
        .join(format!("{}.json", claims::encode_key(key)))
}

/// Remove flight records nothing can still read.
///
/// A record only ever answers inside its TTL, and the longest anyone waits on
/// one is the join budget, so `ttl + budget` is the point past which no reader
/// exists. It is doubled for margin against a clock that moved.
///
/// This is not tidiness. The record dir is keyed by argv, and the roster's
/// handle set changes every time a worker comes or goes, so without a prune the
/// machine slowly fills with files answering questions nobody asks any more -
/// the shape this whole module exists to delete.
pub fn prune_records(root: Option<&Path>, ttl: Duration, budget: Duration) -> usize {
    let Some(root) = root
        .map(Path::to_path_buf)
        .or_else(claims::global_claims_root)
    else {
        return 0;
    };
    let horizon = (ttl + budget) * 2;
    let Ok(entries) = std::fs::read_dir(root.join(".fno/flight")) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|age| age > horizon).unwrap_or(false))
            .unwrap_or(false);
        if too_old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// A holder string unique to ONE flight, not one process.
///
/// `claims::acquire` treats an identical holder as an idempotent re-acquire, so
/// a pid-only holder let every thread in one process straight through - and the
/// daemon runs its sweeps on `spawn_blocking` threads, which is exactly where
/// two overlapping roster reads live. The sequence makes the second thread a
/// joiner, the same as a second process.
fn holder() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "single-flight:{}:{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Read the record when it satisfies the caller's freshness question.
///
/// `max_age` answers "is there an answer worth reusing". `written_after`
/// answers "has the holder written since I started waiting" - the joiner's
/// question, which a record left over from a previous flight must not satisfy.
fn read_record(
    path: &Path,
    max_age: Option<Duration>,
    written_after: Option<i64>,
) -> Option<Vec<u8>> {
    let raw = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let written_at = value.get("written_at_ms")?.as_i64()?;
    if let Some(age) = max_age {
        if claims::now_ms().saturating_sub(written_at) > age.as_millis() as i64 {
            return None;
        }
    }
    if let Some(floor) = written_after {
        if written_at < floor {
            return None;
        }
    }
    Some(value.get("stdout")?.as_str()?.as_bytes().to_vec())
}

/// Tempfile then rename in the same directory, so a joiner polling the record
/// never reads a half-written one.
///
/// Non-UTF-8 stdout is NOT cached: a lossy round-trip would hand a joiner bytes
/// the child never wrote. Its caller still gets the real answer; only the
/// sharing is skipped.
fn write_record(path: &Path, stdout: &[u8]) {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return;
    };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let record = json!({"written_at_ms": claims::now_ms(), "stdout": text});
    let tmp = dir.join(format!(".{}.{}.tmp", file_stem(path), std::process::id()));
    let write = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(record.to_string().as_bytes())
            .and_then(|_| f.sync_all())
    });
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "flight".into())
}

/// Emit the gate event and hand the outcome back. Every path goes through here,
/// including the failure paths: a reader cannot tell a latch that deduped
/// nothing from a latch that never ran unless both say so.
fn finish(
    key: &str,
    kind: FlightKind,
    stdout: Option<Vec<u8>>,
    started: Instant,
    note: Option<&str>,
) -> Flight {
    let waited_ms = started.elapsed().as_millis() as u64;
    let mut data = Map::new();
    data.insert("key_hash".into(), json!(key_hash(key)));
    data.insert("outcome".into(), json!(kind.as_str()));
    data.insert("waited_ms".into(), json!(waited_ms));
    data.insert("answered".into(), json!(stdout.is_some()));
    if let Some(note) = note {
        data.insert("note".into(), json!(note));
    }
    claims::emit_audit_event(None, GATE_EVENT, data);
    Flight {
        kind,
        stdout,
        waited_ms,
    }
}

/// The key, hashed. The full argv can carry a session id or a path, and the
/// event line is capped; the hash is enough to correlate two callers.
fn key_hash(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex()[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A private claims root per test, passed explicitly.
    ///
    /// Never through `FNO_CLAIMS_ROOT`: it is process-global and every test in
    /// this binary shares it, so a `set_var` here reads whichever test wrote
    /// last. Pinning the root as an argument is why these assertions hold under
    /// the full threaded suite and not only when run alone.
    fn root_for(name: &str) -> (PathBuf, String) {
        let dir =
            std::env::temp_dir().join(format!("fno-single-flight-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (dir, format!("flight:{name}"))
    }

    // AC5: a comma-separated list is one flight regardless of order; distinct
    // scalar arguments stay distinct.
    #[test]
    fn handle_lists_normalize_and_scalars_do_not() {
        assert_eq!(
            flight_key(&["agents", "truth", "--handles", "a,b"]),
            flight_key(&["agents", "truth", "--handles", "b,a"])
        );
        assert_ne!(
            flight_key(&["do", "pr", "wait", "1463"]),
            flight_key(&["do", "pr", "wait", "1462"])
        );
        assert_eq!(
            flight_key(&["agents", "truth", "--handles", "b, a ,b"]),
            flight_key(&["agents", "truth", "--handles", "a,b"])
        );
    }

    // AC1: no record -> spawn once, write the record, release.
    #[test]
    fn first_caller_spawns_and_writes_the_record() {
        let (root, key) = root_for("ac1");
        let flight = run_or_join_at(
            Some(&root),
            &key,
            Duration::from_secs(10),
            Duration::from_secs(5),
            || Some(b"{\"ok\":1}".to_vec()),
        );
        assert_eq!(flight.kind, FlightKind::Spawn);
        assert_eq!(flight.stdout.as_deref(), Some(&b"{\"ok\":1}"[..]));
        assert!(record_path(&root, &key).exists());
        // Released, so the next caller is free to acquire.
        assert_eq!(
            claims::status(&key, Some(&root)).0,
            claims::ClaimState::Free
        );
    }

    // A record past every reader's horizon is dead weight; one inside it is
    // still somebody's answer.
    #[test]
    fn pruning_removes_only_records_no_reader_can_still_use() {
        let (root, key) = root_for("prune");
        let path = record_path(&root, &key);
        write_record(&path, b"fresh");
        assert_eq!(
            prune_records(
                Some(&root),
                Duration::from_secs(10),
                Duration::from_secs(30)
            ),
            0
        );
        assert!(path.exists());
        // A zero horizon makes every record older than every reader.
        assert_eq!(
            prune_records(Some(&root), Duration::from_secs(0), Duration::from_secs(0)),
            1
        );
        assert!(!path.exists());
    }

    // AC2: a fresh record answers and nothing runs.
    #[test]
    fn second_caller_inside_the_ttl_reads_the_cache() {
        let (root, key) = root_for("ac2");
        let runs = Arc::new(AtomicUsize::new(0));
        let ttl = Duration::from_secs(10);
        for _ in 0..2 {
            let runs = Arc::clone(&runs);
            run_or_join_at(Some(&root), &key, ttl, Duration::from_secs(5), move || {
                runs.fetch_add(1, Ordering::SeqCst);
                Some(b"answer".to_vec())
            });
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    // AC2, the other half: a record older than the TTL is not an answer.
    #[test]
    fn a_stale_record_does_not_answer() {
        let (root, key) = root_for("ac2-stale");
        let path = record_path(&root, &key);
        write_record(&path, b"old");
        assert!(read_record(&path, Some(Duration::from_secs(10)), None).is_some());
        assert!(read_record(&path, Some(Duration::from_millis(0)), None).is_none());
    }

    // AC3: a second caller arriving while a flight is held waits for the
    // holder's answer instead of starting one.
    #[test]
    fn a_joiner_waits_for_the_holders_answer() {
        let (root, key) = root_for("ac3");
        let path = record_path(&root, &key);
        // Hold the claim from a foreign holder, then answer from a thread.
        claims::acquire(
            &key,
            "foreign-holder",
            claims::AcquireOpts {
                ttl_ms: Some(120_000),
                root: Some(root.clone()),
                ..Default::default()
            },
        );
        let writer_path = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            write_record(&writer_path, b"holder-answer");
        });
        let flight = run_or_join_at(
            Some(&root),
            &key,
            Duration::from_secs(10),
            Duration::from_secs(5),
            || panic!("a joiner must not run the work"),
        );
        assert_eq!(flight.kind, FlightKind::Join);
        assert_eq!(flight.stdout.as_deref(), Some(&b"holder-answer"[..]));
        assert!(flight.waited_ms > 0);
        let _ = claims::release(&key, "foreign-holder", Some(&root), None);
    }

    // AC4: a held flight whose record never advances costs one extra child and
    // a named outcome, never a hang.
    #[test]
    fn an_exhausted_join_budget_runs_anyway() {
        let (root, key) = root_for("ac4");
        claims::acquire(
            &key,
            "foreign-holder",
            claims::AcquireOpts {
                ttl_ms: Some(120_000),
                root: Some(root.clone()),
                ..Default::default()
            },
        );
        let flight = run_or_join_at(
            Some(&root),
            &key,
            Duration::from_secs(10),
            Duration::from_millis(300),
            || Some(b"own-answer".to_vec()),
        );
        assert_eq!(flight.kind, FlightKind::Timeout);
        assert_eq!(flight.stdout.as_deref(), Some(&b"own-answer"[..]));
        let _ = claims::release(&key, "foreign-holder", Some(&root), None);
    }

    // A record left by a PREVIOUS flight must not satisfy a joiner: it would
    // hand back an answer to a question nobody asked in this round.
    #[test]
    fn a_joiner_ignores_a_record_written_before_it_arrived() {
        let (root, key) = root_for("join-floor");
        let path = record_path(&root, &key);
        write_record(&path, b"previous");
        let floor = claims::now_ms() + 1;
        assert!(read_record(&path, None, Some(floor)).is_none());
    }

    // Non-UTF-8 stdout is not shareable, so it is not cached. The caller still
    // gets its own answer; only the sharing is skipped.
    #[test]
    fn non_utf8_stdout_is_not_cached() {
        let (root, key) = root_for("binary");
        let path = record_path(&root, &key);
        write_record(&path, &[0xff, 0xfe]);
        assert!(!path.exists());
    }
}
