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
//! Verb surface: the ops ride the EXISTING `fleet-incident` action as its
//! `gh-budget` argument (law d-fe66560a allows no new client action):
//! `fno-agents fleet-incident gh-budget <<< '{"op":"admit","argv":[...]}'`.
//! One JSON payload on stdin (the `rust_binary.verb_call` door convention),
//! one JSON object on stdout. All three ops exit 0; the refusal is carried
//! in the payload, never in the exit code, because `verb_call` raises on a
//! nonzero exit.
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::io::{IsTerminal, Read};
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

#[derive(Debug, Clone)]
struct Ledger {
    stamps: Vec<(i64, u32)>,
    backoff_until_ms: i64,
    backoff_step: i64,
    /// 0 means "never refused"; a real refusal stamp is a real epoch ms.
    last_refusal_ms: i64,
    /// 0 means "no half-open probe yet"; the first probe during a backoff
    /// always passes so recovery gets noticed instead of waited out.
    last_half_open_ms: i64,
    /// GitHub's own account-wide core reading: -1 unknown, 0 exhausted.
    /// Writers pass what the conditional door, the classify probe or the
    /// loop gate observed; a later reset replaces the stored value.
    core_remaining: i64,
    /// The epoch ms the stored reading's bucket resets at; 0 unknown.
    core_reset_ms: i64,
    /// When the stored reading was observed; 0 never.
    core_seen_ms: i64,
}

