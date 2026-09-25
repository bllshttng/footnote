//! The status coalescing cache and the status door: one row per
//! (repo, PR, head), flock-protected, refreshed at most once per TTL, served
//! degraded inside a fleet backoff. Ported from `fno.pr._cache`.
//!
//! Code default, deliberately not operator config: TTL 60s. Env overrides
//! exist for tests and one-off tuning: FNO_PR_STATUS_TTL,
//! FNO_PR_STATUS_CACHE_DIR.

use super::compose::{serve_notes, verdict_line};
use super::RestReason;
use crate::pr_status_facts::GhProbe;
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// One status read must not become a hundred gh calls; the counting probe
/// feeds the spend note.
pub(crate) struct CountingProbe<P: GhProbe> {
    inner: P,
    pub(crate) calls: AtomicUsize,
}

impl<P: GhProbe> GhProbe for CountingProbe<P> {
    fn run_gh(&self, cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.run_gh(cwd, args)
    }
}

fn ttl() -> u64 {
    let raw = std::env::var("FNO_PR_STATUS_TTL").unwrap_or_default();
    raw.parse::<u64>().map(|v| v.max(1)).unwrap_or(60)
}

/// The one dir resolver: the shared config helper already folds the env
/// override and the state root; the door hands it the payload's cwd.
fn cache_dir(cwd: &Path) -> Option<PathBuf> {
    crate::agents_config::pr_status_cache_dir(cwd)
}

/// A row's `{ts, exit, output}` or None: a corrupt row is a miss, never an
/// error (the network read is the truth).
fn read_row(dir: &Path, key: &str) -> Option<Value> {
    let row = std::fs::read_to_string(dir.join(format!("{key}.json"))).ok()?;
    let parsed: Value = serde_json::from_str(&row).ok()?;
    if parsed.is_object() {
        Some(parsed)
    } else {
        None
    }
}

