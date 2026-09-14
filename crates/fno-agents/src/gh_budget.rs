//! One machine-wide GitHub request budget that every gh call is admitted
//! against.
//!
//! GitHub's secondary limit counts request RATE and concurrency across every
//! session on this machine's account, and before this module nothing added
//! the fleet's schedules up: each guard in the tree reacted after a refusal
//! and kept its own memory of it, so stand-downs kept spending probe
//! requests while GitHub was refusing. This module answers one question
//! BEFORE a request leaves - "may this GitHub request go out now?" - from
//! one JSON ledger every caller shares.
//!
//! The ledger lives at `~/.fno/locks/github-request-budget.json`,
//! config-independent on purpose (same directory contract as
//! `machine_locks_dir()`): a budget that protects the machine must not move
//! with a project's config. Each read-modify-write takes a `libc::flock` on
//! a `.lock` sidecar and writes through a temp file + rename, so a reader
//! never sees a torn file.
//!
//! Fail-open rule: an unreadable or corrupt ledger admits and says so in the
//! receipt. This budget protects the fleet; it is not a stop. An all-stop is
//! the operator's breaker (`fno agents incident`), and a disk fault must not
//! refuse every gh call on the machine.
//!
//! Verb surface: `fno-agents gh-budget` takes one JSON payload on stdin (the
//! `rust_binary.verb_call` door convention) and prints one JSON object. All
//! three ops exit 0; the refusal is carried in the payload, never in the
//! exit code, because `verb_call` raises on a nonzero exit.
use serde_json::{json, Value};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Half of GitHub's advertised 900 points/minute REST secondary limit.
pub const DEFAULT_POINTS_PER_MIN: u32 = 450;
const WINDOW_MS: i64 = 60_000;
const HALF_OPEN_MS: i64 = 30_000;
const ESCALATION_WINDOW_MS: i64 = 900_000;
const BASE_BACKOFF_MS: i64 = 60_000;
const MAX_BACKOFF_MS: i64 = 900_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerState {
    Ok,
    Absent,
    Unreadable,
}

fn ledger_word(state: LedgerState) -> &'static str {
    match state {
        LedgerState::Ok => "ok",
        LedgerState::Absent => "absent",
        LedgerState::Unreadable => "unreadable",
    }
}

#[derive(Debug, Clone, Default)]
struct Ledger {
    stamps: Vec<(i64, u32)>,
    backoff_until_ms: i64,
    backoff_step: i64,
    /// 0 means "never refused"; a real refusal stamp is a real epoch ms.
    last_refusal_ms: i64,
    /// 0 means "no half-open probe yet"; the first probe during a backoff
    /// always passes so recovery gets noticed instead of waited out.
    last_half_open_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub admitted: bool,
    pub cause: Option<&'static str>,
    pub retry_after_s: i64,
    pub points_60s: i64,
    pub cap: u32,
    pub ledger: LedgerState,
    pub refusal: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub points_60s: i64,
    pub cap: u32,
    pub backoff_remaining_s: i64,
    pub ledger: LedgerState,
}

pub fn ledger_path() -> PathBuf {
    // A HOME-unset context (cron, launchd) must not scatter the ledger into
    // whatever cwd invoked gh; the temp dir keeps admission working and the
    // file out of sight until HOME comes back.
    crate::agents_config::machine_locks_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("fno-locks"))
        .join("github-request-budget.json")
}

pub fn cap_from_env() -> u32 {
    std::env::var("FNO_GH_BUDGET_POINTS_PER_MIN")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_POINTS_PER_MIN)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Port of `fno.pr._quota.command_args`: drop gh-wide options so equivalent
/// command spellings share one cost.
fn command_args(args: &[String]) -> Vec<String> {
    fn strip(tokens: &[String]) -> Vec<String> {
        let mut rest = tokens.to_vec();
        while let Some(first) = rest.first() {
            if first == "-R" || first == "--repo" || first == "--hostname" {
                if rest.len() < 2 {
                    return Vec::new();
                }
                rest.drain(0..2);
            } else if (first.starts_with("-R") && first.len() > 2)
                || first.starts_with("--repo=")
                || first.starts_with("--hostname=")
            {
                rest.remove(0);
            } else {
                break;
            }
        }
        rest
    }
    let normalized = strip(args);
    if normalized.first().map(String::as_str) == Some("pr") {
        let mut out = vec!["pr".to_string()];
        out.extend(strip(&normalized[1..]));
        return out;
    }
    normalized
}