impl Default for Ledger {
    fn default() -> Self {
        Ledger {
            stamps: Vec::new(),
            backoff_until_ms: 0,
            backoff_step: 0,
            last_refusal_ms: 0,
            last_half_open_ms: 0,
            // Unknown, never 0: an absent or unreadable ledger must not
            // read as an exhausted account during outage diagnosis. The
            // admit gate stays inert anyway (reset 0 is never in the
            // future); this keeps `status` honest.
            core_remaining: -1,
            core_reset_ms: 0,
            core_seen_ms: 0,
        }
    }
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
    /// GitHub's account-wide core reading: -1 unknown.
    pub core_remaining: i64,
    /// Seconds until the stored core reading resets; 0 when unknown or past.
    pub core_reset_in_s: i64,
    /// The active reserve; 0 disables the reserve refusals.
    pub core_reserve: u32,
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

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Port of `fno.pr._quota.command_args`: drop gh-wide options so equivalent
/// command spellings share one cost.
pub(crate) fn command_args(args: &[String]) -> Vec<String> {
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

/// The fleet-wide hold deadline: this ledger's own, raised by the claims
/// store's shared `gh-budget:fleet` row when another host recorded the
/// refusal. Store trouble reads as no shared hold - fail-open, the same
/// posture as an unreadable ledger; the local ledger still catches this
/// machine's own refusals.
fn shared_hold_until(now: i64) -> i64 {
    crate::claim_store::budget_hold_read(None, now)
        .ok()
        .flatten()
        .map(|(until, _, _)| until)
        .unwrap_or(0)
}

pub(crate) fn lock_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

/// flock held for the scope of one read-modify-write, following
/// `loop_lead::bump_respawn_count`. Shared with the pr-park store so the
/// daemon sweep and the watcher tick serialize on the same `.lock` file.
pub(crate) struct FileLock {
    handle: std::fs::File,
}

impl FileLock {
    /// The panicking form, kept for the callers that own their own store and
    /// have no degrade. The budget consult does NOT use this one; it takes
    /// try_acquire.
    pub(crate) fn acquire(path: &Path) -> FileLock {
        FileLock::try_acquire(path)
            .unwrap_or_else(|e| panic!("gh-budget: cannot open lock {}: {e}", path.display()))
    }

    /// The fallible twin: None when the lock file cannot be created or
    /// flocked (a read-only or uncreatable state directory). The budget
    /// consult degrades to a lock-free read instead of aborting the process
    /// on every gh call - the module's fail-open rule.
    pub(crate) fn try_acquire(path: &Path) -> Result<FileLock, String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let handle = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let locked = unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_EX) };
        if locked != 0 {
            return Err(format!("cannot flock {}: {locked}", path.display()));
        }
        Ok(FileLock { handle })
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
    // Absent core fields read as unknown (-1/0/0): an older ledger written
    // by a deployed fno-agents without this change admits as before.
    let core_remaining = v
        .get("core_remaining")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    (
        Ledger {
            stamps,
            backoff_until_ms: num("backoff_until_ms"),
            backoff_step: num("backoff_step"),
            last_refusal_ms: num("last_refusal_ms"),
            last_half_open_ms: num("last_half_open_ms"),
            core_remaining,
            core_reset_ms: num("core_reset_ms"),
            core_seen_ms: num("core_seen_ms"),
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
        "core_remaining": out.core_remaining,
        "core_reset_ms": out.core_reset_ms,
        "core_seen_ms": out.core_seen_ms,
    });
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    let tmp = dir.join(format!(".github-request-budget.tmp-{}", std::process::id()));
    std::fs::write(&tmp, body.to_string())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

fn refusal_line(verdict: &Verdict, core_left: i64) -> String {
    // "rate limit" keeps both existing classifiers (_rest.py and
    // stderr_smells_rate_limit) reading a local refusal as secondary, so
    // every current caller backs off without new handling. "HTTP 403" must
    // stay out: it is the marker _quota.record_refusal keys on, and a local
    // conditional refusal must never be recorded as GitHub's.
    if matches!(verdict.cause, Some("core") | Some("core-reserve")) {
        return format!(
            "gh budget: GitHub core rate limit held locally ({cause}: {core_left} left, \
             resets in {retry}s); this command did not reach GitHub. Retry after {retry}s. \
             Ledger: fno-agents fleet-incident gh-budget status",
            cause = verdict.cause.unwrap_or("core"),
            retry = verdict.retry_after_s,
        );
    }
    let cause = verdict.cause.unwrap_or("budget");
    format!(
        "gh budget: fleet GitHub rate limit held locally ({cause}: {}/{} points in 60s | \
         backoff {}s left); this command did not reach GitHub. Retry after {}s. \
         Ledger: fno-agents fleet-incident gh-budget status",
        verdict.points_60s, verdict.cap, verdict.retry_after_s, verdict.retry_after_s
    )
}

/// May this request go out now? Takes the ledger path, the clock and the cap
/// so tests pass a temp path, a fixed clock, and a small cap.
pub fn admit(path: &Path, argv: &[String], now: i64, cap: u32) -> Verdict {
    let points = points_for(argv);
    let _lock = FileLock::try_acquire(&lock_path(path));
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

    // The core gate sits before the secondary backoff: an exhausted or
    // reserved core bucket refuses core-shaped requests regardless of the
    // secondary window, while GraphQL reads ride the untouched bucket.
    let (bucket, _) = crate::gh_spend::bucket_for(argv);
    if bucket == crate::gh_spend::Bucket::Core
        && now < ledger.core_reset_ms
        && ledger.core_remaining >= 0
    {
        let reserve = core_reserve() as i64;
        let retry_after_s = ((ledger.core_reset_ms - now) / 1000).max(1);
        let exhausted = ledger.core_remaining == 0;
        // The reserve stands down only the recurring pollers, so the last
        // requests of the bucket land on merges and pushes, never on a
        // watch loop.
        let reserved = reserve > 0
            && ledger.core_remaining <= reserve
            && crate::gh_spend::is_poll_class(&crate::gh_spend::caller_label());
        if exhausted || reserved {
            let mut verdict = Verdict {
                admitted: false,
                cause: Some(if exhausted { "core" } else { "core-reserve" }),
                retry_after_s,
                points_60s: window_points(&ledger, now),
                cap,
                ledger: state,
                refusal: None,
            };
            verdict.refusal = Some(refusal_line(&verdict, ledger.core_remaining));
            crate::gh_spend::journal_verdict(path, argv, now, &verdict);
            return verdict;
        }
    }

    // The shared hold rides only the REAL ledger: tests stage temp paths,
    // and a test's world must not read (or inherit) the dev machine's fleet
    // hold.
    let shared_until = if path == ledger_path().as_path() {
        shared_hold_until(now)
    } else {
        0
    };
    let backoff_until_ms = ledger.backoff_until_ms.max(shared_until);

    if now < backoff_until_ms {
        // One request per 30s as a half-open probe, so recovery at the top
        // gets noticed instead of waited out; the rest are refused with
        // cause `backoff`.
        if ledger.last_half_open_ms == 0 || now - ledger.last_half_open_ms >= HALF_OPEN_MS {
            ledger.last_half_open_ms = now;
            ledger.stamps.push((now, points));
            write_ledger(path, &ledger, now).ok();
            let v = Verdict {
                admitted: true,
                cause: None,
                retry_after_s: 0,
                points_60s: window_points(&ledger, now),
                cap,
                ledger: state,
                refusal: None,
            };
            crate::gh_spend::journal_verdict(path, argv, now, &v);
            return v;
        }
        let retry_after_s = ((backoff_until_ms - now) / 1000).max(1);
        let mut verdict = Verdict {
            admitted: false,
            cause: Some("backoff"),
            retry_after_s,
            points_60s: window_points(&ledger, now),
            cap,
            ledger: state,
            refusal: None,
        };
        verdict.refusal = Some(refusal_line(&verdict, -1));
        crate::gh_spend::journal_verdict(path, argv, now, &verdict);
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
        verdict.refusal = Some(refusal_line(&verdict, -1));
        crate::gh_spend::journal_verdict(path, argv, now, &verdict);
        return verdict;
    }

    ledger.stamps.push((now, points));
    write_ledger(path, &ledger, now).ok();
    let v = Verdict {
        admitted: true,
        cause: None,
        retry_after_s: 0,
        points_60s: points_60s + points as i64,
        cap,
        ledger: state,
        refusal: None,
    };
    crate::gh_spend::journal_verdict(path, argv, now, &v);
    v
}

/// The admit op's origin judgment. An agent-origin caller (a spawned worker,
/// or an unattended claude thread whose supervisor stripped the identity) is
/// held by the budget like any transport; the user's own typed verb is never
/// held, and its verdict is read off the ledger without writing it.
fn admit_for_origin(path: &Path, argv: &[String], now: i64, cap: u32, agent: bool) -> Verdict {
    if !agent {
        let (ledger, state) = read_ledger(path);
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
    admit(path, argv, now, cap)
}

/// GitHub traffic is gh itself or a loop-check adapter that wraps it.
pub(crate) fn is_github_program(program: &OsStr) -> bool {
    let Some(name) = std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    name == "gh" || crate::loopcheck::internal_gh_adapter(name)
}

/// The child stamps each real request for an agent origin, so a transport
/// stamp too charged one call 1+N; this names the spawns whose child
/// consults, and the transport stands down for them. Any unresolved shape
/// reads false, so the transport keeps charging: unknown always means
/// charge.
pub(crate) fn child_charges_at(
    program: &OsStr,
    agent: bool,
    proxy_dir: Option<&Path>,
    path_var: Option<&OsStr>,
) -> bool {
    if !agent {
        return false;
    }
    let Some(name) = std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    if name == "fno-gh-proxy" || crate::loopcheck::internal_gh_adapter(name) {
        return true;
    }
    if name != "gh" {
        return false;
    }
    let Some(proxy_dir) = proxy_dir else {
        return false;
    };
    let explicit = std::path::Path::new(program)
        .parent()
        .is_some_and(|p| !p.as_os_str().is_empty());
    let resolved = if explicit {
        Some(std::path::PathBuf::from(program))
    } else {
        first_executable_on_path(name, path_var)
    };
    let Some(resolved) = resolved else {
        return false;
    };
    let Some(parent) = resolved.parent() else {
        return false;
    };
    match (parent.canonicalize(), proxy_dir.canonicalize()) {
        (Ok(child), Ok(proxy)) => child == proxy,
        _ => false,
    }
}

/// A bare program name resolves to its first executable hit on `path_var`;
/// no executable hit resolves to None.
fn first_executable_on_path(name: &str, path_var: Option<&OsStr>) -> Option<std::path::PathBuf> {
    let pv = path_var?;
    std::env::split_paths(pv)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .map(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.is_file() && m.permissions().mode() & 0o111 != 0
                })
                .unwrap_or(false)
        })
}