/// A numeric row field through the finite-or-zero guard.
fn num(row: &Value, key: &str) -> f64 {
    row.get(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// Every row file for (slug_key, pr), newest mtime first.
fn rows_newest_first(dir: &Path, slug_key: &str, pr: &str) -> Vec<PathBuf> {
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let prefix = format!("{slug_key}-{pr}-");
        for entry in rd.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with(&prefix) && name.ends_with(".json") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        candidates.push((mtime, path));
                    }
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().map(|(_, p)| p).collect()
}

fn write_row(dir: &Path, key: &str, row: &Value) {
    let p = dir.join(format!("{key}.json"));
    let tmp = p.with_extension("tmp");
    if std::fs::File::create(&tmp)
        .ok()
        .and_then(|mut f| f.write_all(row.to_string().as_bytes()).ok())
        .is_some()
    {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Print one cached row, degraded to unknown when `stale`, and answer the
/// verb's exit code; None when the row is not servable (empty output or a
/// foreign schema: a miss, checked before any write).
fn serve(row: &Value, stale: bool) -> Option<(i32, String, String)> {
    let mut out = row.get("output").cloned().unwrap_or(Value::Null);
    if !out.is_object() || out.as_object().unwrap().is_empty() {
        return None;
    }
    let exit_raw = row.get("exit");
    let code = match exit_raw {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(-2) as i32,
        _ => return None,
    };
    if code < 0 {
        return None;
    }
    let head_unverified = row.get("head_unverified").and_then(Value::as_bool) == Some(true);
    let (code, degraded) = if stale || head_unverified {
        (
            3,
            json!({
                "stale_verdict": out.get("verdict").cloned().unwrap_or(Value::Null),
                "verdict": "unknown",
                "green": false,
                "settled": false,
                "ready": false,
                "ready_blockers": ["status_stale"],
                "stale_reason": "secondary rate limit backoff - the check set is unreadable, so this is the last cached row degraded to unknown, not a verdict",
            }),
        )
    } else {
        (code, Value::Null)
    };
    if degraded.is_object() {
        for (k, v) in degraded.as_object().unwrap() {
            out[k] = v.clone();
        }
        if let Some(obj) = out.as_object_mut() {
            obj.remove("failures");
        }
    }
    out["cached"] = json!(true);
    let ts = num(row, "ts");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    out["cached_age_seconds"] = if ts > 0.0 {
        json!(((now - ts) as i64).max(0))
    } else {
        Value::Null
    };
    out["cached_at"] = if ts > 0.0 {
        json!(chrono::DateTime::from_timestamp(ts as i64, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()))
    } else {
        Value::Null
    };
    let stdout = format!("{}\n", serde_json::to_string(&out).unwrap_or_default());
    let mut stderr = vec![verdict_line(&out)];
    stderr.extend(serve_notes(&out));
    Some((code, stdout, stderr.join("\n")))
}

/// The gh probe wrapper this module's live reads ride: `&GhProbe` objects.
pub(crate) struct ProbeRef<'a>(pub(crate) &'a dyn GhProbe);

impl GhProbe for ProbeRef<'_> {
    fn run_gh(&self, cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
        self.0.run_gh(cwd, args)
    }
}

/// The live read behind the cache: `run_status`'s whole flow, returning the
/// exit code, the payload, and the stderr lines. `prior` is the same-head
/// previous payload (detail and rerun facts reused within one head only).
pub(crate) fn live_status<P: GhProbe>(
    probe: &CountingProbe<P>,
    cwd: &Path,
    pr: u64,
    prior: Option<&Value>,
    slug: &str,
) -> (i32, Value, Vec<String>, usize) {
    let (code, payload, stderr) = super::status_payload(probe, cwd, pr, prior, slug);
    (code, payload, stderr, probe.calls.load(Ordering::SeqCst))
}

// ---------------------------------------------------------------------------
// The status door
// ---------------------------------------------------------------------------

/// The status door: one op dispatch for the verb's stdin payload. Only
/// `status-read` today; the wait, logs, and ci ops land with their ports.
pub(crate) fn run_door(op: &str, payload: &Value) -> (i32, String, String) {
    match op {
        "status-read" => status_read(payload),
        _ => (2, String::new(), format!("unknown status door op {op}\n")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn status_read(payload: &Value) -> (i32, String, String) {
    let cwd_str = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
    let pr = payload.get("pr").and_then(Value::as_u64).unwrap_or(0);
    let refresh = payload
        .get("refresh")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (code, out, stderr_lines, calls) = cached_status(cwd_str, pr, refresh);
    let snap = crate::gh_budget::snapshot(&crate::gh_budget::ledger_path(), now_ms());
    let mut stderr = stderr_lines.join("\n");
    if !stderr.is_empty() {
        stderr.push('\n');
    }
    stderr.push_str(&format!(
        "note: {calls} gh call(s) this invocation, fleet budget {} of {} points in the last 60s\n",
        snap.points_60s, snap.cap
    ));
    let stdout = format!("{}\n", serde_json::to_string(&out).unwrap_or_default());
    (code, stdout, stderr)
}

/// `cached_status`: the coalescing chokepoint. Head-keyed rows, one live
/// read per TTL under the per-key flock, a zero-network backoff pre-check,
/// and the fail-closed stale serve when the head read itself refuses.
fn cached_status(cwd_str: &str, pr: u64, refresh: bool) -> (i32, Value, Vec<String>, usize) {
    let cwd = Path::new(cwd_str);
    // The slug resolves once here; the reader never runs git. No repo
    // context: serve uncached rather than key every caller onto one row.
    let slug = git_slug(cwd);
    let (Some(slug), true) = (slug, pr > 0) else {
        let probe = fresh_probe();
        let (code, out, lines) = super::status_payload(&probe, cwd, pr, None, "");
        return (code, out, lines, probe.calls.load(Ordering::SeqCst));
    };
    let slug_key = slug.replace('/', "--");
    let Some(dir) = cache_dir(cwd) else {
        let probe = fresh_probe();
        let (code, out, lines) = super::status_payload(&probe, cwd, pr, None, &slug);
        return (code, out, lines, probe.calls.load(Ordering::SeqCst));
    };
    let _ = std::fs::create_dir_all(&dir);
    let now = now_secs();
    // Backoff pre-check, zero network: inside a live refusal every waiter's
    // tick short-circuits to the newest cached row instead of re-attempting.
    if !refresh
        && crate::gh_budget::snapshot(&crate::gh_budget::ledger_path(), now_ms())
            .backoff_remaining_s
            > 0
    {
        if let Some(row) = newest_row(&dir, &slug_key, &pr.to_string()) {
            let stale = now - num(&row, "ts") >= ttl() as f64;
            if let Some(answer) = serve(&row, stale) {
                return into_answer(answer, 0);
            }
        }
    }
    // The light head read that mints the key. A refusal fails CLOSED: the
    // newest row degraded, or the loud live read when there is no row.
    let probe = fresh_probe();
    let head = read_head(&probe, cwd, &slug, pr);
    let (head_sha, pr_state) = match &head {
        Ok(info) => (
            info.get("head_sha")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            info.get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        Err(_) => {
            if refresh {
                return live_through(cwd, pr, None, &slug, &dir, "");
            }
            if let Some(row) = newest_row(&dir, &slug_key, &pr.to_string()) {
                if let Some(answer) = serve(&row, true) {
                    return into_answer(answer, probe.calls.load(Ordering::SeqCst));
                }
            }
            return live_through(cwd, pr, None, &slug, &dir, "");
        }
    };
    let key = mint_key(cwd, pr, &head_sha, &pr_state, &slug_key);
    if !refresh {
        if let Some(row) = read_row(&dir, &key) {
            if now - num(&row, "ts") < ttl() as f64 {
                if let Some(answer) = serve(&row, false) {
                    return into_answer(answer, probe.calls.load(Ordering::SeqCst));
                }
            }
        }
    }
    // Miss: the ONE live read under the per-key lock; queued pollers re-read
    // the fresh row after.
    let lock_path = dir.join(format!("{key}.lock"));
    let _lock = crate::gh_budget::FileLock::acquire(&lock_path);
    let prior = read_row(&dir, &key)
        .and_then(|row| row.get("output").cloned())
        .filter(|v| v.is_object());
    if !refresh {
        if let Some(row) = read_row(&dir, &key) {
            if now_secs() - num(&row, "ts") < ttl() as f64 {
                if let Some(answer) = serve(&row, false) {
                    return into_answer(answer, probe.calls.load(Ordering::SeqCst));
                }
            }
        }
    }
    live_through(cwd, pr, prior.as_ref(), &slug, &dir, &key)
}

fn fresh_probe() -> CountingProbe<crate::pr_status_facts::RealGhProbe> {
    CountingProbe {
        inner: crate::pr_status_facts::RealGhProbe,
        calls: AtomicUsize::new(0),
    }
}

fn git_slug(cwd: &Path) -> Option<String> {
    let url = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    crate::merge_gates::repo_slug_from_origin(&url)
}

fn newest_row(dir: &Path, slug_key: &str, pr: &str) -> Option<Value> {
    let path = rows_newest_first(dir, slug_key, pr).into_iter().next()?;
    let row = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&row).ok()
}

/// The light read that mints the key: the PR's head sha and state, one call.
fn read_head<P: GhProbe>(probe: &P, cwd: &Path, slug: &str, pr: u64) -> Result<Value, RestReason> {
    let args = vec!["api".to_string(), format!("repos/{slug}/pulls/{pr}")];
    let (ok, stdout, stderr) = probe.run_gh(cwd, &args).map_err(|e| RestReason {
        text: e,
        rate_limit_class: String::new(),
    })?;
    if !ok {
        return Err(super::rest_reason(probe, cwd, &stderr));
    }
    let pulls: Value = serde_json::from_str(&stdout).map_err(|e| RestReason {
        text: format!("unparseable PR read: {e}"),
        rate_limit_class: String::new(),
    })?;
    let head_sha = pulls
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if head_sha.is_empty() {
        return Err(RestReason {
            text: "the PR read carried no head sha".into(),
            rate_limit_class: String::new(),
        });
    }
    let state = match pulls.get("state").and_then(Value::as_str).unwrap_or("") {
        "open" => "OPEN".to_string(),
        "closed" => {
            if pulls.get("merged").and_then(Value::as_bool) == Some(true)
                || pulls.get("merged_at").and_then(Value::as_str).is_some()
            {
                "MERGED".to_string()
            } else {
                "CLOSED".to_string()
            }
        }
        _ => "UNKNOWN".to_string(),
    };
    Ok(json!({"head_sha": head_sha, "state": state}))
}

fn mint_key(cwd: &Path, pr: u64, head_sha: &str, pr_state: &str, slug_key: &str) -> String {
    let payload = json!({
        "cwd": cwd.display().to_string(),
        "pr": pr,
        "head_sha": head_sha,
        "pr_state": pr_state,
        "slug": slug_key,
    });
    let out = crate::pr_status_facts::status_cache_key(&payload);
    if let Some(key) = out
        .get("key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
    {
        return key.to_string();
    }
    format!("{slug_key}-{pr}-{}", &head_sha[..head_sha.len().min(12)])
}

/// The live read the row is written from, plus the write and the prune of
/// superseded heads' rows. A refused read (exit 4) writes nothing.
fn live_through(
    cwd: &Path,
    pr: u64,
    prior: Option<&Value>,
    slug: &str,
    dir: &Path,
    key: &str,
) -> (i32, Value, Vec<String>, usize) {
    let probe = fresh_probe();
    let (code, out, lines) = super::status_payload(&probe, cwd, pr, prior, slug);
    let calls = probe.calls.load(Ordering::SeqCst);
    if code == 4 && out.get("rate_limit_class").and_then(Value::as_str) == Some("secondary") {
        let _ = crate::gh_budget::record_refusal(&crate::gh_budget::ledger_path(), now_ms());
    }
    if code != 4 && !key.is_empty() && out.is_object() {
        let row = json!({"ts": now_secs(), "exit": code, "output": out});
        write_row(dir, key, &row);
        let slug_key = slug.replace('/', "--");
        prune_rows(dir, &slug_key, &pr.to_string(), key);
    }
    (code, out, lines, calls)
}

fn prune_rows(dir: &Path, slug_key: &str, pr: &str, keep_key: &str) {
    for path in rows_newest_first(dir, slug_key, pr) {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem != keep_key {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn into_answer(answer: (i32, String, String), calls: usize) -> (i32, Value, Vec<String>, usize) {
    let (code, stdout, stderr) = answer;
    let payload = serde_json::from_str::<Value>(stdout.trim()).unwrap_or(Value::Null);
    let lines: Vec<String> = stderr.lines().map(str::to_string).collect();
    let _ = calls;
    (code, payload, lines, 0)
}