fn rest_write(cmd: &[&str]) -> bool {
    for (i, tok) in cmd.iter().enumerate() {
        let tok = *tok;
        let method = if tok == "-X" || tok == "--method" {
            cmd.get(i + 1).copied()
        } else if let Some(v) = tok.strip_prefix("--method=") {
            Some(v)
        } else if tok.len() > 2 && tok.starts_with("-X") {
            Some(&tok[2..])
        } else {
            None
        };
        if let Some(m) = method {
            return matches!(
                m.to_ascii_uppercase().as_str(),
                "POST" | "PATCH" | "PUT" | "DELETE"
            );
        }
    }
    false
}

/// Whether an `api graphql` argv carries a mutation: the query rides either
/// as the positional value after `graphql` or as `-f query=...`.
fn graphql_mutation(cmd: &[&str]) -> bool {
    let Some(pos) = cmd.iter().position(|t| *t == "graphql") else {
        return false;
    };
    let mut query: Option<String> = None;
    let mut i = pos + 1;
    while i < cmd.len() {
        let tok = cmd[i];
        let inline = tok
            .strip_prefix("--field=")
            .or_else(|| tok.strip_prefix("--raw-field="));
        if let Some(v) = inline {
            if let Some(val) = v.strip_prefix("query=") {
                query = Some(val.to_string());
            }
        } else if tok == "-f" || tok == "-F" || tok == "--field" || tok == "--raw-field" {
            if let Some(next) = cmd.get(i + 1) {
                if let Some(val) = next.strip_prefix("query=") {
                    query = Some(val.to_string());
                }
            }
            i += 1;
        } else if !tok.starts_with('-') {
            query = Some(tok.to_string());
        }
        i += 1;
    }
    query.map(is_mutation_query).unwrap_or(false)
}

/// An unreadable query (a literal `@file` argv, gh loads it client-side)
/// fails toward the WRITE cost: an unprovable read is charged as a mutation,
/// never the reverse.
fn is_mutation_query(q: String) -> bool {
    if q.trim_start().starts_with('@') {
        return true;
    }
    q.trim_start().starts_with("mutation")
}

/// The request cost of one gh argv, in GitHub's advertised points: a GET or
/// a GraphQL query is 1, a content-creating request is 5, and the exempt
/// local/exempt-endpoint shapes are 0.
pub fn points_for(argv: &[String]) -> u32 {
    // ponytail: `--paginate` sends one request per page but is charged once
    // here; if pagination bursts trip the real limit, charge per-page via a
    // last-page estimate instead of raising the cap.
    let normalized = command_args(argv);
    let cmd: Vec<&str> = normalized.iter().map(String::as_str).collect();
    match cmd.first().copied() {
        Some("--version" | "version" | "help" | "--help" | "completion" | "config" | "alias") => 0,
        Some("api") => {
            if cmd.get(1) == Some(&"rate_limit") {
                return 0; // exempt from the primary limit; the budget need not count it
            }
            if graphql_mutation(&cmd) || rest_write(&cmd) {
                5
            } else {
                1
            }
        }
        Some("pr") => match cmd.get(1).copied() {
            Some("create" | "merge" | "edit" | "comment" | "review" | "close" | "ready") => 5,
            _ => 1,
        },
        Some("issue") => match cmd.get(1).copied() {
            Some("create" | "comment") => 5,
            _ => 1,
        },
        Some("release") => match cmd.get(1).copied() {
            Some("create") => 5,
            _ => 1,
        },
        _ => 1,
    }
}

fn window_points(ledger: &Ledger, now: i64) -> i64 {
    ledger
        .stamps
        .iter()
        .filter(|(t, _)| now - *t < WINDOW_MS)
        .map(|(_, p)| *p as i64)
        .sum()
}

fn lock_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