/// The env-reading form the spawn doors call: the origin judge, the proxy
/// dir and PATH all come from this process's environment.
pub(crate) fn child_charges(program: &OsStr) -> bool {
    let proxy_dir = std::env::var_os("FNO_GH_PROXY_DIR").map(std::path::PathBuf::from);
    let path_var = std::env::var_os("PATH");
    child_charges_at(
        program,
        crate::spawn_gate_admission::gate_agent_origin(),
        proxy_dir.as_deref(),
        path_var.as_deref(),
    )
}

/// Some(refusal line) when `program args` is GitHub traffic the fleet budget
/// refuses now. None for every other program, every admit, and every
/// disarmed unit-test build (admission_armed).
pub(crate) fn admit_spawn<S: AsRef<OsStr>>(program: &OsStr, args: &[S]) -> Option<String> {
    if !admission_armed() {
        return None;
    }
    // The transport stands down only where the child consults; a user or
    // daemon spawn, and every unresolvable shape, keeps the stamp here.
    if child_charges(program) {
        return None;
    }
    let argv: Vec<String> = args
        .iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect();
    admit_spawn_at(&ledger_path(), now_ms(), cap_from_env(), program, &argv)
}

/// admit_spawn for a caller whose origin is judged first: an agent-origin
/// read is held by the budget, an attended user's never is. The door a
/// Python adapter delegates to uses this in place of the origin-blind
/// admission inside `bounded_read`.
pub(crate) fn admit_spawn_for_origin(argv: &[String]) -> Option<String> {
    if !admission_armed() {
        return None;
    }
    if child_charges(std::ffi::OsStr::new("gh")) {
        return None;
    }
    let verdict = admit_for_origin(
        &ledger_path(),
        argv,
        now_ms(),
        cap_from_env(),
        crate::spawn_gate_admission::gate_agent_origin(),
    );
    if verdict.admitted {
        None
    } else {
        verdict.refusal
    }
}

/// The same question with the ledger path, clock and cap injected, so tests
/// pass a temp ledger. The argv carries the subcommand and arguments, NOT
/// the program name - the shape `admit` takes and `points_for` prices.
pub(crate) fn admit_spawn_at(
    path: &Path,
    now: i64,
    cap: u32,
    program: &OsStr,
    args: &[String],
) -> Option<String> {
    if !is_github_program(program) {
        return None;
    }
    let verdict = admit(path, args, now, cap);
    if verdict.admitted {
        None
    } else {
        verdict.refusal
    }
}

/// admit_spawn for an already-built Command: the program and args come off
/// the command, so a transport admits without knowing what it was handed.
/// The program check runs before any argv copy: the transports carry every
/// git read too, and those must stay allocation-free.
pub(crate) fn admit_command(cmd: &std::process::Command) -> Option<String> {
    if !admission_armed() || !is_github_program(cmd.get_program()) {
        return None;
    }
    // The same stand-down rule as admit_spawn: only a spawn whose child
    // consults skips this layer; user and daemon reads keep the stamp.
    if child_charges(cmd.get_program()) {
        return None;
    }
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    admit_spawn_at(
        &ledger_path(),
        now_ms(),
        cap_from_env(),
        cmd.get_program(),
        &args,
    )
}

/// The refusal as a finished child: exit 75 (the Python door's REFUSED),
/// empty stdout, the ledger line on stderr. A caller folds it into its
/// existing "gh failed" path with no new handling.
pub(crate) fn refused_output(line: String) -> std::process::Output {
    std::process::Output {
        status: std::os::unix::process::ExitStatusExt::from_raw(75 << 8),
        stdout: Vec::new(),
        stderr: line.into_bytes(),
    }
}

/// Command::output() behind the budget: the door for a direct gh spawn. A
/// refusal never spawns a child; everything else is the command's own output.
pub(crate) fn output(mut cmd: std::process::Command) -> std::io::Result<std::process::Output> {
    if let Some(line) = admit_command(&cmd) {
        return Ok(refused_output(line));
    }
    cmd.output()
}

/// Whether the in-process consult is armed. A unit-test build runs dozens of
/// gh-named stubs in one process, and their aggregate stamps would exhaust
/// (or inherit a hold from) the operator's machine-wide ledger - the stubs
/// are not GitHub traffic. The consult disarms there; budget behavior is
/// tested at admit()/record_refusal() with injected paths and clocks, and a
/// test that wants the transport consult live opts in with
/// FNO_GH_BUDGET_TEST_ARMED.
pub(crate) fn admission_armed() -> bool {
    !(cfg!(test) && std::env::var_os("FNO_GH_BUDGET_TEST_ARMED").is_none())
}

