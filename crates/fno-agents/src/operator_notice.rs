//! How a Rust caller reaches the operator, and when it should stay quiet.
//!
//! Three jobs share one module because they share one chokepoint: the
//! `fno inbox notify` spawn (one site, by the answerer sweep's count), the
//! state-change signal store that keeps a sampler honest, and the
//! notify_watch arm - the timer that samples computed state and notifies only
//! on a change. Every caller into the chokepoint before this module was
//! event-driven: something happened inside a process, and that process posted.
//! State that changes with no event reached nobody, which is how main went
//! red on 2026-09-06 and no instrument reported it for forty minutes.
//!
//! The arm samples the king board queues and main CI's check runs, collapses
//! each signal on a token through the store, and emits one pointer notice per
//! changed signal. The token IS the state; the tick is only a sample. A
//! notice is a pointer to the durable queue, never a copy of it.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// The one spawn site
// ---------------------------------------------------------------------------

/// Fire-and-forget operator notice through the `fno inbox notify` chokepoint,
/// binary resolved from `FNO_BIN` (default `fno`). Detached with null stdio,
/// reaped on its own thread, spawn failure logged and dropped - the daemon
/// leg's discipline, kept verbatim, because it is the stricter of the two.
/// Returns whether the child spawned, so the signal store can refuse to
/// commit state for a notice that never left the machine.
pub fn notify_operator(title: &str, body: &str, pointer: Option<&str>) -> bool {
    let fno = std::env::var_os("FNO_BIN").unwrap_or_else(|| std::ffi::OsString::from("fno"));
    notify_operator_with(&fno, title, body, pointer)
}

/// The same spawn with the binary handed in. Callers whose env seam is pinned
/// by a test (`FNO_LOOPCHECK_FNO_BIN`) resolve their own binary and pass it
/// here; the args array stays in this one place.
pub fn notify_operator_with(bin: &OsStr, title: &str, body: &str, pointer: Option<&str>) -> bool {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["inbox", "notify", title, body]);
    if let Some(p) = pointer {
        cmd.args(["--pointer", p]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Spawn is a fork, not a wait: it answers here, so the caller learns
    // whether the notice left, and only the reap rides a detached thread.
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(e) => {
            eprintln!(
                "fno-agents: operator notice skipped ({} inbox notify): {e}",
                bin.to_string_lossy()
            );
            false
        }
    }
}

// ---------------------------------------------------------------------------
// The signal store
// ---------------------------------------------------------------------------

/// Whether a send went out, was already sent, or was held by the rate floor.
pub enum Verdict {
    Sent,
    Deduped,
    RateHeld,
    SendFailed,
}

fn notify_signals_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_NOTIFY_SIGNALS").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("."));
    PathBuf::from(home).join(".fno").join("notify-signals.json")
}