/// flock held for the scope of one read-modify-write, following
/// `loop_king::bump_respawn_count`.
struct FileLock {
    handle: std::fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> FileLock {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let handle = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|e| panic!("gh-budget: cannot open lock {}: {e}", path.display()));
        unsafe {
            libc::flock(handle.as_raw_fd(), libc::LOCK_EX);
        }
        FileLock { handle }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.handle.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_ledger(path: &Path) -> (Ledger, LedgerState) {
    if !path.exists() {
        return (Ledger::default(), LedgerState::Absent);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return (Ledger::default(), LedgerState::Unreadable),
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (Ledger::default(), LedgerState::Unreadable);
    };
    let stamps = v
        .get("stamps")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let pair = s.as_array()?;
                    Some((pair.first()?.as_i64()?, pair.get(1)?.as_u64()? as u32))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let num = |key: &str| v.get(key).and_then(Value::as_i64).unwrap_or(0);
    (
        Ledger {
            stamps,
            backoff_until_ms: num("backoff_until_ms"),
            backoff_step: num("backoff_step"),
            last_refusal_ms: num("last_refusal_ms"),
            last_half_open_ms: num("last_half_open_ms"),
        },
        LedgerState::Ok,
    )
}

fn write_ledger(path: &Path, ledger: &Ledger, now: i64) -> Result<(), String> {
    let mut out = ledger.clone();
    out.stamps.retain(|(t, _)| now - *t < WINDOW_MS);
    let body = json!({
        "stamps": out.stamps,
        "backoff_until_ms": out.backoff_until_ms,
        "backoff_step": out.backoff_step,
        "last_refusal_ms": out.last_refusal_ms,
        "last_half_open_ms": out.last_half_open_ms,
    });
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    let tmp = dir.join(format!(".github-request-budget.tmp-{}", std::process::id()));
    std::fs::write(&tmp, body.to_string())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

fn refusal_line(verdict: &Verdict) -> String {
    // "rate limit" keeps both existing classifiers (_rest.py and
    // stderr_smells_rate_limit) reading a local refusal as secondary, so
    // every current caller backs off without new handling. "HTTP 403" must
    // stay out: it is the marker _quota.record_refusal keys on, and a local
    // refusal must never be recorded as GitHub's.
    let cause = verdict.cause.unwrap_or("budget");
    format!(
        "gh budget: fleet GitHub rate limit held locally ({cause}: {}/{} points in 60s | \
         backoff {}s left); this command did not reach GitHub. Retry after {}s. \
         Ledger: fno-agents gh-budget status",
        verdict.points_60s, verdict.cap, verdict.retry_after_s, verdict.retry_after_s
    )
}

/// May this request go out now? Takes the ledger path, the clock and the cap
/// so tests pass a temp path, a fixed clock, and a small cap.
pub fn admit(path: &Path, argv: &[String], now: i64, cap: u32) -> Verdict {
    let points = points_for(argv);
    let _lock = FileLock::acquire(&lock_path(path));
    let (mut ledger, state) = read_ledger(path);

    // Exempt shapes are admitted and write nothing: a version probe is not
    // traffic, and writing would only churn the file under every gh call.
    if points == 0 {
        return Verdict {
            admitted: true,
            cause: None,
            retry_after_s: 0,
            points_60s: window_points(&ledger, now),
            cap,
            ledger: state,
            refusal: None,
        };
    }

    if now < ledger.backoff_until_ms {
        // One request per 30s as a half-open probe, so recovery gets noticed
        // instead of waited out; the rest are refused with cause `backoff`.
        if ledger.last_half_open_ms == 0 || now - ledger.last_half_open_ms >= HALF_OPEN_MS {
            ledger.last_half_open_ms = now;
            ledger.stamps.push((now, points));
            write_ledger(path, &ledger, now).ok();
            return Verdict {
                admitted: true,
                cause: None,
                retry_after_s: 0,
                points_60s: window_points(&ledger, now),
                cap,
                ledger: state,
                refusal: None,
            };
        }
        let retry_after_s = ((ledger.backoff_until_ms - now) / 1000).max(1);
        let mut verdict = Verdict {
            admitted: false,
            cause: Some("backoff"),
            retry_after_s,
            points_60s: window_points(&ledger, now),
            cap,
            ledger: state,
            refusal: None,
        };
        verdict.refusal = Some(refusal_line(&verdict));
        return verdict;
    }

    let points_60s = window_points(&ledger, now);
    if points_60s + points as i64 > cap as i64 {
        let oldest = ledger.stamps.iter().map(|(t, _)| *t).min().unwrap_or(now);
        let retry_after_s = (((WINDOW_MS - (now - oldest)) / 1000).max(1)).min(WINDOW_MS / 1000);
        let mut verdict = Verdict {
            admitted: false,
            cause: Some("budget"),
            retry_after_s,
            points_60s,
            cap,
            ledger: state,
            refusal: None,
        };
        verdict.refusal = Some(refusal_line(&verdict));
        return verdict;
    }

    ledger.stamps.push((now, points));
    write_ledger(path, &ledger, now).ok();
    Verdict {
        admitted: true,
        cause: None,
        retry_after_s: 0,
        points_60s: points_60s + points as i64,
        cap,
        ledger: state,
        refusal: None,
    }
}

/// A GitHub refusal reached one caller. One fleet-wide backoff opens in the
/// ledger, escalating while refusals stay within 900s of each other. A no-op
/// while a backoff is already live, so two writers reporting one refusal
/// widen nothing.
pub fn record_refusal(path: &Path, now: i64) -> Result<(), String> {
    let _lock = FileLock::acquire(&lock_path(path));
    let (mut ledger, _) = read_ledger(path);
    if now < ledger.backoff_until_ms {
        return Ok(());
    }
    // 0 is the "never" sentinel, not the epoch: a first refusal must not
    // escalate off an unset field.
    let escalated =
        ledger.last_refusal_ms > 0 && now - ledger.last_refusal_ms <= ESCALATION_WINDOW_MS;
    ledger.backoff_step = if escalated {
        ledger.backoff_step + 1
    } else {
        0
    };
    let shift = ledger.backoff_step.min(4) as u32;
    let backoff = (BASE_BACKOFF_MS << shift).min(MAX_BACKOFF_MS);
    ledger.backoff_until_ms = now + backoff;
    ledger.last_refusal_ms = now;
    write_ledger(path, &ledger, now)
}

/// Read-only projection for status surfaces and the loop gate's pre-read.
pub fn snapshot(path: &Path, now: i64) -> Snapshot {
    let (ledger, state) = read_ledger(path);
    Snapshot {
        points_60s: window_points(&ledger, now),
        cap: cap_from_env(),
        backoff_remaining_s: ((ledger.backoff_until_ms - now).max(0)) / 1000,
        ledger: state,
    }
}

/// Binary entry: one JSON payload on stdin, one JSON object on stdout,
/// exit 0 for every op the payload names (the refusal rides in the payload,
/// never in the exit code - verb_call raises on a nonzero exit).
pub fn run_gh_budget(_args: &[String]) -> i32 {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        eprintln!("gh-budget: stdin read failed");
        return 2;
    }
    let payload: Value = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gh-budget: bad payload: {e}");
            return 2;
        }
    };
    let now = now_ms();
    match payload.get("op").and_then(Value::as_str) {
        Some("admit") => {
            let argv: Vec<String> = payload
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let v = admit(&ledger_path(), &argv, now, cap_from_env());
            println!(
                "{}",
                json!({
                    "verdict": if v.admitted { "admitted" } else { "refused" },
                    "cause": v.cause,
                    "retry_after_s": v.retry_after_s,
                    "points_60s": v.points_60s,
                    "cap": v.cap,
                    "ledger": ledger_word(v.ledger),
                    "refusal": v.refusal,
                })
            );
            0
        }
        Some("refused") => match record_refusal(&ledger_path(), now) {
            Ok(()) => {
                println!("{}", json!({"recorded": true}));
                0
            }
            Err(e) => {
                eprintln!("gh-budget: {e}");
                1
            }
        },
        Some("status") => {
            let s = snapshot(&ledger_path(), now);
            println!(
                "{}",
                json!({
                    "points_60s": s.points_60s,
                    "cap": s.cap,
                    "backoff_remaining_s": s.backoff_remaining_s,
                    "ledger": ledger_word(s.ledger),
                })
            );
            0
        }
        other => {
            eprintln!("gh-budget: unknown op {other:?}; expected admit, refused, or status");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn points_for_matches_the_cost_table() {
        assert_eq!(points_for(&argv(&["--version"])), 0);
        assert_eq!(points_for(&argv(&["help"])), 0);
        assert_eq!(points_for(&argv(&["api", "rate_limit"])), 0);
        assert_eq!(
            points_for(&argv(&["pr", "view", "1", "--json", "state"])),
            1
        );
        assert_eq!(points_for(&argv(&["api", "user"])), 1);
        assert_eq!(points_for(&argv(&["pr", "merge", "1"])), 5);
        assert_eq!(points_for(&argv(&["issue", "comment", "1", "-b", "x"])), 5);
        assert_eq!(
            points_for(&argv(&["api", "-X", "POST", "repos/o/r/issues"])),
            5
        );
        assert_eq!(
            points_for(&argv(&["api", "graphql", "-f", "query=mutation { x }"])),
            5
        );
        assert_eq!(
            points_for(&argv(&["api", "graphql", "-f", "query=query { x }"])),
            1
        );
        // An unreadable query fails toward the write cost.
        assert_eq!(
            points_for(&argv(&["api", "graphql", "-f", "query=@x.graphql"])),
            5
        );
        // gh-wide options never change the cost of the command word.
        assert_eq!(points_for(&argv(&["-R", "o/r", "pr", "merge", "1"])), 5);
        assert_eq!(points_for(&argv(&["--repo=o/r", "api", "user"])), 1);
    }

    #[test]
    fn admit_charges_points_and_refuses_over_the_cap() {
        // AC1-HP: three 1-point reads admitted, the fourth refused `budget`.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        let now = 1_760_000_000_000i64;
        let read = argv(&["pr", "view", "1", "--json", "state"]);
        for _ in 0..3 {
            let v = admit(&path, &read, now, 3);
            assert!(v.admitted, "expected admitted: {v:?}");
        }
        let v = admit(&path, &read, now, 3);
        assert!(!v.admitted);
        assert_eq!(v.cause, Some("budget"));
        let line = v.refusal.expect("refusal line");
        assert!(line.contains("rate limit"), "line: {line}");
        assert!(!line.contains("HTTP 403"), "line: {line}");
    }

    #[test]
    fn admit_on_a_corrupt_ledger_admits_and_rewrites_it() {
        // AC1-ERR: unreadable ledger fails open, and the next stamping write
        // replaces the junk with valid JSON.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        std::fs::write(&path, "not json").unwrap();
        let now = 1_760_000_000_000i64;
        let v = admit(&path, &argv(&["api", "user"]), now, 450);
        assert!(v.admitted);
        assert_eq!(v.ledger, LedgerState::Unreadable);
        let (ledger, state) = read_ledger(&path);
        assert_eq!(state, LedgerState::Ok);
        assert_eq!(ledger.stamps, vec![(now, 1)]);
    }

    #[test]
    fn backoff_holds_fleet_wide_with_one_half_open_probe() {
        // AC2-EDGE: a refusal at t=0 opens a 60s fleet-wide window; a second
        // refusal inside it widens nothing; the +20s admit is the half-open
        // probe; the +35s admit is refused `backoff`; a 0-point version probe
        // at +40s admits and writes nothing.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        let t0 = 1_760_000_000_000i64;
        record_refusal(&path, t0).unwrap();
        // A second caller (a different session, same machine ledger) reporting
        // the same refusal widens nothing: the budget is fleet-wide, never
        // session-scoped.
        record_refusal(&path, t0 + 10_000).unwrap();
        let (ledger, _) = read_ledger(&path);
        assert_eq!(ledger.backoff_until_ms, t0 + 60_000);

        let v = admit(&path, &argv(&["api", "user"]), t0 + 20_000, 450);
        assert!(v.admitted, "the half-open probe passes");
        let v = admit(&path, &argv(&["api", "user"]), t0 + 35_000, 450);
        assert!(!v.admitted);
        assert_eq!(v.cause, Some("backoff"));

        let before = std::fs::read(&path).unwrap();
        let v = admit(&path, &argv(&["--version"]), t0 + 40_000, 450);
        assert!(v.admitted);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "0-point admit writes nothing"
        );
    }

    #[test]
    fn backoff_expires_and_budget_refusals_age_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        let t0 = 1_760_000_000_000i64;
        record_refusal(&path, t0).unwrap();
        // The half-open probe at +20s is spent, so +25s refuses - but past
        // the 60s window the next request is admitted again.
        admit(&path, &argv(&["api", "user"]), t0 + 20_000, 450);
        let v = admit(&path, &argv(&["api", "user"]), t0 + 25_000, 450);
        assert_eq!(v.cause, Some("backoff"));
        let v = admit(&path, &argv(&["api", "user"]), t0 + 61_000, 450);
        assert!(v.admitted, "window expiry reopens admission: {v:?}");

        // A budget refusal opens NO backoff: only GitHub's own refusals do.
        let path2 = tmp.path().join("budget2.json");
        let read = argv(&["pr", "view", "1", "--json", "state"]);
        for i in 0..5 {
            admit(&path2, &read, t0 + i * 100, 3);
        }
        let v = admit(&path2, &read, t0 + 600, 3);
        assert_eq!(v.cause, Some("budget"));
        let (ledger, _) = read_ledger(&path2);
        assert_eq!(
            ledger.backoff_until_ms, 0,
            "budget refusal opens no backoff"
        );
    }

    #[test]
    fn refusal_escalates_within_the_window_and_resets_outside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        let t0 = 1_760_000_000_000i64;
        record_refusal(&path, t0).unwrap();
        // The first refusal after the window expired but within 900s of the
        // last one escalates one step: 60s -> 120s.
        admit(&path, &argv(&["api", "user"]), t0 + 61_000, 450); // half-open probe spent
        record_refusal(&path, t0 + 61_500).unwrap();
        let (ledger, _) = read_ledger(&path);
        assert_eq!(ledger.backoff_until_ms, t0 + 61_500 + 120_000);
        // A refusal more than 900s after the last one starts over at 60s.
        record_refusal(&path, t0 + 61_500 + 120_000 + 1_000_000).unwrap();
        let (ledger, _) = read_ledger(&path);
        assert_eq!(ledger.backoff_step, 0);
    }
}