/// A GitHub refusal reached one caller. One fleet-wide backoff opens in the
/// ledger, escalating while refusals stay within 900s of each other. A no-op
/// while a backoff is already live, so two writers reporting one refusal
/// widen nothing.
pub fn record_refusal(path: &Path, now: i64) -> Result<(), String> {
    let _lock = FileLock::try_acquire(&lock_path(path));
    let (mut ledger, _) = read_ledger(path);
    if now < ledger.backoff_until_ms {
        return Ok(());
    }
    // 0 is the "never" sentinel, not the epoch: a first refusal must not
    // escalate off an unset field. The shared row's history counts even
    // after its deadline expires, so refusals alternating hosts inside the
    // window keep escalating instead of restarting at the base cooldown.
    let (shared_step, shared_last) = if path == ledger_path().as_path() {
        crate::claim_store::budget_hold_history(None).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let last_refusal = ledger.last_refusal_ms.max(shared_last);
    let escalated = last_refusal > 0 && now - last_refusal <= ESCALATION_WINDOW_MS;
    ledger.backoff_step = if escalated {
        ledger.backoff_step.max(shared_step) + 1
    } else {
        0
    };
    let shift = ledger.backoff_step.min(4) as u32;
    let backoff = (BASE_BACKOFF_MS << shift).min(MAX_BACKOFF_MS);
    ledger.backoff_until_ms = now + backoff;
    ledger.last_refusal_ms = now;
    write_ledger(path, &ledger, now)?;
    // The hold is fleet-wide: GitHub limits the user, not the machine, so
    // the backoff rides the claims store's shared row for the other hosts.
    // Rides only the REAL ledger (tests stage temp paths); store trouble is
    // a printed note, never a failure of the local record.
    if path == ledger_path().as_path() {
        if let Err(e) = crate::claim_store::budget_hold_write(
            None,
            &crate::claims::machine_id(),
            ledger.backoff_until_ms,
            ledger.backoff_step,
            ledger.last_refusal_ms,
        ) {
            eprintln!("gh-budget: fleet hold not shared: {e}");
        }
    }
    Ok(())
}

/// The reserve before exhaustion that stands down only the pollers: 0
/// disables the reserve refusals; exhaustion (remaining == 0) refuses
/// regardless of the reserve.
pub(crate) fn core_reserve() -> u32 {
    std::env::var("FNO_GH_CORE_RESERVE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(500)
}

/// One core reading lands in the ledger under the same flock, read-modify
/// write: it writes when the reading is newer than core_seen_ms, and
/// lowers core_remaining only within the same core_reset_ms (a later reset
/// replaces the bucket; an older one is a stale window, dropped). A fresh
/// reading (under 60 s old, same reset, both values above twice the
/// reserve) skips the write, so the write rate stays flat while the
/// bucket is healthy.
pub(crate) fn record_core_at(path: &Path, remaining: i64, reset_epoch_s: i64, now: i64) {
    let _lock = FileLock::try_acquire(&lock_path(path));
    let (mut ledger, _) = read_ledger(path);
    let reset_ms = reset_epoch_s * 1000;
    if ledger.core_seen_ms > 0
        && now - ledger.core_seen_ms < 60_000
        && ledger.core_reset_ms == reset_ms
        && ledger.core_remaining > 2 * core_reserve() as i64
        && remaining > 2 * core_reserve() as i64
    {
        return;
    }
    if ledger.core_reset_ms == 0 || reset_ms > ledger.core_reset_ms {
        // A later reset replaces the stored bucket wholesale.
        ledger.core_remaining = remaining;
        ledger.core_reset_ms = reset_ms;
        ledger.core_seen_ms = now;
    } else if reset_ms == ledger.core_reset_ms && now > ledger.core_seen_ms {
        ledger.core_remaining = if ledger.core_remaining < 0 {
            remaining
        } else {
            ledger.core_remaining.min(remaining)
        };
        ledger.core_seen_ms = now;
    } else {
        // An older reset: a stale window's reading, dropped.
        return;
    }
    write_ledger(path, &ledger, now).ok();
}

/// The one core-observation entry the three writers call: it resolves the
/// REAL ledger and no-ops under a unit-test build (a stubbed probe's core-0
/// reading must never land in the operator's live ledger). Opt-in for a
/// test that wants the entry live: FNO_GH_BUDGET_TEST_ARMED.
pub(crate) fn record_core_observation(remaining: i64, reset_epoch_s: i64) {
    if !core_recording_armed() {
        return;
    }
    record_core_at(&ledger_path(), remaining, reset_epoch_s, now_ms());
}

pub(crate) fn core_recording_armed() -> bool {
    !(cfg!(test) && std::env::var_os("FNO_GH_BUDGET_TEST_ARMED").is_none())
}

/// Read-only projection for status surfaces and the loop gate's pre-read.
pub fn snapshot(path: &Path, now: i64) -> Snapshot {
    let (ledger, state) = read_ledger(path);
    // The shared hold rides only the REAL ledger (see admit): tests stage
    // temp paths and must not read the dev machine's fleet hold.
    let shared_until = if path == ledger_path().as_path() {
        shared_hold_until(now)
    } else {
        0
    };
    let until = ledger.backoff_until_ms.max(shared_until);
    Snapshot {
        points_60s: window_points(&ledger, now),
        cap: cap_from_env(),
        backoff_remaining_s: ((until - now).max(0)) / 1000,
        ledger: state,
        core_remaining: ledger.core_remaining,
        core_reset_in_s: if ledger.core_reset_ms > now {
            (ledger.core_reset_ms - now) / 1000
        } else {
            0
        },
        core_reserve: core_reserve(),
    }
}

/// Binary entry under the `fleet-incident` action (law d-fe66560a allows no
/// new client action): `fno-agents fleet-incident gh-budget [status]`, or the
/// stdin door `verb_call("fleet-incident", {"op": ...})` takes. One JSON
/// payload on stdin (the `rust_binary.verb_call` door convention), one JSON
/// object on stdout. All three ops exit 0; the refusal is carried in the
/// payload, never in the exit code, because `verb_call` raises on a nonzero
/// exit. The bare `status` word answers without stdin, so the refusal prose
/// can name a copy-pasteable command.
pub fn run_gh_budget(args: &[String]) -> i32 {
    if args == ["status"] {
        print_status();
        return 0;
    }
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
    dispatch_op(&payload)
}

/// The stdin door verb_call takes: `fno-agents fleet-incident` with the ops
/// payload on stdin and NO argv. A payload naming a budget op dispatches
/// here; anything else (a bare interactive `fno-agents fleet-incident`, a
/// tty stdin, non-JSON stdin) falls through to the incident breaker's own
/// behavior, which owns its usage error.
pub fn run_gh_budget_stdin_door() -> i32 {
    if std::io::stdin().is_terminal() {
        return -1; // not ours: the caller falls through to the breaker
    }
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return -1;
    }
    let Ok(payload) = serde_json::from_str::<Value>(&buf) else {
        return -1;
    };
    if payload.get("op").and_then(Value::as_str).is_none() {
        return -1;
    }
    dispatch_op(&payload)
}

fn print_status() {
    let s = snapshot(&ledger_path(), now_ms());
    println!(
        "{}",
        json!({
            "points_60s": s.points_60s,
            "cap": s.cap,
            "backoff_remaining_s": s.backoff_remaining_s,
            "ledger": ledger_word(s.ledger),
            "core_remaining": s.core_remaining,
            "core_reset_in_s": s.core_reset_in_s,
            "core_reserve": s.core_reserve,
        })
    );
}

fn dispatch_op(payload: &Value) -> i32 {
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
            // The op is the one place the Python doors' origin is judged;
            // the Rust transports stamp only the spawns whose child does
            // not consult (child_charges).
            let v = admit_for_origin(
                &ledger_path(),
                &argv,
                now,
                cap_from_env(),
                crate::spawn_gate_admission::gate_agent_origin(),
            );
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
            print_status();
            0
        }
        Some("spend") => {
            let hours = payload.get("hours").and_then(Value::as_i64).unwrap_or(1);
            let github = if payload
                .get("github")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Some(crate::gh_spend::rate_limit_probe())
            } else {
                None
            };
            println!(
                "{}",
                crate::gh_spend::spend_report_at(
                    &crate::gh_spend::journal_path(),
                    now,
                    hours,
                    github,
                )
            );
            0
        }
        other => {
            eprintln!(
                "gh-budget: unknown op {other:?}; expected admit, refused, spend, or status \
                 (fno-agents fleet-incident gh-budget)"
            );
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
    fn admit_op_holds_agent_callers_and_never_the_user() {
        // AC1-HP / AC1-ERR: at cap, an agent-origin admit refuses `budget`;
        // the user's read admits and the ledger file is untouched.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        let now = 1_760_000_000_000i64;
        let read = argv(&["pr", "view", "1", "--json", "state"]);
        let v = admit(&path, &read, now, 1);
        assert!(v.admitted, "the staging read is admitted: {v:?}");
        let before = std::fs::read(&path).unwrap();

        let v = admit_for_origin(&path, &read, now, 1, false);
        assert!(v.admitted);
        assert_eq!(v.points_60s, 1);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a user admit writes nothing"
        );

        let v = admit_for_origin(&path, &read, now, 1, true);
        assert!(!v.admitted);
        assert_eq!(v.cause, Some("budget"));
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

    #[test]
    fn admit_spawn_refuses_github_programs_only() {
        // A live backoff with the half-open probe spent (the shape the
        // 2026-10-15 fleet repro seeded): every 1-point gh ask refuses,
        // other programs never consult the ledger, and a 0-point gh shape
        // admits without a write.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        let now = 1_760_000_000_000i64;
        std::fs::write(
            &path,
            r#"{"stamps":[],"backoff_until_ms":1760000600000,"backoff_step":0,"last_refusal_ms":1759999540000,"last_half_open_ms":1760000000000}"#,
        )
        .unwrap();
        let gh = std::ffi::OsStr::new("gh");
        let args = &["api".to_string(), "user".to_string()];
        let line = admit_spawn_at(&path, now, 450, gh, args).expect("gh refuses under the backoff");
        assert!(line.contains("gh budget:"), "line: {line}");

        // An absolute path and the internal coverage adapter are the same
        // traffic under other names.
        let abs = std::ffi::OsStr::new("/opt/homebrew/bin/gh");
        assert!(
            admit_spawn_at(&path, now, 450, abs, args).is_some(),
            "an absolute gh path refuses too"
        );
        let adapter = std::ffi::OsStr::new("fno-gh-coverage");
        let pr_view = &["pr".to_string(), "view".to_string()];
        assert!(
            admit_spawn_at(&path, now, 450, adapter, pr_view).is_some(),
            "the coverage adapter refuses too"
        );

        // git never reaches the ledger; `gh --version` is a 0-point shape
        // that admits untouched. Neither writes a stamp.
        let before = std::fs::read(&path).unwrap();
        let git = std::ffi::OsStr::new("git");
        assert!(admit_spawn_at(&path, now, 450, git, &["status".to_string()]).is_none());
        assert!(admit_spawn_at(&path, now, 450, gh, &["--version".to_string()]).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before, "no stamp for either");
    }

    #[test]
    fn child_charges_names_consulting_children_for_agents_only() {
        // The stand-down table: an adapter name is a consulting child for an
        // agent and never for anyone else; a bare gh stands down only when
        // its first PATH hit resolves inside the proxy dir; every
        // unresolvable or outside shape charges.
        let tmp = tempfile::tempdir().unwrap();
        let proxy_dir = tmp.path().join("proxy");
        let other_dir = tmp.path().join("other");
        std::fs::create_dir(&proxy_dir).unwrap();
        std::fs::create_dir(&other_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for stub in [
            proxy_dir.join("gh"),
            other_dir.join("gh"),
            proxy_dir.join("fno-gh-proxy"),
        ] {
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let proxy_path: std::ffi::OsString = proxy_dir.clone().into_os_string();
        let other_path: std::ffi::OsString = other_dir.clone().into_os_string();
        let adapter = std::ffi::OsStr::new("fno-gh-coverage");
        let shim = std::ffi::OsStr::new("fno-gh-proxy");
        let gh = std::ffi::OsStr::new("gh");
        let abs = std::ffi::OsStr::new("/opt/homebrew/bin/gh");

        // An adapter stands down for an agent and charges for everyone else.
        assert!(child_charges_at(
            adapter,
            true,
            Some(&proxy_dir),
            Some(&proxy_path)
        ));
        assert!(!child_charges_at(
            adapter,
            false,
            Some(&proxy_dir),
            Some(&proxy_path)
        ));
        // The proxy shim itself consults through the Python doors.
        assert!(child_charges_at(
            shim,
            true,
            Some(&proxy_dir),
            Some(&proxy_path)
        ));
        // A bare gh whose first PATH hit is the proxy shim is a consulting
        // child; the same bare gh found elsewhere is unknown traffic.
        assert!(child_charges_at(
            gh,
            true,
            Some(&proxy_dir),
            Some(&proxy_path)
        ));
        assert!(!child_charges_at(
            gh,
            true,
            Some(&proxy_dir),
            Some(&other_path)
        ));
        // No proxy dir, no resolution, and any absolute gh outside the
        // proxy dir all charge: unknown always means charge.
        assert!(!child_charges_at(gh, true, None, Some(&proxy_path)));
        assert!(!child_charges_at(
            abs,
            true,
            Some(&proxy_dir),
            Some(&proxy_path)
        ));
        assert!(!child_charges_at(gh, true, Some(&proxy_dir), None));
    }

    /// Sets one env var for a door test and restores the previous value on
    /// drop, so a failing assert cannot poison the process env for sibling
    /// tests. Caller-kind tests must hold the shared env lock first.
    struct DoorEnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl DoorEnvGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let guard = DoorEnvGuard(vec![(key, std::env::var_os(key))]);
            std::env::set_var(key, value);
            guard
        }
    }
    impl Drop for DoorEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Seeds a hermetic ledger (FNO_STATE_DIR) with a live backoff, so any
    /// consult refuses: Some from a door proves the consult happened, None
    /// under the same ledger proves the stand-down, and a foreign consult
    /// from a sibling test thread cannot flip either observable.
    fn seed_backoff_ledger(state: &std::path::Path) -> std::path::PathBuf {
        let locks = state.join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        let path = locks.join("github-request-budget.json");
        let now_ms = now_ms();
        std::fs::write(
            &path,
            format!(
                r#"{{"stamps":[],"backoff_until_ms":{},"backoff_step":0,"last_refusal_ms":{},"last_half_open_ms":0}}"#,
                now_ms + 120_000,
                now_ms - 1_000
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn admit_spawn_charges_by_caller_kind() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let proxy = tmp.path().join("proxy");
        let other = tmp.path().join("other");
        std::fs::create_dir(&proxy).unwrap();
        std::fs::create_dir(&other).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for stub in [proxy.join("gh"), other.join("gh")] {
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _state = DoorEnvGuard::set("FNO_STATE_DIR", tmp.path().join("state").as_os_str());
        let _armed = DoorEnvGuard::set("FNO_GH_BUDGET_TEST_ARMED", std::ffi::OsStr::new("1"));
        let _proxy = DoorEnvGuard::set("FNO_GH_PROXY_DIR", proxy.as_os_str());
        seed_backoff_ledger(&tmp.path().join("state"));
        let args = &["api".to_string(), "user".to_string()];
        let gh = std::ffi::OsStr::new("gh");

        // User origin: the transport charges regardless of where gh sits.
        let _attended =
            DoorEnvGuard::set("CLAUDE_CODE_SESSION_ATTENDED", std::ffi::OsStr::new("1"));
        let _path = DoorEnvGuard::set("PATH", proxy.as_os_str());
        assert!(
            admit_spawn(gh, args).is_some(),
            "a user-origin spawn keeps the transport consult"
        );
        drop(_path);

        // Agent origin, bare gh first hit inside the proxy: the child
        // consults, so the transport stands down under a live backoff.
        let _agent = crate::spawn_gate_admission::AgentSelfFixture::set();
        let _path = DoorEnvGuard::set("PATH", proxy.as_os_str());
        assert!(
            admit_spawn(gh, args).is_none(),
            "a consulting child stands the transport down"
        );
        // An adapter name is a consulting child at any PATH.
        let adapter = std::ffi::OsStr::new("fno-gh-coverage");
        assert!(
            admit_spawn(adapter, args).is_none(),
            "an adapter spawn stands the transport down"
        );
        drop(_path);

        // Agent origin, gh resolving outside the proxy: unknown means the
        // transport charges, and the seeded backoff refuses it.
        let _path = DoorEnvGuard::set("PATH", other.as_os_str());
        assert!(
            admit_spawn(gh, args).is_some(),
            "an unresolvable-as-child spawn keeps the consult"
        );
    }

    #[test]
    fn admit_command_charges_by_caller_kind() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let proxy = tmp.path().join("proxy");
        let other = tmp.path().join("other");
        std::fs::create_dir(&proxy).unwrap();
        std::fs::create_dir(&other).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for stub in [proxy.join("gh"), other.join("gh")] {
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _state = DoorEnvGuard::set("FNO_STATE_DIR", tmp.path().join("state").as_os_str());
        let _armed = DoorEnvGuard::set("FNO_GH_BUDGET_TEST_ARMED", std::ffi::OsStr::new("1"));
        let _proxy = DoorEnvGuard::set("FNO_GH_PROXY_DIR", proxy.as_os_str());
        seed_backoff_ledger(&tmp.path().join("state"));

        // User origin: an absolute gh outside the proxy keeps the consult.
        let _attended =
            DoorEnvGuard::set("CLAUDE_CODE_SESSION_ATTENDED", std::ffi::OsStr::new("1"));
        let cmd = std::process::Command::new(other.join("gh"));
        assert!(
            admit_command(&cmd).is_some(),
            "a user-origin Command keeps the transport consult"
        );

        // Agent origin: an absolute gh inside the proxy stands down; the
        // same shape outside it consults and refuses.
        let _agent = crate::spawn_gate_admission::AgentSelfFixture::set();
        let inside = std::process::Command::new(proxy.join("gh"));
        assert!(
            admit_command(&inside).is_none(),
            "a proxy-shim Command stands the transport down"
        );
        let outside = std::process::Command::new(other.join("gh"));
        assert!(
            admit_command(&outside).is_some(),
            "a gh outside the proxy keeps the consult"
        );
    }

    #[test]
    fn core_gate_refuses_at_zero_stands_down_pollers_and_never_outlives_its_reset() {
        // AC3-HP: an exhausted bucket refuses the core read with the
        // reset-shaped retry, never the GraphQL read.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget.json");
        let now = 1_760_000_000_000i64;
        record_core_at(&path, 0, now / 1000 + 600, now);
        let core_read = argv(&["api", "repos/o/r/pulls/1"]);
        let v = admit(&path, &core_read, now, 450);
        assert!(!v.admitted);
        assert_eq!(v.cause, Some("core"));
        assert!((v.retry_after_s - 600).abs() <= 1);
        let graphql = argv(&["api", "graphql", "-f", "query=query { x }"]);
        let v = admit(&path, &graphql, now, 450);
        assert!(v.admitted, "GraphQL is a different bucket: {v:?}");

        // AC3-HP, the reserve: remaining 300 against the default 500
        // reserve refuses the poller and admits the merge. FNO_GH_CALLER
        // pins the label per call, so the test serializes on the env lock
        // and sets it around each admit.
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path2 = tmp.path().join("budget2.json");
        record_core_at(&path2, 300, now / 1000 + 600, now);
        let wait_read = argv(&["api", "repos/o/r/pulls/7/reviews?per_page=100"]);
        std::env::set_var("FNO_GH_CALLER", "fno do pr wait");
        let v = admit(&path2, &wait_read, now, 450);
        assert!(!v.admitted);
        assert_eq!(v.cause, Some("core-reserve"));
        std::env::set_var("FNO_GH_CALLER", "fno do pr merge");
        let v = admit(&path2, &wait_read, now, 450);
        assert!(v.admitted, "the merge keeps the bucket's tail: {v:?}");
        std::env::remove_var("FNO_GH_CALLER");
        drop(_env);

        // AC3-EDGE: an exhausted reading whose reset has passed gates
        // nothing - an old exhaustion never outlives its reset.
        let path3 = tmp.path().join("budget3.json");
        record_core_at(&path3, 0, now / 1000 - 100, now);
        let v = admit(&path3, &core_read, now, 450);
        assert!(v.admitted, "a passed reset ignores core state: {v:?}");
    }

    // --- the guard against a new bare gh spawn ------------------------------

    /// One production line carrying a bare gh constructor:
    /// `Command::new("gh")`, `Command::new(&gh)`, `Command::new(gh)`, or
    /// `Command::new("fno-gh-<lowercase>")`. Hand-rolled over the line text
    /// so the gate needs no regex dependency.
    fn bare_gh_constructor(line: &str) -> bool {
        let Some(pos) = line.find("Command::new(") else {
            return false;
        };
        let rest = line[pos + "Command::new(".len()..].trim_start();
        let rest = match rest.strip_prefix('&') {
            Some(r) => r.trim_start(),
            None => rest,
        };
        let after = if let Some(quoted) = rest.strip_prefix('"') {
            let Some(end) = quoted.find('"') else {
                return false;
            };
            let name = &quoted[..end];
            let named = name == "gh"
                || (name.starts_with("fno-gh-")
                    && name["fno-gh-".len()..]
                        .chars()
                        .all(|c| c.is_ascii_lowercase()));
            if !named {
                return false;
            }
            &quoted[end + 1..]
        } else {
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if ident != "gh" {
                return false;
            }
            &rest[ident.len()..]
        };
        after.trim_start().starts_with(')')
    }

    /// The cfg(test) items cut out by brace depth, the skipper
    /// scripts/ci/check-mux-process-admission.sh uses. Returns (number,
    /// line) so hits keep their on-disk line numbers.
    fn production_lines(source: &str) -> Vec<(usize, String)> {
        let mut kept = Vec::new();
        let mut pending_test_item = false;
        let mut skip_depth: i64 = 0;
        for (idx, line) in source.lines().enumerate() {
            let stripped = line.trim();
            if skip_depth > 0 {
                skip_depth += line.matches('{').count() as i64 - line.matches('}').count() as i64;
                if skip_depth <= 0 {
                    skip_depth = 0;
                }
                continue;
            }
            if pending_test_item {
                if stripped.is_empty() || stripped.starts_with("//") {
                    continue;
                }
                pending_test_item = false;
                let depth = line.matches('{').count() as i64 - line.matches('}').count() as i64;
                if depth > 0 {
                    skip_depth = depth;
                }
                continue;
            }
            if stripped == "#[cfg(test)]" {
                pending_test_item = true;
                continue;
            }
            kept.push((idx + 1, line.to_string()));
        }
        kept
    }

    fn fn_name_of(line: &str) -> Option<String> {
        let pos = line.find("fn ")?;
        let rest = &line[pos + 3..];
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if ident.is_empty() {
            None
        } else {
            Some(ident)
        }
    }

    /// Every production `Command::new(gh...)` whose enclosing fn reaches no
    /// admitting sink (`gh_budget::output(` or `bounded_cmd::output_with_timeout`),
    /// minus the allowlist. Pure over the source so a mutated fixture can
    /// drive it, in the shape `direct_wait_bypasses` established.
    fn bare_gh_spawns(rel_path: &str, source: &str) -> Vec<String> {
        // Test files carry gh-named stubs by design; the walk skips them.
        if rel_path.contains("/tests/")
            || rel_path.ends_with("tests.rs")
            || rel_path.ends_with("_tests.rs")
        {
            return Vec::new();
        }
        let lines = production_lines(source);
        let hit_idxs: Vec<usize> = lines
            .iter()
            .enumerate()
            // A comment never spawns; a doc aside quoting the constructor
            // must not read as a violation.
            .filter(|(_, (_, line))| {
                !line.trim_start().starts_with("//") && bare_gh_constructor(line)
            })
            .map(|(i, _)| i)
            .collect();
        let mut violations = Vec::new();
        for &idx in &hit_idxs {
            // The fn runs from the last `fn ` line at or above the hit to
            // the next `fn ` line below it; the file top counts as one fn.
            // Comment lines never bound a fn: a `// fn foo()` aside must not
            // cut the body short of its admitting sink.
            let is_code = |l: &String| !l.trim_start().starts_with("//");
            let mut start = 0usize;
            let mut fn_name = String::new();
            for i in (0..=idx).rev() {
                if !is_code(&lines[i].1) {
                    continue;
                }
                if let Some(name) = fn_name_of(&lines[i].1) {
                    start = i;
                    fn_name = name;
                    break;
                }
            }
            let mut end = lines.len();
            for i in (idx + 1)..lines.len() {
                if is_code(&lines[i].1) && lines[i].1.contains("fn ") {
                    end = i;
                    break;
                }
            }
            let body = lines[start..end]
                .iter()
                .map(|(_, l)| l.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let behind_sink = body.contains("gh_budget::output(")
                || body.contains("bounded_cmd::output_with_timeout");
            // Allowlist (one site, one reason each):
            // - crates/fno/src/setup_run.rs step_gh_auth: the `fno` crate
            //   cannot depend on fno-agents, and the one-time `gh auth
            //   status` probe carries no fleet identity (a human runs setup
            //   once).
            // - fno-agents/src/merge_hold.rs disarm_automerge: the emergency
            //   auto-merge disarm must reach GitHub even under a fleet hold;
            //   an armed queue merging server-side is the outcome the hold
            //   exists to prevent.
            // - fno-agents/src/merge_verb/mod.rs disarm_queued_auto_merge:
            //   the check-stage port of the same emergency stop, same reason.
            let allowlist = [
                ("fno/src/setup_run.rs", "step_gh_auth"),
                ("fno-agents/src/merge_hold.rs", "disarm_automerge"),
                (
                    "fno-agents/src/merge_verb/mod.rs",
                    "disarm_queued_auto_merge",
                ),
            ];
            let allowlisted = allowlist
                .iter()
                .any(|(p, f)| rel_path == *p && fn_name == *f);
            if !behind_sink && !allowlisted {
                let (number, line) = &lines[idx];
                violations.push(format!("{rel_path}:{number}: {line}"));
            }
        }
        violations
    }

    fn literal_hits(source: &str) -> Vec<(usize, String)> {
        production_lines(source)
            .into_iter()
            .filter(|(_, line)| !line.trim_start().starts_with("//") && bare_gh_constructor(line))
            .collect()
    }

    fn rs_files_under(
        dir: &std::path::Path,
        rel_base: &std::path::Path,
        out: &mut Vec<(String, String)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rs_files_under(&path, rel_base, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let rel = path
                    .strip_prefix(rel_base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                if let Ok(source) = std::fs::read_to_string(&path) {
                    out.push((rel, source));
                }
            }
        }
    }

    #[test]
    fn every_gh_spawn_sits_behind_the_budget_seam() {
        // (c) Every transport still carries its admission call in the
        // production half: a removed consult fails here before a spawn can
        // slip past it.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let transports = [
            "src/bounded_cmd.rs",
            "src/gate_probes.rs",
            "src/org_board/budget.rs",
            "src/loopcheck/bounded_run.rs",
            "src/pr_draft_ready.rs",
            "src/backlog/pr_link.rs",
            "src/operator_notice.rs",
        ];
        for rel in transports {
            let path = manifest.join(rel);
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let prod_text = production_lines(&source)
                .iter()
                .map(|(_, l)| l.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                prod_text.contains("gh_budget::admit_"),
                "transport {rel} lost its fleet-budget admission"
            );
        }

        let crates_dir = manifest.parent().unwrap().to_path_buf();
        let mut files = Vec::new();
        rs_files_under(&manifest.join("src"), &crates_dir, &mut files);
        rs_files_under(&crates_dir.join("fno/src"), &crates_dir, &mut files);

        let mut raw = 0usize;
        let mut violations = Vec::new();
        for (rel_path, source) in &files {
            raw += literal_hits(source).len();
            violations.extend(bare_gh_spawns(rel_path, source));
        }
        // (b) A wrong haystack must not read clean: the walk on the real
        // tree finds well over a dozen literal gh constructors.
        assert!(
            raw >= 12,
            "the haystack decayed: only {raw} literal gh constructors found"
        );

        // (a) The scanner refuses a bare fn and passes the same fn behind
        // the door.
        let bare = "fn f() {\n    std::process::Command::new(\"gh\").args([\"api\", \"x\"]).output();\n}\n";
        assert_eq!(bare_gh_spawns("x.rs", bare).len(), 1, "bare fn is a hit");
        let behind = "fn f() {\n    let cmd = std::process::Command::new(\"gh\");\n    let _ = crate::gh_budget::output(cmd);\n}\n";
        assert!(
            bare_gh_spawns("x.rs", behind).is_empty(),
            "fn behind the door passes"
        );

        assert!(
            violations.is_empty(),
            "bare gh spawns outside the fleet budget seam:\n{}\n\
             route each through crate::gh_budget::output or a bounded_cmd \
             transport, or add an allowlist entry with a reason",
            violations.join("\n")
        );
    }
}