fn load_store(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_store(path: &Path, store: &Map<String, Value>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // pid-suffixed temp + rename: a concurrent pass never reads a half file.
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(store).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn age_seconds(ts: &str, now: u64) -> u64 {
    match crate::tick_ledger::parse_rfc3339_unix(ts) {
        Some(then) => now.saturating_sub(then),
        // Unreadable ts reads as old, never held forever.
        None => u64::MAX,
    }
}

/// Collapse one send on `token`. Deduped when the stored token equals it;
/// RateHeld when it changed inside the floor (held, not dropped - the token
/// stays unwritten, so the next pass reconsiders the same change); otherwise
/// the sender runs and state commits only on an accepted send.
pub fn notify_signal(
    key: &str,
    token: &str,
    title: &str,
    body: &str,
    pointer: Option<&str>,
    min_interval_s: u64,
) -> Verdict {
    notify_signal_via(
        &notify_signals_path(),
        now_unix(),
        min_interval_s,
        key,
        token,
        title,
        body,
        pointer,
        || notify_operator(title, body, pointer),
    )
}

/// The state machine behind `notify_signal`, with the send handed in - the
/// same seam the proven Python tests used at `send_notification`.
pub fn notify_signal_via(
    path: &Path,
    now: u64,
    min_interval_s: u64,
    key: &str,
    token: &str,
    _title: &str,
    _body: &str,
    _pointer: Option<&str>,
    send: impl FnOnce() -> bool,
) -> Verdict {
    let mut store = load_store(path);
    let entry = store.get(key).cloned();
    if let Some(entry) = &entry {
        if entry.get("token").and_then(Value::as_str) == Some(token) {
            return Verdict::Deduped;
        }
    }
    if let Some(entry) = &entry {
        if age_seconds(entry.get("ts").and_then(Value::as_str).unwrap_or(""), now) < min_interval_s
        {
            return Verdict::RateHeld;
        }
    }
    // Send first, commit only on an accepted send: a notice that never left
    // the machine must not leave state saying it did - the next pass retries.
    if !send() {
        return Verdict::SendFailed;
    }
    store.insert(
        key.to_string(),
        json!({"token": token, "ts": crate::events::now_rfc3339()}),
    );
    if let Err(e) = write_store(path, &store) {
        eprintln!("fno-agents: notify signal state write failed: {e}");
    }
    Verdict::Sent
}

/// Drop one signal's stored state, so the next change sends again. The empty
/// side of a signal (queue drained) calls this instead of notifying: silence
/// about nothing is the designed quiet.
pub fn forget(key: &str) {
    forget_at(&notify_signals_path(), key)
}

pub fn forget_at(path: &Path, key: &str) {
    let mut store = load_store(path);
    if store.remove(key).is_some() {
        if let Err(e) = write_store(path, &store) {
            eprintln!("fno-agents: notify signal state write failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// The notify_watch arm
// ---------------------------------------------------------------------------

/// Board queues the arm subscribes to: key -> (pointer verb, count label).
const BOARD_QUEUES: [(&str, &str, &str); 3] = [
    (
        "operator_question",
        "fno inbox outstanding",
        "open operator question(s)",
    ),
    (
        "mergeable_pr",
        "fno inbox board",
        "mergeable PR(s) with no live driver",
    ),
    (
        "undriven_pr",
        "fno inbox board",
        "PR(s) undriven across checks",
    ),
];

const CHECK_BAD: [&str; 4] = ["failure", "timed_out", "startup_failure", "action_required"];
const CHECK_GOOD: [&str; 3] = ["success", "neutral", "skipped"];

/// A commit younger than this with an empty check-run list is still in its
/// launch window, not a "jobs never ran" state. Path-filtered CI stays empty
/// forever, so the arm waits out the window once and then reports it.
const EMPTY_RUNS_GRACE_S: u64 = 900;

/// One pass. `acted` counts notices that left the machine; `skip_reason`
/// names only whole-lane trouble - dedupe is the designed quiet, never a skip.
pub fn run_notify_watch(
    signals: &[String],
    roots: &[PathBuf],
    min_interval_s: u64,
) -> WatchOutcome {
    let mut acted: u64 = 0;
    let mut notes: Vec<String> = Vec::new();
    let mut skip: Option<String> = None;
    let path = notify_signals_path();
    let now = now_unix();

    // Board lane: ONE in-process read; a queue at zero is forgotten, a change
    // in a queue's row set is a notice, an unreadable queue skips the whole
    // lane - never reported as empty.
    let board_keys: Vec<&str> = BOARD_QUEUES
        .iter()
        .map(|(k, _, _)| *k)
        .filter(|k| signals.iter().any(|s| s == *k))
        .collect();
    if !board_keys.is_empty() {
        let board = crate::king_board::read_board(&crate::king_board::BoardOpts::default());
        let queues: Map<String, Value> = board
            .get("queues")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|q| {
                        let name = q.get("name").and_then(Value::as_str)?;
                        Some((name.to_string(), q.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let unreadable = board_keys.iter().any(|k| {
            queues
                .get(*k)
                .map(|q| q.get("status").and_then(Value::as_str) == Some("unreadable"))
                .unwrap_or(false)
        });
        if unreadable {
            skip = Some("board_unreadable".to_string());
            notes.push("board:unreadable".to_string());
        } else {
            for (key, pointer, label) in BOARD_QUEUES.iter() {
                if !board_keys.contains(key) {
                    continue;
                }
                let Some(queue) = queues.get(*key) else {
                    notes.push(format!("{key}:queue_absent"));
                    continue;
                };
                let count = queue.get("count").and_then(Value::as_i64).unwrap_or(0);
                if count <= 0 {
                    forget_at(&path, key);
                    notes.push(format!("{key}:clear"));
                    continue;
                }
                let token = rows_token(queue.get("rows").and_then(Value::as_array));
                let body = format!("{count} {label}. {pointer}");
                let verdict = verdict_label(notify_signal_via(
                    &path,
                    now,
                    min_interval_s,
                    key,
                    &token,
                    &format!("operator: {label}"),
                    &body,
                    Some(pointer),
                    || notify_operator(&format!("operator: {label}"), &body, Some(pointer)),
                ));
                acted += u64::from(verdict == "sent");
                notes.push(format!("{key}:{verdict}"));
            }
        }
    }

    // Main CI lane: worst conclusion folded over the head commit's check
    // runs, with the run POPULATION in the token. A green that comes from
    // jobs not running must never read as a green from jobs passing - the
    // 2026-09-06 incident was a crates-only merge reading green because
    // cli-ci was path-filtered without crates/**.
    if signals.iter().any(|s| s == "main_ci") {
        if !gh_on_path() {
            if skip.is_none() {
                skip = Some("gh_absent".to_string());
            }
            notes.push("main_ci:gh_absent".to_string());
        } else {
            for root in roots {
                match main_ci_sample(root) {
                    None => notes.push(format!("main_ci:{}:unreadable", root.display())),
                    Some((key, token, body, pointer)) => {
                        let verdict = verdict_label(notify_signal_via(
                            &path,
                            now,
                            min_interval_s,
                            &key,
                            &token,
                            "main CI",
                            &body,
                            Some(&pointer),
                            || notify_operator("main CI", &body, Some(&pointer)),
                        ));
                        acted += u64::from(verdict == "sent");
                        notes.push(format!("{key}:{verdict}"));
                    }
                }
            }
        }
    }

    WatchOutcome {
        acted,
        skip_reason: skip,
        detail: notes.join("; ").chars().take(200).collect(),
    }
}

pub struct WatchOutcome {
    pub acted: u64,
    pub skip_reason: Option<String>,
    pub detail: String,
}

fn verdict_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Sent => "sent",
        Verdict::Deduped => "deduped",
        Verdict::RateHeld => "rate-held",
        Verdict::SendFailed => "send_failed",
    }
}

/// sha256 over the sorted row ids - membership change is state change.
fn rows_token(rows: Option<&Vec<Value>>) -> String {
    let mut ids: Vec<String> = rows
        .unwrap_or(&Vec::new())
        .iter()
        .filter(|r| r.is_object())
        .map(|r| {
            r.get("id")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .or_else(|| {
                    r.get("number")
                        .and_then(Value::as_i64)
                        .map(|n| n.to_string())
                })
                .unwrap_or_default()
        })
        .collect();
    ids.sort();
    let mut h = Sha256::new();
    for id in &ids {
        h.update(id.as_bytes());
        h.update(b"\n");
    }
    format!("{:x}", h.finalize())
}

/// Fold one repo's check runs into `(population, verdict)`, or None when
/// there is no state to report: a pending run is not a state (notifying on
/// it would spam), and an empty list inside the launch window is the same
/// not-a-state. An empty list past the grace is its own state and never
/// folds into success.
fn fold_check_runs(conclusions: &[&str], head_age_s: Option<u64>) -> Option<(usize, String)> {
    if conclusions.is_empty() {
        let old_enough = head_age_s
            .map(|age| age >= EMPTY_RUNS_GRACE_S)
            .unwrap_or(true);
        return if old_enough {
            Some((0, "none".to_string()))
        } else {
            None
        };
    }
    if conclusions.iter().any(|c| CHECK_BAD.contains(c)) {
        Some((conclusions.len(), "failure".to_string()))
    } else if conclusions.iter().all(|c| CHECK_GOOD.contains(c)) {
        Some((conclusions.len(), "success".to_string()))
    } else {
        None
    }
}

fn gh_on_path() -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| dir.join("gh").is_file())
}

/// One repo's main-CI sample: `(signal key, token, body)`, or None when the
/// world cannot be read or has no state yet. The git legs run INSIDE the repo
/// (the proven Python leg's first draft took no cwd and every sample died on
/// it); the https slug drops the host, or the gh api path is wrong.
fn main_ci_sample(root: &Path) -> Option<(String, String, String, String)> {
    let url = run_captured(
        "git",
        &["remote", "get-url", "origin"],
        root,
        Duration::from_secs(10),
    )?;
    let slug = slug_from_remote(url.trim());
    if slug.is_empty() {
        return None;
    }
    let branch_out = run_captured(
        "git",
        &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"],
        root,
        Duration::from_secs(10),
    )?;
    let branch = branch_out
        .trim()
        .strip_prefix("origin/")
        .unwrap_or(branch_out.trim())
        .to_string();
    let branch = if branch.is_empty() {
        "main".to_string()
    } else {
        branch
    };
    let ref_enc = percent_encode_ref(&branch);
    let runs_out = run_captured(
        "gh",
        &["api", &format!("repos/{slug}/commits/{ref_enc}/check-runs")],
        root,
        Duration::from_secs(30),
    )?;
    let runs: Vec<Value> = serde_json::from_str::<Value>(runs_out.trim())
        .ok()?
        .get("check_runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let conclusions: Vec<&str> = runs
        .iter()
        .filter_map(|r| r.get("conclusion").and_then(Value::as_str))
        .collect();
    let head_age_s = if conclusions.is_empty() {
        Some(head_commit_age_s(root, &slug, &ref_enc)?)
    } else {
        None
    };
    let (population, verdict) = fold_check_runs(&conclusions, head_age_s)?;
    let token = format!("{population}:{verdict}");
    let body = if population == 0 {
        format!("main CI on {slug}: no check runs (population 0); jobs may not have run.")
    } else {
        format!("main CI on {slug}: {verdict} ({population} check run(s)).")
    };
    Some((
        format!("main_ci:{slug}"),
        token,
        body,
        format!("https://github.com/{slug}/actions"),
    ))
}

/// Unix age of the head commit's committer date; an unreadable date reads as
/// old (the empty state reports rather than hiding behind a parse failure).
fn head_commit_age_s(root: &Path, slug: &str, ref_enc: &str) -> Option<u64> {
    let out = run_captured(
        "gh",
        &["api", &format!("repos/{slug}/commits/{ref_enc}")],
        root,
        Duration::from_secs(30),
    )?;
    let head: Value = serde_json::from_str(out.trim()).ok()?;
    let date = head
        .pointer("/commit/committer/date")
        .and_then(Value::as_str)
        // Some histories carry only an author date.
        .or_else(|| head.pointer("/commit/author/date").and_then(Value::as_str))?;
    let then = crate::tick_ledger::parse_rfc3339_unix(&date)?;
    Some(now_unix().saturating_sub(then))
}

/// `owner/repo` from an https or scp-form git remote url, empty when unclear.
fn slug_from_remote(url: &str) -> String {
    let mut u = url.trim().trim_end_matches(".git").trim().to_string();
    if let Some(pos) = u.find("://") {
        u = u[pos + 3..].to_string();
        u = match u.find('/') {
            Some(p) => u[p + 1..].to_string(),
            None => String::new(),
        };
    } else {
        u = match u.find(':') {
            Some(p) => u[p + 1..].to_string(),
            None => String::new(),
        };
    }
    let u = u.trim_matches('/');
    if u.matches('/').count() == 1 {
        u.to_string()
    } else {
        String::new()
    }
}

/// Percent-encode a ref for a gh api path: unreserved bytes pass, everything
/// else rides its UTF-8 percent triple (a branch named `feature/x` is one
/// path segment, not two).
fn percent_encode_ref(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Run one capture command inside `cwd`, bounded by `timeout`, stdout
/// drained on its own thread so a chatty child cannot deadlock the wait.
/// Returns stdout regardless of exit status; the caller judges the content.
fn run_captured(bin: &str, args: &[&str], cwd: &Path, timeout: Duration) -> Option<String> {
    let mut child = crate::bounded_spawn::spawn_bounded(OsStr::new(bin), args, cwd).ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        use std::io::Read;
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Some(reader.join().unwrap_or_default()),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// The verb
// ---------------------------------------------------------------------------

/// `fno-agents notify-watch --json [--root PATH]...`: run one pass, print the
/// receipt, exit 0 - an honest skip is the tick row's business, never an
/// error. Not a routable `fno agents` verb (matched with `==` beside `board`),
/// so the routable-verb parity guard sees no new advertised verb.
pub fn run_notify_watch_verb(args: &[String]) -> i32 {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" | "-J" => {}
            "--root" => match it.next() {
                Some(r) => roots.push(PathBuf::from(r)),
                None => {
                    eprintln!("fno-agents notify-watch: --root needs a path");
                    return 2;
                }
            },
            other => {
                eprintln!("fno-agents notify-watch: unknown flag {other}");
                return 2;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let signals = crate::agents_config::notify_signals(&cwd);
    if signals.is_empty() {
        println!(
            "{}",
            json!({"acted": 0, "skip_reason": "notify_off", "detail": "[notify] signals empty"})
        );
        return 0;
    }
    let floor = crate::agents_config::notify_min_interval_s(&cwd);
    let out = run_notify_watch(&signals, &roots, floor);
    println!(
        "{}",
        json!({"acted": out.acted, "skip_reason": out.skip_reason, "detail": out.detail})
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::test_env_lock;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-operator-notice-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p.set_extension("json");
        p
    }

    fn sent_once(counter: &mut u32) -> impl FnOnce() -> bool + '_ {
        move || {
            *counter += 1;
            true
        }
    }

    #[test]
    fn same_token_dedupes_and_sends_nothing() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_path("dedupe");
        let mut sends: u32 = 0;
        assert!(matches!(
            notify_signal_via(
                &path,
                1_000,
                300,
                "k",
                "t1",
                "T",
                "B",
                None,
                sent_once(&mut sends)
            ),
            Verdict::Sent
        ));
        assert_eq!(sends, 1);
        assert!(matches!(
            notify_signal_via(
                &path,
                1_100,
                300,
                "k",
                "t1",
                "T",
                "B",
                None,
                sent_once(&mut sends)
            ),
            Verdict::Deduped
        ));
        assert_eq!(sends, 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn changed_token_inside_floor_is_held_not_dropped() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_path("held");
        let mut sends: u32 = 0;
        notify_signal_via(
            &path,
            1_000,
            3_600,
            "k",
            "t1",
            "T",
            "B",
            None,
            sent_once(&mut sends),
        );
        assert!(matches!(
            notify_signal_via(
                &path,
                1_060,
                3_600,
                "k",
                "t2",
                "T",
                "B",
                None,
                sent_once(&mut sends)
            ),
            Verdict::RateHeld
        ));
        assert_eq!(sends, 1, "held, not sent");
        // The token was NOT written: the next pass after the floor reconsiders t2.
        let store = load_store(&path);
        assert_eq!(store["k"]["token"], "t1");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn changed_token_after_floor_sends() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_path("after-floor");
        let mut sends: u32 = 0;
        notify_signal_via(
            &path,
            1_000,
            0,
            "k",
            "t1",
            "T",
            "B",
            None,
            sent_once(&mut sends),
        );
        assert!(matches!(
            notify_signal_via(
                &path,
                1_001,
                0,
                "k",
                "t2",
                "T",
                "B",
                None,
                sent_once(&mut sends)
            ),
            Verdict::Sent
        ));
        assert_eq!(sends, 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn failed_send_rolls_back_so_the_next_pass_retries() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_path("rollback");
        assert!(matches!(
            notify_signal_via(&path, 1_000, 300, "k", "t1", "T", "B", None, || false),
            Verdict::SendFailed
        ));
        let store = load_store(&path);
        assert!(
            store.get("k").is_none(),
            "no state for a notice that never left"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn failed_spawn_through_the_real_path_rolls_back() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let signals_path = temp_path("rollback-real");
        let bin = std::env::var_os("FNO_BIN");
        let prev = std::env::var_os("FNO_NOTIFY_SIGNALS");
        std::env::set_var("FNO_BIN", "/nonexistent/fno-binary-for-tests");
        std::env::set_var("FNO_NOTIFY_SIGNALS", &signals_path);
        let verdict = notify_signal("k", "t1", "T", "B", None, 300);
        match bin {
            Some(v) => std::env::set_var("FNO_BIN", v),
            None => std::env::remove_var("FNO_BIN"),
        }
        match prev {
            Some(v) => std::env::set_var("FNO_NOTIFY_SIGNALS", v),
            None => std::env::remove_var("FNO_NOTIFY_SIGNALS"),
        }
        assert!(matches!(verdict, Verdict::SendFailed));
        assert!(
            load_store(&signals_path).get("k").is_none(),
            "a spawn that failed must not commit state"
        );
        std::fs::remove_file(&signals_path).ok();
    }

    #[test]
    fn forget_lets_the_next_change_send() {
        let _lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = temp_path("forget");
        let mut sends: u32 = 0;
        notify_signal_via(
            &path,
            1_000,
            300,
            "k",
            "t1",
            "T",
            "B",
            None,
            sent_once(&mut sends),
        );
        forget_at(&path, "k");
        assert!(matches!(
            notify_signal_via(
                &path,
                1_100,
                300,
                "k",
                "t1",
                "T",
                "B",
                None,
                sent_once(&mut sends)
            ),
            Verdict::Sent
        ));
        assert_eq!(sends, 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rows_token_changes_when_membership_changes() {
        let rows_a = vec![json!({"id": "q-2"}), json!({"id": "q-1"})];
        let rows_b = vec![json!({"id": "q-1"}), json!({"id": "q-2"})];
        let rows_c = vec![json!({"id": "q-1"}), json!({"number": 7})];
        assert_eq!(rows_token(Some(&rows_a)), rows_token(Some(&rows_b)));
        assert_ne!(rows_token(Some(&rows_a)), rows_token(Some(&rows_c)));
        assert_ne!(rows_token(Some(&rows_a)), rows_token(None));
    }

    #[test]
    fn slug_from_remote_handles_both_url_forms() {
        assert_eq!(
            slug_from_remote("git@github.com:owner/repo.git"),
            "owner/repo"
        );
        assert_eq!(
            slug_from_remote("https://github.com/owner/repo.git"),
            "owner/repo"
        );
        assert_eq!(
            slug_from_remote("ssh://git@github.com/owner/repo.git"),
            "owner/repo"
        );
        assert_eq!(slug_from_remote("https://github.com/owner"), "");
        assert_eq!(slug_from_remote(""), "");
    }

    #[test]
    fn fold_reports_population_and_keeps_empty_out_of_success() {
        let good = ["success", "neutral"];
        assert_eq!(
            fold_check_runs(&good, None),
            Some((2, "success".to_string()))
        );
        let bad = ["success", "failure"];
        assert_eq!(
            fold_check_runs(&bad, None),
            Some((2, "failure".to_string()))
        );
        // Pending is not a state.
        assert_eq!(fold_check_runs(&["success", "in_progress"], None), None);
        // Empty inside the launch window is not a state...
        assert_eq!(fold_check_runs(&[], Some(60)), None);
        // ...but past the grace it is its own token, never success.
        assert_eq!(
            fold_check_runs(&[], Some(EMPTY_RUNS_GRACE_S)),
            Some((0, "none".to_string()))
        );
        assert_eq!(
            fold_check_runs(&[], Some(3_600)),
            Some((0, "none".to_string()))
        );
        // Unreadable head age reads as old: the state reports.
        assert_eq!(fold_check_runs(&[], None), Some((0, "none".to_string())));
    }

    #[test]
    fn percent_encode_ref_keeps_a_slashed_branch_one_segment() {
        assert_eq!(percent_encode_ref("main"), "main");
        assert_eq!(percent_encode_ref("feature/x"), "feature%2Fx");
        assert_eq!(percent_encode_ref("rel-1.2_3~4"), "rel-1.2_3~4");
    }
}
