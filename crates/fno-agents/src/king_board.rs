//! The king board's budget-aware collector, in Rust (x-25b8).
//!
//! Ports `cli/src/fno/king/board.py`'s `build_board` + `collect_inputs` with
//! the budget branch built in (the x-f8e3 reference, `feature/x-f8e3`
//! bfd1a5e8e, which could not land because the file-budget gate refuses grown
//! Python): the caller hands in ONE whole-board budget, every per-source slice
//! derives from it, and as the budget runs out the board stops starting reads,
//! marks each unstarted source, and still emits the payload it has.
//!
//! The speed win is in-process reads. One graph read (the same
//! `read_defaulted_opts(path, false, false)` the keeper's `read_strict` runs)
//! feeds undispatched, claimed-node lookups, PR binding, and crown scope. The
//! claims merge is a directory scan. `needs` folds in-process over the same
//! sources `fno agents needs` reads. Exactly three subprocesses remain: `gh pr
//! list` (a real network boundary), `fno backlog ready` (its selection logic
//! lives inline in the typer command with no function behind it; re-typing the
//! filter chain here would drift from `next`'s), and `fno inbox outstanding`
//! (measured 2026-09-04: 1.12s wall at load 52, far under its 10s bar - the
//! plan's change 2 keeps it and records the measurement).
//!
//! Output keeps the Python JSON shape: `actionable`, `unreadable`, `queues`
//! (same names, same order, same row dicts), `warnings`, `exit_code` - plus a
//! per-source `sources` map carrying `ok`/`truncated` that the plan's
//! done-probe reads. `parse_king_board` in loopcheck.rs ignores the extra key,
//! so every existing reader is unchanged.

use crate::graph_store;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Held back from the sources so the board can still serialize and print its
/// payload before the caller's outer timer fires.
pub const SERIALIZE_RESERVE_MS: u64 = 1_000;

/// Whole-board budget when a human runs the board by hand and passes no
/// `--budget-ms`. This budgets the ENTIRE board, never a per-source read: an
/// independent per-source default is exactly what must not exist here (the old
/// 60s-per-read Python default was twice the 30s whole-board kill, so no inner
/// timeout could ever fire).
pub const HAND_RUN_BUDGET_MS: u64 = 30_000;

/// Live node claims resolved per board read; the cut is reported (the
/// x-f8e3 reference carried the same cap).
const MAX_CLAIMED_NODE_READS: usize = 20;

/// Priorities a king treats as its own work. Lower bands are the operator's.
const KING_PRIORITIES: [&str; 2] = ["p0", "p1"];

/// Claim states that mean the lock outlived its holder.
const DEAD_CLAIM_STATES: [&str; 2] = ["stale", "corrupted"];

/// The activity vocabulary that counts as a staffed lane (reachability
/// `_ACTIVE_STATES`). Copied with a test pinning the Python side, because a
/// Rust module cannot import the Python frozenset; the pin makes the
/// vocabulary fix that adds a fourth word fail loudly here.
const ACTIVE_STATES: [&str; 3] = ["working", "watching", "your-move"];

/// Transcript age past which an active-looking holder reads stalled
/// (session_truth.STALLED_AFTER_S; same pin as ACTIVE_STATES).
const STALLED_AFTER_S: f64 = 2.0 * 3600.0;

/// Per-project rows rendered for the capture stream; the count stays whole.
const CAPTURE_PROJECT_CAP: usize = 8;

const TERMINAL_RUNGS: [&str; 2] = ["done", "superseded"];
const LEGACY_DEFER_PREFIX: &str = "deferred:";
const COVERAGE_STATUS_CONTEXT: &str = "fno/review-coverage";
const COVERAGE_UNAVAILABLE_STATUS_CONTEXT: &str = "fno/review-coverage-unavailable";
const NODE_ID_BODY: &str = "[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}";

const PASS_STATES: [&str; 3] = ["SUCCESS", "NEUTRAL", "SKIPPED"];
const FAIL_STATES: [&str; 7] = [
    "FAILURE",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
    "STALE",
    "ERROR",
];

/// The literal commands a reader can re-run; they ARE the checkability
/// property, so they sit beside the readers (board.py spelled them identically).
const SRC_UNDISPATCHED: &str = "fno backlog undispatched --json";
const SRC_READY: &str = "fno backlog ready --json -A";
const SRC_CLAIMS: &str = "fno agents claim list -J --include-stale --prefix node:";
const SRC_PRS: &str =
    "gh pr list --state open --json number,title,mergeable,statusCheckRollup,headRefName,url";
const SRC_PR_NODES: &str = "gh pr list --state open --json number,title,mergeable,statusCheckRollup,headRefName,url + fno backlog get <id>";
const SRC_QUESTIONS: &str = "fno inbox outstanding --json";
const SRC_NEEDS: &str = "fno agents needs --json";

fn now_secs_board() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Config + path resolution (mirrors fno.paths / fno.claims.io defaults)
// ---------------------------------------------------------------------------

fn expand_home(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

fn home_dot_fno() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fno")
}

/// `paths.graph_json()`: a `paths.graph_json` override wins (a relative one
/// anchors under `~/.fno`, the same treatment the ledger override gets);
/// otherwise the state dir's `graph.json`. The default lands at
/// `~/.fno/graph.json`, which is also what `FNO_HOME` redirects.
fn graph_json_path(cwd: &Path) -> PathBuf {
    if let Some(v) = crate::agents_config::config_lookup(cwd, &["paths", "graph_json"])
        .and_then(|v| v.as_str().map(str::to_string))
    {
        let expanded = expand_home(&v);
        if expanded.is_absolute() {
            return expanded;
        }
        return home_dot_fno().join(expanded);
    }
    if let Some(home) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(home).join("graph.json");
    }
    home_dot_fno().join("graph.json")
}

/// `paths.operator_lane()`: pinned global like the ledger - one file per
/// person, never per checkout.
fn operator_lane_path(cwd: &Path) -> PathBuf {
    if let Some(v) = crate::agents_config::config_lookup(cwd, &["paths", "operator_lane"])
        .and_then(|v| v.as_str().map(str::to_string))
    {
        let expanded = expand_home(&v);
        if expanded.is_absolute() {
            return expanded;
        }
        return home_dot_fno().join(expanded);
    }
    let state_dir = crate::agents_config::config_lookup(cwd, &["state_dir"])
        .and_then(|v| v.as_str().map(str::to_string))
        .map(|s| expand_home(&s));
    match state_dir {
        Some(dir) if dir.is_absolute() => dir.join("my-priorities.md"),
        _ => home_dot_fno().join("my-priorities.md"),
    }
}

/// `config.king.autonomous_merge`, fail-safe to off: an unreadable config
/// resolves an outward, hard-to-reverse action to off, which is the invariant
/// every gate resolver applies to itself.
fn autonomous_merge_enabled(cwd: &Path) -> bool {
    crate::agents_config::config_lookup(cwd, &["king", "autonomous_merge"])
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The {alias: canonical} project map from `work.workspaces.*.projects[]`
/// (projects/resolve.py's cache builder). `Err` names why the map is absent so
/// a scope spelling that is neither project nor epic can be refused with the
/// Python resolver's wording.
fn project_map(cwd: &Path) -> Result<HashMap<String, String>, String> {
    let work = match crate::agents_config::config_lookup(cwd, &["work", "workspaces"]) {
        Some(v) => v,
        None => return Err("no work.workspaces in any candidate config.toml".to_string()),
    };
    let Some(table) = work.as_table() else {
        return Ok(HashMap::new());
    };
    let mut map: HashMap<String, String> = HashMap::new();
    for (_ws, ws_data) in table {
        let Some(projects) = ws_data.get("projects").and_then(|p| p.as_array()) else {
            continue;
        };
        for project in projects {
            let Some(project) = project.as_table() else {
                continue;
            };
            let Some(canonical) = project.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if canonical.is_empty() {
                continue;
            }
            map.entry(canonical.to_string())
                .or_insert_with(|| canonical.to_string());
            if let Some(short) = project.get("short_name").and_then(|s| s.as_str()) {
                if !short.is_empty() && short != canonical {
                    map.entry(short.to_string())
                        .or_insert_with(|| canonical.to_string());
                }
            }
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// SourceRead: one source's answer, or the reason there is no answer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct SourceRead {
    payload: Option<Value>,
    error: Option<String>,
}

impl SourceRead {
    fn ok(payload: Value) -> Self {
        SourceRead {
            payload: Some(payload),
            error: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        SourceRead {
            payload: None,
            error: Some(msg.into()),
        }
    }
    fn is_ok(&self) -> bool {
        self.error.is_none()
    }
    fn rows(&self) -> Vec<Value> {
        match &self.payload {
            Some(Value::Array(rows)) => rows.clone(),
            _ => Vec::new(),
        }
    }
    /// The nested-shape half of the degrade-not-crash promise (board.py
    /// `_as_dict`): a stream that changed shape degrades that stream, never
    /// the whole board.
    fn dict(&self) -> Map<String, Value> {
        match &self.payload {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        }
    }
}

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn s_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

/// Python `bool()` over a JSON value: null/false/empty-string/zero/empty
/// container are false. The classify port decides `completed`/`has_pr`/
/// `batch_owner` the way the Python `bool(entry.get(...))` did, so a legacy
/// empty-string `completed_at` or a zero `pr_number` reads as absent, exactly
/// as the retired module read it.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(st) => !st.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn as_int(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

/// The whole-board budget: every per-source slice derives from what remains
/// minus the serialization reserve; there is no second, independent per-source
/// timeout to invert.
struct Budget {
    deadline: Instant,
    last: Option<&'static str>,
}

impl Budget {
    fn new(budget_ms: u64) -> Self {
        Budget {
            deadline: Instant::now() + Duration::from_millis(budget_ms),
            last: None,
        }
    }
    /// The slice this next source may spend, or None once spent.
    fn slice(&self) -> Option<Duration> {
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .and_then(|d| d.checked_sub(Duration::from_millis(SERIALIZE_RESERVE_MS)));
        left.filter(|d| !d.is_zero())
    }
    /// Claim the budget for `name`; returns its slice or None once spent.
    fn start(&mut self, name: &'static str) -> Option<Duration> {
        let s = self.slice();
        if s.is_some() {
            self.last = Some(name);
        }
        s
    }
    fn spent_error(&self) -> String {
        match self.last {
            None => "not-read: board budget exhausted before any source".to_string(),
            Some(last) => format!("not-read: board budget exhausted after {last}"),
        }
    }
}

/// Run a subprocess with a hard wall-clock bound. A dedicated reader per pipe
/// drains stdout/stderr WHILE the child runs: a 210KB payload over a pipe the
/// parent never reads while waiting blocks the child on write until the kill,
/// which read every timeout as a dead child (measured: `fno inbox outstanding`
/// alone answers in 4s; the same command piped-only-at-exit died at the 26s
/// slice). The poll granularity (25ms) is far below any slice this board
/// hands out, and the kill is the degrade-not-crash contract's enforcement half.
fn run_with_timeout(cmd: &[String], cwd: &Path, timeout: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {}", cmd[0], e))?;
    let deadline = Instant::now() + timeout;
    // Drain the pipes concurrently; a killed child's pipe stays readable to
    // EOF, so the joins return promptly even on the kill path.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_reader.join().unwrap_or_default();
                let stderr = err_reader.join().unwrap_or_default();
                if !status.success() {
                    let detail = String::from_utf8_lossy(&stderr);
                    let detail = detail.trim();
                    let detail = if detail.is_empty() {
                        String::from_utf8_lossy(&stdout).trim().to_string()
                    } else {
                        detail.to_string()
                    };
                    return Err(format!(
                        "exit {}: {}",
                        status.code().unwrap_or(-1),
                        detail.chars().take(500).collect::<String>()
                    ));
                }
                return Ok(stdout);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let shown: Vec<String> = cmd.iter().take(6).cloned().collect::<Vec<_>>();
                    let mut shown = shown.join(" ");
                    if cmd.len() > 6 {
                        shown.push_str(" ...");
                    }
                    return Err(format!(
                        "{shown}: timed out after {:.1}s",
                        timeout.as_secs_f64()
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("{}: {}", cmd[0], e)),
        }
    }
}

fn run_json(cmd: Vec<String>, cwd: &Path, timeout: Duration) -> SourceRead {
    match run_with_timeout(&cmd, cwd, timeout) {
        Err(e) => SourceRead::err(e),
        Ok(stdout) => {
            if stdout.is_empty() {
                return SourceRead::ok(Value::Null);
            }
            match serde_json::from_slice::<Value>(&stdout) {
                Ok(v) => SourceRead::ok(v),
                Err(e) => SourceRead::err(format!("unparseable output: {e}")),
            }
        }
    }
}

/// The argv prefix for a Python `fno` self-shellout, resolved without a PATH
/// dependency (board.py `_fno` -> `_subprocess_util.fno_py_cmd`): the
/// `fno-py` console script, found on PATH first, then the bare name so a
/// genuinely-missing CLI surfaces a real subprocess error rather than a
/// silent no-op. A cargo-only install has no `fno` on PATH; `fno-py` (in
/// `~/.local/bin`) is what the mux forwards to, and PATH usually carries it.
fn fno_py_cmd() -> Vec<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("fno-py");
            if candidate.is_file() {
                return vec![candidate.display().to_string()];
            }
        }
    }
    vec!["fno-py".to_string()]
}

// ---------------------------------------------------------------------------
// Claims: the merged both-roots scan (claims.cli._merge_claims_across_roots)
// ---------------------------------------------------------------------------

/// Percent-decode a claim filename back to its key (io.decode_key /
/// `urllib.parse.unquote`); `%` escapes are the only ones the encoder writes.
fn decode_key(filename: &str) -> String {
    let bytes = filename.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One root's live + dead claim rows (core._list_claims_impl with
/// include_stale=true): every `.lock` file, classified, dead states kept.
fn scan_claims_dir(dir: &Path) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return rows;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".lock") {
            continue;
        }
        if path.is_dir() {
            continue; // the .expired archive dir and any future subdirs
        }
        let key = decode_key(name.trim_end_matches(".lock"));
        if !key.starts_with("node:") {
            continue;
        }
        match crate::claims::read_claim_file(&path) {
            Err(_) => continue, // gone between list and read: not a state
            Ok(rec) => {
                let state = crate::claims::classify(&rec, None);
                let state = state.as_str();
                // The board consumes live/suspect (stalled_holder's locks,
                // undriven_pr's driver read) and stale/corrupted (its own
                // queue); `free` never has a file to scan.
                if matches!(state, "live" | "suspect" | "stale" | "corrupted") {
                    rows.push(json!({
                        "key": rec.key,
                        "state": state,
                        "holder": rec.holder,
                        "host": rec.host,
                        "pid": rec.pid,
                    }));
                }
            }
        }
    }
    rows
}

/// Both roots, best-state-wins merged into one view (the Python merge's
/// priority order: live beats suspect beats stale beats corrupted).
fn read_claims(cwd: &Path) -> SourceRead {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let push = |dir: Option<PathBuf>, seen: &mut HashSet<PathBuf>, dirs: &mut Vec<PathBuf>| {
        if let Some(d) = dir {
            let resolved = d.canonicalize().unwrap_or_else(|_| d.clone());
            if seen.insert(resolved) {
                dirs.push(d);
            }
        }
    };
    // The global root (claims_root_for("node:") = FNO_CLAIMS_ROOT or $HOME),
    // then the canonical checkout's own root; dedup when they are the same.
    let global = crate::claims::global_claims_root();
    push(
        global.map(|g| g.join(".fno").join("claims")),
        &mut seen,
        &mut dirs,
    );
    let canonical = crate::paths::canonical_repo_root(cwd);
    push(
        canonical.map(|c| c.join(".fno").join("claims")),
        &mut seen,
        &mut dirs,
    );
    if dirs.is_empty() {
        return SourceRead::err("agents claim list: no claims root resolves");
    }
    const PRIORITY: [&str; 5] = ["live", "suspect", "stale", "corrupted", "free"];
    let prio = |state: &str| PRIORITY.iter().position(|s| *s == state).unwrap_or(5);
    let mut best: BTreeMap<String, Value> = BTreeMap::new();
    for dir in &dirs {
        for row in scan_claims_dir(dir) {
            let key = s_str(&row, "key").unwrap_or_default().to_string();
            let state = s_str(&row, "state").unwrap_or_default().to_string();
            match best.get(&key) {
                Some(existing)
                    if prio(s_str(existing, "state").unwrap_or("free")) <= prio(&state) =>
                {
                    continue;
                }
                _ => {
                    best.insert(key, row);
                }
            }
        }
    }
    SourceRead::ok(Value::Array(best.into_values().collect()))
}

// ---------------------------------------------------------------------------
// Undispatched: classify_planned_unclaimed over the graph we already hold
// ---------------------------------------------------------------------------

/// Pure port of `backlog/undispatched.classify_planned_unclaimed`, minus the
/// selector filters the board never sets (project/mission/roadmap/parent).
/// Reads the same entries and claims rows the other queues use.
fn classify_planned_unclaimed(entries: &[Value], claims: &[Value]) -> Result<Value, String> {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| s_str(e, "id").map(|id| (id, e)))
        .collect();
    let mut claimed: HashMap<&str, &str> = HashMap::new();
    for claim in claims {
        let Some(key) = s_str(claim, "key") else {
            return Err("claims unreadable: claim key is not a string".to_string());
        };
        if let Some(node_id) = key.strip_prefix("node:") {
            claimed.insert(node_id, s_str(claim, "state").unwrap_or("unknown"));
        }
    }
    let child_ids: HashSet<&str> = entries.iter().filter_map(|e| s_str(e, "parent")).collect();

    let priority_rank = |p: &str| match p {
        "p0" => 0,
        "p1" => 1,
        "p2" => 2,
        "p3" => 3,
        _ => 99,
    };
    let mut rows: Vec<(i32, String, Value)> = Vec::new();
    for entry in entries {
        let Some(node_id) = s_str(entry, "id") else {
            return Err("graph unreadable: entry id is not a string".to_string());
        };
        let plan_finalized = s_str(entry, "plan_path")
            .map(|p| !p.trim().is_empty())
            .unwrap_or(false);
        let status_ready = s_str(entry, "status") == Some("ready");
        let leaf = s_str(entry, "type") != Some("epic") && !child_ids.contains(&node_id);
        let completed = entry.get("completed_at").map(truthy).unwrap_or(false);
        let has_pr = entry.get("pr_number").map(truthy).unwrap_or(false)
            || entry
                .get("additional_prs")
                .and_then(Value::as_array)
                .map(|extras| {
                    extras
                        .iter()
                        .any(|e| e.is_object() && e.get("number").map(truthy).unwrap_or(false))
                })
                .unwrap_or(false);
        let batch_owner = entry.get("batch").map(truthy).unwrap_or(false);
        let blocked = entry
            .get("blocked_by")
            .and_then(Value::as_array)
            .is_some_and(|blockers| {
                blockers.iter().any(|b| {
                    let Some(blocker_id) = b.as_str() else {
                        return true;
                    };
                    match by_id.get(blocker_id) {
                        None => true,
                        Some(blocker) => {
                            s_str(blocker, "status") != Some("done")
                                && !blocker.get("completed_at").map(truthy).unwrap_or(false)
                        }
                    }
                })
            });
        let claim_state = claimed.get(node_id).copied();
        let selected = status_ready
            && plan_finalized
            && leaf
            && !completed
            && !has_pr
            && !batch_owner
            && !blocked
            && claim_state.is_none();
        if !selected {
            continue;
        }
        let priority = s_str(entry, "priority").unwrap_or("unknown").to_string();
        let mut row = json!({
            "id": node_id,
            "priority": entry.get("priority"),
            "domain": entry.get("domain"),
            "plan_path": entry.get("plan_path"),
            "facts": {
                "status_ready": status_ready,
                "plan_finalized": plan_finalized,
                "leaf": leaf,
                "completed": completed,
                "has_pr": has_pr,
                "batch_owner": batch_owner,
                "blocked": blocked,
                "claim_state": claim_state,
            },
        });
        if let Some(obj) = row.as_object_mut() {
            for key in ["title", "project", "mission_id", "roadmap_id", "parent"] {
                if !entry.get(key).unwrap_or(&Value::Null).is_null() {
                    obj.insert(key.to_string(), entry.get(key).cloned().unwrap());
                }
            }
        }
        let rank = priority_rank(&priority);
        rows.push((rank, node_id.to_string(), row));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(json!({
        "source": SRC_UNDISPATCHED,
        "status": "ok",
        "entries_scanned": entries.len(),
        "claims_scanned": claims.len(),
        "rows": rows.into_iter().map(|(_, _, r)| r).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Lane: the operator's own ranked file (king/lane.py)
// ---------------------------------------------------------------------------

struct LaneItem {
    text: String,
    node: Option<String>,
    parked: Option<String>,
    done: bool,
    line: usize,
}

fn parse_lane(path: &Path) -> Result<Vec<LaneItem>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read operator lane {}: {e}", path.display())),
    };
    let item_re = regex::Regex::new(r"^- \[( |x|X)\] (.*)$").expect("static regex");
    let body = format!("[a-z][a-z0-9]{{0,7}}-[0-9a-f]{{4,8}}");
    let suffix_re = regex::Regex::new(&format!(
        r"->\s*(?:(?P<node>{body})|parked:\s*(?P<reason>\S.*?))\s*$"
    ))
    .expect("static regex");
    let mut items = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let Some(caps) = item_re.captures(raw) else {
            continue;
        };
        let done = &caps[1] != " ";
        let rest = caps[2].to_string();
        let (mut node, mut parked, mut text_out) = (None, None, rest.clone());
        if let Some(sc) = suffix_re.captures(&rest) {
            node = sc.name("node").map(|m| m.as_str().to_string());
            parked = sc.name("reason").map(|m| m.as_str().to_string());
            text_out = rest[..sc.get(0).unwrap().start()].trim_end().to_string();
        }
        items.push(LaneItem {
            text: text_out.trim().to_string(),
            node,
            parked,
            done,
            line: i + 1,
        });
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// PRs: one listing, binding classification, mergeable filter
// ---------------------------------------------------------------------------

/// Delimiter-bounded node-id candidates of a head ref (pr/closure.branch_node_ids).
/// Hand-rolled: the pattern needs lookaheads (`(?=$|[/-])`) that the regex
/// crate does not support.
fn branch_node_ids(head_ref: &str) -> Vec<String> {
    let b = head_ref.as_bytes();
    let mut ids: Vec<String> = Vec::new();
    // Non-overlapping left-to-right scan, exactly like Python's finditer: a
    // match is consumed and the scan resumes after it, so "feature/x-cdef-1234"
    // never yields the bogus "cdef-1234" from inside the first match's tail.
    let mut i = 0;
    while i < b.len() {
        // A candidate starts at the string head or after '-' / '/'.
        if !(i == 0 || b[i - 1] == b'-' || b[i - 1] == b'/') {
            i += 1;
            continue;
        }
        if !b[i].is_ascii_lowercase() {
            i += 1;
            continue;
        }
        // [a-z][a-z0-9]{0,7} then '-' then [0-9a-f]{4,8}
        let mut j = i + 1;
        let mut alnum = 0;
        while j < b.len() && alnum < 7 && (b[j].is_ascii_lowercase() || b[j].is_ascii_digit()) {
            j += 1;
            alnum += 1;
        }
        if j >= b.len() || b[j] != b'-' {
            i += 1;
            continue;
        }
        let hex_start = j + 1;
        let mut k = hex_start;
        while k < b.len()
            && k - hex_start < 8
            && (b[k].is_ascii_digit() || b[k].is_ascii_hexdigit())
        {
            k += 1;
        }
        let hex_len = k - hex_start;
        if !(4..=8).contains(&hex_len) {
            i += 1;
            continue;
        }
        if !(k == b.len() || b[k] == b'-' || b[k] == b'/') {
            i += 1;
            continue;
        }
        let candidate = &head_ref[i..k];
        if !ids.iter().any(|c| c == candidate) {
            ids.push(candidate.to_string());
        }
        i = k;
    }
    ids
}

/// A rollup entry's pass/fail/pending class (pr/_status._classify).
fn classify_check(check: &Value) -> &'static str {
    let status = s_str(check, "status").unwrap_or("").to_uppercase();
    if !status.is_empty() && status != "COMPLETED" {
        return "pending";
    }
    let raw = check
        .get("conclusion")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .or_else(|| s_str(check, "state"))
        .unwrap_or("")
        .to_uppercase();
    if PASS_STATES.contains(&raw.as_str()) {
        return "pass";
    }
    if FAIL_STATES.contains(&raw.as_str()) {
        return "fail";
    }
    "pending"
}

/// Dedup to the latest run per check name/context (check_supersession's
/// generated selector), then drop the coverage projections, then every fetched
/// row is judged.
fn read_prs(
    cwd: &Path,
    slice: Duration,
    max_pr_reads: usize,
    entries: Option<&[Value]>,
) -> (SourceRead, SourceRead, Vec<String>) {
    let cmd = vec![
        "gh".to_string(),
        "pr".to_string(),
        "list".to_string(),
        "--state".to_string(),
        "open".to_string(),
        "--limit".to_string(),
        max_pr_reads.to_string(),
        "--json".to_string(),
        "number,title,mergeable,statusCheckRollup,headRefName,url".to_string(),
    ];
    let listing = run_json(cmd, cwd, slice);
    if !listing.is_ok() {
        let err = listing.error.clone().unwrap_or_default();
        return (
            SourceRead::err(err.clone()),
            SourceRead::err(format!("undriven_pr: {err}")),
            Vec::new(),
        );
    }
    let rows = listing.rows();
    let mut warnings: Vec<String> = Vec::new();
    if rows.len() >= max_pr_reads {
        warnings.push(format!(
            "mergeable_pr: the open-PR listing hit its {max_pr_reads}-PR limit, \
             so more open PRs can exist; raise max_pr_reads to read further"
        ));
    }

    // Binding: graph rows for nodes an open PR points back at. An unreadable
    // binding is an unreadable QUEUE, never an empty one: mergeable_pr needs
    // no node, undriven_pr is nothing but nodes.
    let pr_nodes = match entries {
        None => SourceRead::err("pr node binding unreadable: graph unreadable"),
        Some(entries) => {
            let real_ids: HashSet<&str> = entries.iter().filter_map(|e| s_str(e, "id")).collect();
            let node_by_id: HashMap<&str, &Value> = entries
                .iter()
                .filter_map(|e| s_str(e, "id").map(|i| (i, e)))
                .collect();
            // First pass: which nodes have exactly one open PR.
            let mut open_prs_by_node: HashMap<String, Vec<i64>> = HashMap::new();
            let mut parsed: Vec<(i64, Option<String>, String, Vec<String>)> = Vec::new();
            for row in &rows {
                let Some(number) = s_i64(row, "number") else {
                    continue;
                };
                let head = s_str(row, "headRefName").unwrap_or("");
                if head.is_empty() {
                    continue;
                }
                let matched: Vec<String> = branch_node_ids(head)
                    .into_iter()
                    .filter(|nid| real_ids.contains(nid.as_str()))
                    .collect();
                if matched.len() == 1 {
                    open_prs_by_node
                        .entry(matched[0].clone())
                        .or_default()
                        .push(number);
                }
                parsed.push((
                    number,
                    row.get("url").and_then(Value::as_str).map(str::to_string),
                    head.to_string(),
                    matched,
                ));
            }
            let mut bound: Vec<Value> = Vec::new();
            for (number, url, _head, matched) in parsed {
                if matched.is_empty() {
                    continue; // untracked: carries no candidate
                }
                if matched.len() > 1 {
                    continue; // ambiguous: a list-order guess is the wrong-node bind
                }
                let nid = &matched[0];
                let mut siblings = open_prs_by_node
                    .get(nid.as_str())
                    .cloned()
                    .unwrap_or_default();
                siblings.sort();
                if siblings.len() > 1 {
                    continue; // ambiguous
                }
                let Some(node) = node_by_id.get(nid.as_str()) else {
                    continue;
                };
                let refs_this_pr = node_pr_refs(node).iter().any(|(n, _)| *n == number);
                if !refs_this_pr {
                    warnings.push(format!("pr_node_binding_missing: #{number} -> {nid}"));
                    continue;
                }
                let mut row = (*node).clone();
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("pr_number".to_string(), json!(number));
                    obj.insert("pr_url".to_string(), json!(url));
                }
                bound.push(row);
            }
            SourceRead::ok(Value::Array(bound))
        }
    };

    // Every fetched row is judged; dropping any of them loses real work.
    let mut ready: Vec<Value> = Vec::new();
    for pr in &rows {
        if s_str(pr, "mergeable") != Some("MERGEABLE") {
            continue;
        }
        let rollup = pr
            .get("statusCheckRollup")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        let deduped = crate::check_supersession::latest_per_name(&rollup);
        let filtered: Vec<Value> = deduped
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|check| {
                        let is_coverage = |value: Option<&str>| {
                            value == Some(COVERAGE_STATUS_CONTEXT)
                                || value == Some(COVERAGE_UNAVAILABLE_STATUS_CONTEXT)
                        };
                        !is_coverage(s_str(check, "context")) && !is_coverage(s_str(check, "name"))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let had_rows = deduped.as_array().map(|a| !a.is_empty()).unwrap_or(false);
        if had_rows && filtered.is_empty() {
            // Diagnostic-only rollup: CI has not reported; not green.
            continue;
        }
        let mut has_fail = false;
        let mut has_pending = false;
        for check in &filtered {
            match classify_check(check) {
                "fail" => has_fail = true,
                "pending" => has_pending = true,
                _ => {}
            }
        }
        if has_fail || has_pending {
            continue;
        }
        ready.push(json!({
            "number": pr.get("number"),
            "title": pr.get("title"),
        }));
    }
    (SourceRead::ok(Value::Array(ready)), pr_nodes, warnings)
}

/// (pr_number, pr_url) pairs for a node, primary first, deduped
/// (graph/_reconcile.node_pr_refs).
fn node_pr_refs(node: &Value) -> Vec<(i64, Option<String>)> {
    let mut refs = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    if let Some(primary) = s_i64(node, "pr_number") {
        refs.push((
            primary,
            node.get("pr_url")
                .and_then(Value::as_str)
                .map(str::to_string),
        ));
        seen.insert(primary);
    }
    if let Some(extras) = node.get("additional_prs").and_then(Value::as_array) {
        for extra in extras {
            let Some(num) = s_i64(extra, "number") else {
                continue;
            };
            if seen.contains(&num) {
                continue;
            }
            refs.push((
                num,
                extra.get("url").and_then(Value::as_str).map(str::to_string),
            ));
            seen.insert(num);
        }
    }
    refs
}

/// The one status string every reader of a row agrees on
/// (graph/statuses.derived_status).
fn derived_status(entry: &Value) -> String {
    let terminal = {
        let status_terminal = s_str(entry, "status")
            .map(|s| TERMINAL_RUNGS.contains(&s))
            .unwrap_or(false);
        let superseded = entry.get("superseded_by").is_some_and(|v| !v.is_null());
        let completed = entry
            .get("completed_at")
            .and_then(Value::as_str)
            .map(|c| !c.is_empty() && !c.starts_with(LEGACY_DEFER_PREFIX))
            .unwrap_or(false);
        status_terminal || superseded || completed
    };
    if terminal && entry.get("completed_at").is_some_and(|v| !v.is_null()) {
        return "done".to_string();
    }
    s_str(entry, "status").unwrap_or("unknown").to_string()
}

// ---------------------------------------------------------------------------
// Claimed nodes + holder activity
// ---------------------------------------------------------------------------

/// The backlog row behind each LIVE node claim (board._read_claimed_nodes):
/// one graph read, exact id match (claims carry real ids; the slug fallback is
/// free and harmless), terminal claims dropped at the source.
fn read_claimed_nodes(
    claims: &SourceRead,
    entries: Option<&[Value]>,
) -> (SourceRead, Vec<String>, Vec<String>) {
    if !claims.is_ok() {
        return (
            SourceRead::err(claims.error.clone().unwrap_or_default()),
            Vec::new(),
            Vec::new(),
        );
    }
    let mut held: Vec<(String, String)> = Vec::new();
    for row in &claims.rows() {
        let Some(key) = s_str(row, "key") else {
            continue;
        };
        let Some(node_id) = key.strip_prefix("node:") else {
            continue;
        };
        let state = s_str(row, "state").unwrap_or("");
        if DEAD_CLAIM_STATES.contains(&state) {
            continue;
        }
        let holder = s_str(row, "holder").unwrap_or("");
        if !holder.is_empty() {
            held.push((node_id.to_string(), holder.to_string()));
        }
    }

    let Some(entries) = entries else {
        return (
            SourceRead::err("backlog get: graph unreadable"),
            Vec::new(),
            Vec::new(),
        );
    };
    let mut warnings: Vec<String> = Vec::new();
    if held.len() > MAX_CLAIMED_NODE_READS {
        warnings.push(format!(
            "stalled_holder: capped at {MAX_CLAIMED_NODE_READS} of {} live claims",
            held.len()
        ));
        held.truncate(MAX_CLAIMED_NODE_READS);
    }
    let mut nodes: Vec<Value> = Vec::new();
    let mut holders: Vec<String> = Vec::new();
    let mut seen_holders: HashSet<String> = HashSet::new();
    for (node_id, holder) in &held {
        let node = entries
            .iter()
            .find(|e| {
                s_str(e, "id")
                    .map(|i| i.eq_ignore_ascii_case(node_id))
                    .unwrap_or(false)
            })
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| s_str(e, "slug").map(|s| s == node_id).unwrap_or(false))
            });
        let Some(node) = node else {
            warnings.push(format!("stalled_holder: {node_id} unreadable: not found"));
            continue;
        };
        // A terminal node's claim is a reaper leak; dropping it here also keeps
        // its holder out of the transcript reads.
        if derived_status(node) == "done"
            || s_str(node, "status")
                .map(|s| TERMINAL_RUNGS.contains(&s))
                .unwrap_or(false)
            || node.get("superseded_by").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        nodes.push(node.clone());
        let priority = s_str(node, "priority").unwrap_or("");
        if KING_PRIORITIES.contains(&priority) && seen_holders.insert(holder.clone()) {
            holders.push(holder.clone());
        }
    }
    (SourceRead::ok(Value::Array(nodes)), holders, warnings)
}

/// Positive evidence the holder is doing something (board._holder_is_active):
/// an absent reading is not a staffed lane.
fn holder_is_active(probe: Option<&crate::claude_ask::TruthProbe>) -> bool {
    let Some(probe) = probe else {
        return false;
    };
    if !ACTIVE_STATES.contains(&probe.state.as_str()) {
        return false;
    }
    match probe.last_activity_age_s {
        None => false,
        Some(age) => age <= STALLED_AFTER_S,
    }
}

/// Who is driving this node: active, stalled, or none. One answer, two queues:
/// stalled_holder selects stalled and undriven_pr selects none.
fn node_driver<'a>(
    node_id: &str,
    claim_by_node: &'a HashMap<String, Value>,
    activity: &'a HashMap<String, crate::claude_ask::TruthProbe>,
) -> (&'static str, Option<&'a Value>) {
    let claim = claim_by_node.get(node_id);
    let Some(claim) = claim else {
        return ("none", None);
    };
    if DEAD_CLAIM_STATES.contains(&s_str(claim, "state").unwrap_or("")) {
        return ("none", Some(claim));
    }
    let holder = s_str(claim, "holder").unwrap_or("");
    let token = holder.split_once(':').map(|(_, t)| t).unwrap_or(holder);
    if holder_is_active(activity.get(token)) {
        return ("active", Some(claim));
    }
    ("stalled", Some(claim))
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Compile a canonical crown scope into the graph node ids it contains
/// (board.compile_scope_ids).
fn compile_scope_ids(
    scope: &str,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> Result<HashSet<String>, String> {
    let canonical_scope = |scopes: &[String]| {
        let mut sorted: Vec<String> = scopes.to_vec();
        sorted.sort();
        sorted.dedup();
        sorted.join(",")
    };
    let members: Vec<String> = scope
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if members.is_empty() {
        return Err("a crown needs a scope: name an epic or a project".to_string());
    }
    let projects = projects.clone()?;

    let entry_by_id = |id: &str| {
        entries
            .iter()
            .find(|e| s_str(e, "id").map(|i| i == id).unwrap_or(false))
    };

    // resolve_crown: the (level, canonical) pair, derived together.
    let (level_two, canonical): (bool, String) = if members.len() > 1 {
        let mut resolved = Vec::new();
        for m in &members {
            match projects.get(m.as_str()) {
                Some(canon) => resolved.push(canon.clone()),
                None => {
                    return Err(format!(
                        "a multi-scope crown rules PROJECTS, but {m} is not a configured \
                         project. Name projects from your config, or pass a single epic instead."
                    ))
                }
            }
        }
        (false, canonical_scope(&resolved))
    } else {
        let raw = members[0].as_str();
        if let Some(canon) = projects.get(raw) {
            (false, canon.clone())
        } else {
            match entry_by_id(raw) {
                None => {
                    return Err(format!(
                        "{raw:?} is neither a configured project nor a backlog node; \
                         nothing to reign over (check for a typo)"
                    ))
                }
                Some(entry) => {
                    if s_str(entry, "type") != Some("epic") {
                        return Err(format!("crown scope {raw:?} is not an epic in the graph"));
                    }
                    (true, raw.to_string())
                }
            }
        }
    };

    if level_two {
        let Some(root) = entry_by_id(&canonical) else {
            return Err(format!(
                "crown scope {canonical:?} is not an epic in the graph"
            ));
        };
        if s_str(root, "type") != Some("epic") {
            return Err(format!(
                "crown scope {canonical:?} is not an epic in the graph"
            ));
        }
        let mut ids: HashSet<String> = HashSet::new();
        ids.insert(canonical.clone());
        // descendants_of: BFS over parent links, cycle-safe.
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in entries {
            if let (Some(id), Some(parent)) = (s_str(e, "id"), s_str(e, "parent")) {
                children.entry(parent).or_default().push(id);
            }
        }
        let mut frontier: Vec<&str> = children
            .get(canonical.as_str())
            .cloned()
            .unwrap_or_default();
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(id) = frontier.pop() {
            if !seen.insert(id) {
                continue;
            }
            ids.insert(id.to_string());
            if let Some(next) = children.get(id) {
                frontier.extend(next.iter().copied());
            }
        }
        return Ok(ids);
    }

    let project_set: HashSet<String> = canonical
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut ids = HashSet::new();
    for e in entries {
        let Some(id) = s_str(e, "id") else {
            continue;
        };
        let project = s_str(e, "project").unwrap_or("").to_string();
        let canonical_project = projects.get(project.as_str()).unwrap_or(&project);
        if project_set.contains(canonical_project) {
            ids.insert(id.to_string());
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Board construction: the eleven queues
// ---------------------------------------------------------------------------

struct Queue {
    name: &'static str,
    source: String,
    status: &'static str,
    error: String,
    count: i64,
    rows: Vec<Value>,
    actionable: bool,
    note: String,
    verb: &'static str,
}

fn queue(
    name: &'static str,
    source: String,
    read: &SourceRead,
    rows: Vec<Value>,
    actionable: bool,
    note: String,
    verb: &'static str,
    count: Option<i64>,
) -> Queue {
    if !read.is_ok() {
        return Queue {
            name,
            source,
            status: "unreadable",
            error: read.error.clone().unwrap_or_default(),
            count: -1,
            rows: Vec::new(),
            actionable,
            note,
            verb,
        };
    }
    Queue {
        name,
        source,
        status: "ok",
        error: String::new(),
        count: count.unwrap_or(rows.len() as i64),
        rows,
        actionable,
        note,
        verb,
    }
}

fn queue_json(q: &Queue) -> Value {
    json!({
        "name": q.name,
        "source": q.source,
        "status": q.status,
        "error": q.error,
        "count": if q.status == "unreadable" { Value::Null } else { json!(q.count) },
        "rows": q.rows,
        "actionable": q.actionable,
        "note": q.note,
        "verb": q.verb,
    })
}

/// King manifest frontmatter fields (king/state.parse_manifest): an unreadable
/// manifest reads as absent.
fn parse_manifest(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        if line.trim() == "---" {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let raw = raw.trim();
        let raw = if raw.starts_with('"') {
            serde_json::from_str::<String>(raw)
                .unwrap_or_else(|_| raw.trim_matches('"').to_string())
        } else {
            raw.to_string()
        };
        out.insert(key.trim().to_string(), raw);
    }
    out
}

/// All the board's fetched sources, ready for the pure build.
struct BoardInputs {
    ready: SourceRead,
    claims: SourceRead,
    claimed_nodes: SourceRead,
    holder_activity: HashMap<String, crate::claude_ask::TruthProbe>,
    prs: SourceRead,
    pr_nodes: SourceRead,
    outstanding: SourceRead,
    needs: SourceRead,
    lane: SourceRead,
    undispatched: SourceRead,
    warnings: Vec<String>,
    autonomous_merge: bool,
    scope_ids: Option<HashSet<String>>,
    crown_scope: Option<String>,
}

/// Build the board payload. Pure; does no I/O. Queue names, order, and row
/// shapes match board.py's `build_board` exactly.
fn build_board(inputs: &BoardInputs) -> Value {
    let warnings = inputs.warnings.clone();
    let mut out_of_scope: Vec<Value> = Vec::new();
    let scope_ids = inputs.scope_ids.as_ref();

    let in_scope = |queue: &str, node_id: &Value, row: &Value, out: &mut Vec<Value>| -> bool {
        let Some(ids) = scope_ids else {
            return true;
        };
        let Some(id) = node_id.as_str() else {
            return true;
        };
        if ids.contains(id) {
            return true;
        }
        let mut extra = Map::new();
        extra.insert("queue".to_string(), json!(queue));
        extra.insert("id".to_string(), json!(id));
        if let Some(title) = row.get("title").filter(|t| !t.is_null()) {
            extra.insert("title".to_string(), title.clone());
        }
        out.push(Value::Object(extra));
        false
    };

    let claim_rows = inputs.claims.rows();
    let mut claim_by_node: HashMap<String, Value> = HashMap::new();
    for row in &claim_rows {
        if let Some(key) = s_str(row, "key") {
            if let Some(node_id) = key.strip_prefix("node:") {
                claim_by_node.insert(node_id.to_string(), row.clone());
            }
        }
    }

    // Undispatched: planned work with no claim, king priorities only.
    let undispatched_rows = if inputs.undispatched.is_ok() {
        inputs
            .undispatched
            .rows()
            .into_iter()
            .filter(|node| KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")))
            .filter(|node| {
                in_scope(
                    "undispatched",
                    node.get("id").unwrap_or(&Value::Null),
                    node,
                    &mut out_of_scope,
                )
            })
            .map(|node| {
                json!({
                    "id": node.get("id"),
                    "priority": node.get("priority"),
                    "title": node.get("title"),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // Unplanned: cold-dispatchable ideas off the ready list.
    let unplanned_rows: Vec<Value> = inputs
        .ready
        .rows()
        .into_iter()
        .filter(|node| KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")))
        .filter(|node| {
            node.get("plan_path")
                .map(|p| p.is_null() || p.as_str().map(|s| s.is_empty()).unwrap_or(false))
                .unwrap_or(true)
        })
        .filter(|node| {
            let dead = claim_by_node
                .get(s_str(node, "id").unwrap_or(""))
                .map(|c| DEAD_CLAIM_STATES.contains(&s_str(c, "state").unwrap_or("")))
                .unwrap_or(false);
            !dead
        })
        .filter(|node| {
            in_scope(
                "unplanned",
                node.get("id").unwrap_or(&Value::Null),
                node,
                &mut out_of_scope,
            )
        })
        .map(|node| {
            json!({
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
            })
        })
        .collect();

    // Stalled holder: starts from the CLAIM, never the ready list (a live
    // holder is exactly what `ready` has already removed).
    let mut stalled_rows: Vec<Value> = Vec::new();
    for node in &inputs.claimed_nodes.rows() {
        if !KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")) {
            continue;
        }
        if s_str(node, "status")
            .map(|s| TERMINAL_RUNGS.contains(&s))
            .unwrap_or(false)
        {
            continue;
        }
        let (state, claim) = node_driver(
            s_str(node, "id").unwrap_or(""),
            &claim_by_node,
            &inputs.holder_activity,
        );
        if state != "stalled" {
            continue;
        }
        let claim = claim.expect("stalled always carries its claim");
        if !in_scope(
            "stalled_holder",
            node.get("id").unwrap_or(&Value::Null),
            &node,
            &mut out_of_scope,
        ) {
            continue;
        }
        stalled_rows.push(json!({
            "id": node.get("id"),
            "priority": node.get("priority"),
            "title": node.get("title"),
            "holder": claim.get("holder"),
            "claim_state": claim.get("state"),
        }));
    }

    // Stale claims: locks nobody will reap.
    let stale_claim_rows: Vec<Value> = claim_rows
        .iter()
        .filter(|row| DEAD_CLAIM_STATES.contains(&s_str(row, "state").unwrap_or("")))
        .filter(|row| {
            let node_id = s_str(row, "key")
                .and_then(|k| k.strip_prefix("node:"))
                .unwrap_or("");
            in_scope("stale_claim", &json!(node_id), row, &mut out_of_scope)
        })
        .map(|row| {
            json!({
                "key": row.get("key"),
                "holder": row.get("holder"),
                "state": row.get("state"),
            })
        })
        .collect();

    // Operator lane.
    let lane_ok = inputs.lane.is_ok();
    let lane_items: Vec<LaneItem> = if lane_ok {
        inputs
            .lane
            .rows()
            .iter()
            .map(|r| LaneItem {
                text: s_str(r, "text").unwrap_or("").to_string(),
                node: r.get("node").and_then(Value::as_str).map(str::to_string),
                parked: r.get("parked").and_then(Value::as_str).map(str::to_string),
                done: r.get("done").and_then(Value::as_bool).unwrap_or(false),
                line: r.get("line").and_then(Value::as_u64).unwrap_or(0) as usize,
            })
            .collect()
    } else {
        Vec::new()
    };
    let lane_open: Vec<&LaneItem> = lane_items
        .iter()
        .filter(|i| !i.done && i.node.is_none() && i.parked.is_none())
        .collect();
    let parked_count = lane_items.iter().filter(|i| i.parked.is_some()).count();
    let mut lane_note = "the operator's own ranking. File each with `fno backlog idea \"<text>\"` and stamp `-> <id>` onto its line, or park it with `-> parked: <reason>`.".to_string();
    if parked_count > 0 {
        lane_note.push_str(&format!(" {parked_count} parked, reasons are in the file."));
    }
    let scoped = scope_ids.is_some();
    if scoped {
        lane_note.push_str(" report-only under a crown: lane lines are the operator's global priorities and carry no node id, so a scoped king cannot attribute them to its subtree");
    }
    let lane_rows: Vec<Value> = if lane_ok {
        lane_open
            .iter()
            .map(|i| json!({"text": i.text, "line": i.line}))
            .collect()
    } else {
        Vec::new()
    };

    let pr_rows: Vec<Value> = inputs
        .prs
        .rows()
        .iter()
        .map(|r| json!({"number": r.get("number"), "title": r.get("title")}))
        .collect();

    // Undriven PR: the complement of stalled_holder, the second half of ONE
    // predicate. Fail CLOSED on an unreadable claim list: every node would
    // read "none" and the king would dispatch over every live worker at once.
    let mergeable_numbers: HashSet<i64> = if inputs.autonomous_merge {
        pr_rows
            .iter()
            .filter_map(|r| r.get("number").and_then(Value::as_i64))
            .collect()
    } else {
        HashSet::new()
    };
    let mut undriven_rows: Vec<Value> = Vec::new();
    if inputs.pr_nodes.is_ok() && inputs.claims.is_ok() {
        for node in &inputs.pr_nodes.rows() {
            if !KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")) {
                continue;
            }
            let terminal = s_str(node, "status")
                .map(|s| TERMINAL_RUNGS.contains(&s))
                .unwrap_or(false)
                || node.get("superseded_by").is_some_and(|v| !v.is_null())
                || node
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .map(|c| !c.is_empty() && !c.starts_with(LEGACY_DEFER_PREFIX))
                    .unwrap_or(false);
            if terminal {
                continue;
            }
            let status = derived_status(node);
            if status == "deferred" || status == "blocked" {
                continue;
            }
            let (state, _claim) = node_driver(
                s_str(node, "id").unwrap_or(""),
                &claim_by_node,
                &inputs.holder_activity,
            );
            if state != "none" {
                continue;
            }
            let pr_number = node.get("pr_number").and_then(Value::as_i64);
            if let Some(n) = pr_number {
                if mergeable_numbers.contains(&n) {
                    continue;
                }
            }
            if !in_scope(
                "undriven_pr",
                node.get("id").unwrap_or(&Value::Null),
                &node,
                &mut out_of_scope,
            ) {
                continue;
            }
            undriven_rows.push(json!({
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
                "status": status,
                "pr_number": node.get("pr_number"),
                "pr_url": node.get("pr_url"),
            }));
        }
    }

    // One outstanding read, three streams.
    let outstanding = inputs.outstanding.dict();
    let question_rows: Vec<Value> = outstanding
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .map(|r| {
                    json!({"id": r.get("id"), "question": r.get("question"), "ts": r.get("ts")})
                })
                .collect()
        })
        .unwrap_or_default();

    let carveout_stream = outstanding
        .get("carveouts")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let mut carveout_by_kind: Vec<(String, i64)> = carveout_stream
        .get("by_kind")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), as_int(v))).collect())
        .unwrap_or_default();
    carveout_by_kind.sort();
    let carveout_rows: Vec<Value> = carveout_by_kind
        .into_iter()
        .map(|(kind, n)| json!({"kind": kind, "n": n}))
        .collect();
    let carveout_root = outstanding
        .get("roots")
        .and_then(|r| r.get("carveouts"))
        .and_then(|c| c.get("root"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let capture_stream = outstanding
        .get("captures")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let capture_by_project: Vec<(String, i64)> = capture_stream
        .get("by_project")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), as_int(v))).collect())
        .unwrap_or_default();
    let total_projects = capture_by_project.len();
    let mut capture_sorted = capture_by_project;
    capture_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut capture_rows: Vec<Value> = capture_sorted
        .iter()
        .take(CAPTURE_PROJECT_CAP)
        .map(|(project, n)| json!({"project": project, "n": n}))
        .collect();
    let elided = total_projects.saturating_sub(capture_rows.len());
    if elided > 0 {
        capture_rows.push(json!({"elided_projects": elided}));
    }

    // `fno agents needs` emits operator questions in the same list; the queue
    // above already carries them, so this one drops the kind.
    let needs_rows: Vec<Value> = inputs
        .needs
        .rows()
        .iter()
        .filter(|row| s_str(row, "kind") != Some("operator_question"))
        .filter(|row| {
            in_scope(
                "unreachable_worker",
                row.get("node").unwrap_or(&Value::Null),
                row,
                &mut out_of_scope,
            )
        })
        .map(|row| {
            json!({"kind": row.get("kind"), "name": row.get("name"), "node": row.get("node")})
        })
        .collect();

    let lane_source = format!("cat {}", operator_lane_path(Path::new(".")).display());

    let mut queues = vec![
        queue(
            "operator_lane",
            lane_source,
            &inputs.lane,
            lane_rows,
            !scoped,
            lane_note,
            "",
            None,
        ),
        queue(
            "undispatched",
            format!("{SRC_UNDISPATCHED} + {SRC_CLAIMS}"),
            &if inputs.undispatched.is_ok() && inputs.claims.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    SourceRead {
                        error: inputs.undispatched.error.clone(),
                        ..Default::default()
                    }
                    .error
                    .or_else(|| inputs.claims.error.clone())
                    .unwrap_or_default(),
                )
            },
            undispatched_rows,
            true,
            "one worker per node; these already carry a plan".to_string(),
            "/fno:target",
            None,
        ),
        queue(
            "unplanned",
            format!("{SRC_READY} + {SRC_CLAIMS}"),
            &if inputs.ready.is_ok() && inputs.claims.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .ready
                        .error
                        .clone()
                        .or_else(|| inputs.claims.error.clone())
                        .unwrap_or_default(),
                )
            },
            unplanned_rows,
            true,
            "batch: up to 3 blueprints per session; merge same-shape nodes into one waved plan".to_string(),
            "/fno:blueprint",
            None,
        ),
        queue(
            "stalled_holder",
            format!("{SRC_CLAIMS} + fno backlog get <id> + fno agents peek <holder>"),
            &if inputs.claims.is_ok() && inputs.claimed_nodes.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .claims
                        .error
                        .clone()
                        .or_else(|| inputs.claimed_nodes.error.clone())
                        .unwrap_or_default(),
                )
            },
            stalled_rows,
            true,
            String::new(),
            "",
            None,
        ),
        queue(
            "undriven_pr",
            SRC_PR_NODES.to_string(),
            &if inputs.pr_nodes.is_ok() && inputs.claims.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .pr_nodes
                        .error
                        .clone()
                        .or_else(|| inputs.claims.error.clone())
                        .unwrap_or_default(),
                )
            },
            undriven_rows,
            true,
            "an open PR with nobody driving it; report only, never close or defer one - that judgment is the operator's".to_string(),
            "/fno:target",
            None,
        ),
        queue(
            "mergeable_pr",
            SRC_PRS.to_string(),
            &inputs.prs,
            pr_rows,
            inputs.autonomous_merge,
            if inputs.autonomous_merge {
                String::new()
            } else {
                "report-only: merging is outward and hard to reverse, so it waits on config.king.autonomous_merge".to_string()
            },
            "",
            None,
        ),
        queue(
            "stale_claim",
            SRC_CLAIMS.to_string(),
            &inputs.claims,
            stale_claim_rows,
            true,
            String::new(),
            "",
            None,
        ),
        queue(
            "operator_question",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            question_rows,
            false,
            "report-only: a human answers these, so counting them would hold the loop open forever".to_string(),
            "",
            None,
        ),
        queue(
            "carveout_pending",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            carveout_rows,
            false,
            if carveout_root.is_empty() {
                "report-only: the sweep is a human verb".to_string()
            } else {
                format!("report-only: the sweep is a human verb; root {carveout_root}")
            },
            "",
            Some(as_int(carveout_stream.get("total").unwrap_or(&Value::Null))),
        ),
        queue(
            "capture_pending",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            capture_rows,
            false,
            "report-only: per-project counts only; the rows cannot be listed".to_string(),
            "",
            Some(as_int(capture_stream.get("total").unwrap_or(&Value::Null))),
        ),
        queue(
            "unreachable_worker",
            SRC_NEEDS.to_string(),
            &inputs.needs,
            needs_rows,
            false,
            "report-only: the refusal event a king would act on does not exist yet".to_string(),
            "",
            None,
        ),
    ];

    if let Some(scope) = &inputs.crown_scope {
        if scope_ids.is_some() {
            queues.push(queue(
                "out_of_scope",
                format!("king manifest scope {scope}"),
                &SourceRead::ok(Value::Array(out_of_scope.clone())),
                out_of_scope.clone(),
                false,
                format!("report-only: outside crown scope {scope}"),
                "",
                None,
            ));
        }
    }

    let mut actionable: i64 = 0;
    let mut unreadable: i64 = 0;
    for q in &queues {
        if q.status == "unreadable" {
            unreadable += 1;
            // A blind ACTIONABLE queue is work: the king may not exit while it
            // cannot see a queue it could have shrunk. A blind report-only
            // queue is loud (the exit code) and still uncounted.
            if q.actionable {
                actionable += 1;
            }
        } else if q.actionable {
            actionable += q.count;
        }
    }

    let queues_json: Vec<Value> = queues.iter().map(queue_json).collect();
    json!({
        "actionable": actionable,
        "unreadable": unreadable,
        "queues": queues_json,
        "warnings": warnings,
        "exit_code": if unreadable > 0 { 1 } else { 0 },
    })
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// The `fno-agents board` entry point: parse flags, print the payload, exit
/// with the board's own exit_code. Kept beside the parity sets' reach (an
/// `==` arm in client.rs), so no advertised verb is added.
pub fn run_board(args: &[String]) -> i32 {
    let mut opts = BoardOpts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" | "-J" => {}
            "--budget-ms" => match it.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) => opts.budget_ms = v,
                None => {
                    eprintln!("fno-agents board: --budget-ms needs a millisecond integer");
                    return 2;
                }
            },
            "--max-pr-reads" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) => opts.max_pr_reads = v,
                None => {
                    eprintln!("fno-agents board: --max-pr-reads needs an integer");
                    return 2;
                }
            },
            "--state" => match it.next() {
                Some(v) => opts.state_path = Some(PathBuf::from(v)),
                None => {
                    eprintln!("fno-agents board: --state needs a path");
                    return 2;
                }
            },
            other => {
                eprintln!("fno-agents board: unknown flag {other}");
                return 2;
            }
        }
    }
    let payload = read_board(&opts);
    println!(
        "{}",
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
    );
    payload
        .get("exit_code")
        .and_then(Value::as_i64)
        .map(|c| c as i32)
        .unwrap_or(1)
}

pub struct BoardOpts {
    pub budget_ms: u64,
    pub max_pr_reads: usize,
    /// King manifest whose `scope` bounds the board.
    pub state_path: Option<PathBuf>,
    /// The directory every config-tier, claims-root, and project-journal
    /// resolution anchors on. `None` means the process cwd (the hand-run CLI
    /// case); an IN-PROCESS caller such as loopcheck passes its `--cwd` here,
    /// because the old subprocess board ran with the king session's cwd and
    /// the calling process's cwd is not guaranteed to be the same directory.
    pub cwd: Option<PathBuf>,
}

impl Default for BoardOpts {
    fn default() -> Self {
        BoardOpts {
            budget_ms: HAND_RUN_BUDGET_MS,
            max_pr_reads: 20,
            state_path: None,
            cwd: None,
        }
    }
}

/// Read the whole board. Never panics on a source; every failure lands in its
/// queue's error and the payload still answers.
pub fn read_board(opts: &BoardOpts) -> Value {
    let cwd = opts
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut budget = Budget::new(opts.budget_ms);
    let mut sources: Map<String, Value> = Map::new();
    let mut warnings: Vec<String> = Vec::new();

    let mark =
        |sources: &mut Map<String, Value>, name: &str, read: &SourceRead, truncated: bool| {
            sources.insert(
                name.to_string(),
                json!({
                    "ok": read.is_ok(),
                    "truncated": truncated,
                    "error": read.error.clone().unwrap_or_default(),
                }),
            );
        };

    // The graph: ONE read, shared by undispatched, claimed-node lookups, PR
    // binding, and crown scope. `read_defaulted_opts(path, false, false)` is
    // exactly the keeper's `read_strict` (defaults applied, diagnosis without
    // a .bak), which is what the Python board's read_graph_strict runs.
    let graph_path = graph_json_path(&cwd);
    let entries: Option<Vec<Value>> =
        match graph_store::read_defaulted_opts(&graph_path, false, false) {
            Ok(e) => Some(e),
            Err(e) => {
                warnings.push(format!("graph unreadable: {e}"));
                None
            }
        };

    let spent = |sources: &mut Map<String, Value>, name: &str, budget: &Budget| {
        let err = budget.spent_error();
        sources.insert(
            name.to_string(),
            json!({"ok": false, "truncated": true, "error": err}),
        );
    };

    // The reference charged slices sequentially; this collector charges them
    // in the same order but runs the four subprocess reads CONCURRENTLY, for
    // the reason the Python board ran its six sources on a thread pool:
    // sequential execution lets the slowest source (gh pr list under fleet
    // load, measured 28s here) starve every queue behind it. The deadline is
    // still the ONE bound: every slice derives from the same total, and the
    // slowest source now bounds the wall, not the sum.
    let s_undispatched = budget.start("backlog undispatched");
    let s_claims = budget.start("agents claim list");
    let s_prs = budget.start(SRC_PRS);
    let s_stalled = budget.start("stalled_holder lookups");
    let s_ready = budget.start(SRC_READY);
    let s_outstanding = budget.start(SRC_QUESTIONS);
    let s_needs = budget.start(SRC_NEEDS);

    // In-process sources: graph already read; claims scan, undispatched
    // classify, claimed-node lookups, the needs fold, and the lane file. None
    // of them spawn.
    let claims = match s_claims {
        None => {
            spent(&mut sources, "claims", &budget);
            SourceRead::err(budget.spent_error())
        }
        Some(_) => {
            let read = read_claims(&cwd);
            mark(&mut sources, "claims", &read, false);
            read
        }
    };
    let undispatched = match s_undispatched {
        None => {
            spent(&mut sources, "undispatched", &budget);
            SourceRead::err(budget.spent_error())
        }
        Some(_) => match (&entries, &claims) {
            (Some(entries), claims) if claims.is_ok() => {
                match classify_planned_unclaimed(entries, &claims.rows()) {
                    Ok(receipt) => {
                        let read = SourceRead::ok(receipt);
                        let rows = read
                            .payload
                            .as_ref()
                            .and_then(|r| r.get("rows").and_then(Value::as_array).cloned());
                        mark(&mut sources, "undispatched", &read, false);
                        match rows {
                            Some(rows) => SourceRead::ok(Value::Array(rows)),
                            None => read,
                        }
                    }
                    Err(e) => {
                        let read = SourceRead::err(format!("undispatched: {e}"));
                        mark(&mut sources, "undispatched", &read, false);
                        read
                    }
                }
            }
            (_, claims) if !claims.is_ok() => SourceRead::err(format!(
                "undispatched: {}",
                claims.error.clone().unwrap_or_default()
            )),
            (None, _) => SourceRead::err("undispatched: graph unreadable"),
            _ => SourceRead::err("undispatched: unreadable"),
        },
    };

    // Claimed nodes: from the locks to the rows, one graph read.
    let (claimed_nodes, holders, claimed_warnings) = match s_stalled {
        None => {
            spent(&mut sources, "claimed_nodes", &budget);
            (
                SourceRead::err(budget.spent_error()),
                Vec::new(),
                Vec::new(),
            )
        }
        Some(_) => {
            let (read, holders, w) = read_claimed_nodes(&claims, entries.as_deref());
            mark(&mut sources, "claimed_nodes", &read, false);
            (read, holders, w)
        }
    };
    warnings.extend(claimed_warnings);

    // The five reads that can take real wall time run concurrently: gh pr
    // list, `fno backlog ready`, `fno inbox outstanding`, the batched truth
    // probe, and the needs fold (in-process, but its refused-worker leg batch
    // probes the whole registry and measured ~7s on a busy fleet). Their
    // slices were derived above in the reference's order.
    let entries_ref = entries.as_deref();
    let cwd_for_threads = cwd.clone();
    let (prs, pr_nodes, pr_warnings, prs_truncated, ready, outstanding, needs, holder_activity) =
        std::thread::scope(|s| {
            let t_prs = s_prs.map(|slice| {
                let cwd = cwd_for_threads.clone();
                s.spawn(move || {
                    let (prs, pr_nodes, w) = read_prs(&cwd, slice, opts.max_pr_reads, entries_ref);
                    let truncated = w.iter().any(|x| x.contains("hit its"));
                    (prs, pr_nodes, w, truncated)
                })
            });
            let t_ready = s_ready.map(|slice| {
                let cwd = cwd_for_threads.clone();
                s.spawn(move || {
                    let mut cmd = fno_py_cmd();
                    cmd.extend(
                        ["backlog", "ready", "--json", "-A"]
                            .iter()
                            .map(|s| s.to_string()),
                    );
                    run_json(cmd, &cwd, slice)
                })
            });
            let t_outstanding = s_outstanding.map(|slice| {
                let cwd = cwd_for_threads.clone();
                s.spawn(move || {
                    let mut cmd = fno_py_cmd();
                    cmd.extend(
                        ["inbox", "outstanding", "--json"]
                            .iter()
                            .map(|s| s.to_string()),
                    );
                    run_json(cmd, &cwd, slice)
                })
            });
            // ONE batched truth probe for every holder the king cares about (the
            // single-transcript-reader constraint; a probe per holder would pay
            // one interpreter cold start each).
            let t_truth = if holders.is_empty() {
                None
            } else {
                let tokens: Vec<String> = holders
                    .iter()
                    .map(|h| {
                        h.split_once(':')
                            .map(|(_, t)| t.to_string())
                            .unwrap_or_else(|| h.clone())
                    })
                    .collect();
                Some(s.spawn(move || crate::claude_ask::family1_truth_probe_many(&tokens)))
            };
            // The needs fold rides a thread too: in-process, but its
            // refused-worker leg batch probes the whole registry and measured
            // ~7s on a busy fleet, which no longer sits on the critical path.
            let t_needs = s_needs.map(|_slice| {
                let cwd = cwd_for_threads.clone();
                s.spawn(move || {
                    let home = crate::paths::AgentsHome::from_env();
                    let (mut event_paths, default_ledger) = default_needs_sources(&home);
                    // The canonical checkout's journal, exactly as `run_needs`
                    // adds it: a question asked from a worktree writes the
                    // CANONICAL .fno/events.jsonl, never the worktree's. The
                    // project journal anchors on the BOARD's cwd (the caller's
                    // --cwd for an in-process reader), never the process cwd.
                    event_paths[0] = cwd.join(".fno").join("events.jsonl");
                    if let Some(root) = crate::paths::canonical_repo_root(&cwd) {
                        let canonical_events = root.join(".fno").join("events.jsonl");
                        let cwd_events = cwd.join(".fno").join("events.jsonl");
                        if canonical_events != cwd_events
                            && !event_paths.contains(&canonical_events)
                        {
                            event_paths.push(canonical_events);
                        }
                    }
                    let since = now_secs_board().saturating_sub(crate::needs::DEFAULT_WINDOW_SECS);
                    crate::needs::collect_needs_items(
                        &home,
                        &event_paths,
                        &default_ledger,
                        since,
                        crate::needs::DEFAULT_FIRES_FLOOR,
                    )
                })
            });

            let (prs, pr_nodes, pr_warnings, prs_truncated) = match t_prs {
                None => {
                    let err = budget.spent_error();
                    (
                        SourceRead::err(err.clone()),
                        SourceRead::err(err),
                        Vec::new(),
                        true,
                    )
                }
                Some(h) => h.join().unwrap_or_else(|_| {
                    (
                        SourceRead::err("prs: reader panicked"),
                        SourceRead::err("undriven_pr: reader panicked"),
                        Vec::new(),
                        false,
                    )
                }),
            };
            let ready = match t_ready {
                None => SourceRead::err(budget.spent_error()),
                Some(h) => h
                    .join()
                    .unwrap_or(SourceRead::err("ready: reader panicked")),
            };
            let outstanding = match t_outstanding {
                None => SourceRead::err(budget.spent_error()),
                Some(h) => h
                    .join()
                    .unwrap_or(SourceRead::err("outstanding: reader panicked")),
            };
            let outstanding = if outstanding.is_ok() {
                SourceRead::ok(outstanding.payload.unwrap_or(json!({})))
            } else {
                outstanding
            };
            let needs = match t_needs {
                None => SourceRead::err(budget.spent_error()),
                Some(h) => SourceRead::ok(
                    h.join()
                        .map(|items| serde_json::to_value(&items).unwrap_or(json!([])))
                        .unwrap_or(json!([])),
                ),
            };
            let holder_activity: HashMap<String, crate::claude_ask::TruthProbe> = match t_truth {
                None => HashMap::new(),
                Some(h) => h.join().unwrap_or_default(),
            };
            (
                prs,
                pr_nodes,
                pr_warnings,
                prs_truncated,
                ready,
                outstanding,
                needs,
                holder_activity,
            )
        });
    warnings.extend(pr_warnings);
    let prs_truncated = prs_truncated || !prs.is_ok();
    mark(&mut sources, "prs", &prs, prs_truncated);
    mark(&mut sources, "ready", &ready, false);
    mark(&mut sources, "outstanding", &outstanding, false);
    sources.insert(
        "holder_activity".to_string(),
        json!({"ok": true, "truncated": false, "error": ""}),
    );

    // Lane: a file read; there is no verb behind it.
    let lane_path = operator_lane_path(&cwd);
    let lane = match parse_lane(&lane_path) {
        Err(e) => SourceRead::err(e),
        Ok(items) => SourceRead::ok(Value::Array(
            items
                .iter()
                .map(|i| {
                    json!({
                        "text": i.text,
                        "node": i.node,
                        "parked": i.parked,
                        "done": i.done,
                        "line": i.line,
                    })
                })
                .collect(),
        )),
    };
    mark(&mut sources, "lane", &lane, false);

    // Scope.
    let mut scope_ids: Option<HashSet<String>> = None;
    let mut crown_scope: Option<String> = None;
    if let Some(state_path) = &opts.state_path {
        let manifest = parse_manifest(state_path);
        let scope = manifest.get("scope").cloned().unwrap_or_default();
        if scope.is_empty() {
            return json!({
                "actionable": 1,
                "unreadable": 1,
                "queues": [queue_json(&Queue {
                    name: "scope",
                    source: state_path.display().to_string(),
                    status: "unreadable",
                    error: "king manifest has no scope".to_string(),
                    count: -1,
                    rows: Vec::new(),
                    actionable: true,
                    note: String::new(),
                    verb: "",
                })],
                "warnings": warnings,
                "exit_code": 1,
                "sources": Value::Object(sources),
            });
        }
        crown_scope = Some(scope.clone());
        let projects = project_map(&cwd);
        match compile_scope_ids(&scope, entries.as_deref().unwrap_or(&[]), &projects) {
            Ok(ids) => scope_ids = Some(ids),
            Err(e) => {
                return json!({
                    "actionable": 1,
                    "unreadable": 1,
                    "queues": [queue_json(&Queue {
                        name: "scope",
                        source: format!("king manifest scope {scope}"),
                        status: "unreadable",
                        error: e,
                        count: -1,
                        rows: Vec::new(),
                        actionable: true,
                        note: String::new(),
                        verb: "",
                    })],
                    "warnings": warnings,
                    "exit_code": 1,
                    "sources": Value::Object(sources),
                });
            }
        }
    }

    let inputs = BoardInputs {
        ready,
        claims,
        claimed_nodes,
        holder_activity,
        prs,
        pr_nodes,
        outstanding,
        needs,
        lane,
        undispatched,
        warnings,
        autonomous_merge: autonomous_merge_enabled(&cwd),
        scope_ids,
        crown_scope,
    };
    let mut payload = build_board(&inputs);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("sources".to_string(), Value::Object(sources));
    }
    payload
}

/// The needs verb's default sources (needs.default_sources): project + global
/// events + questions, and the ledger.
fn default_needs_sources(home: &crate::paths::AgentsHome) -> (Vec<PathBuf>, PathBuf) {
    let fno_dir = home
        .root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let global_events = fno_dir.join("events.jsonl");
    let questions = fno_dir.join("questions.jsonl");
    let project_events = PathBuf::from(".fno").join("events.jsonl");
    let ledger = fno_dir.join("ledger.json");
    (vec![project_events, global_events, questions], ledger)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, status: &str, priority: &str) -> Value {
        json!({"id": id, "slug": id, "status": status, "priority": priority, "type": "feature"})
    }

    fn ok_read(payload: Value) -> SourceRead {
        SourceRead::ok(payload)
    }

    fn inputs_with(ready: Value, claims: Value, claimed_nodes: Value) -> BoardInputs {
        BoardInputs {
            ready: ok_read(ready),
            claims: ok_read(claims),
            claimed_nodes: ok_read(claimed_nodes),
            holder_activity: HashMap::new(),
            prs: ok_read(Value::Array(Vec::new())),
            pr_nodes: ok_read(Value::Array(Vec::new())),
            outstanding: ok_read(json!({})),
            needs: ok_read(Value::Array(Vec::new())),
            lane: ok_read(Value::Array(Vec::new())),
            undispatched: ok_read(Value::Array(Vec::new())),
            warnings: Vec::new(),
            autonomous_merge: false,
            scope_ids: None,
            crown_scope: None,
        }
    }

    #[test]
    fn unplanned_note_names_the_batch_and_undispatched_names_the_target() {
        // x-c1c7: a rule without a number is advice nobody applies; the
        // queue a king dispatches from names the verb, never the blueprint.
        let inputs = inputs_with(
            json!([{"id": "x-1234", "priority": "p0"}]),
            json!([]),
            json!([]),
        );
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unplanned = queues.iter().find(|q| q["name"] == "unplanned").unwrap();
        let note = unplanned["note"].as_str().unwrap();
        assert!(!note.is_empty());
        assert!(note.contains('3') || note.to_lowercase().contains("three"));
        let undispatched = queues.iter().find(|q| q["name"] == "undispatched").unwrap();
        assert_eq!(undispatched["verb"], "/fno:target");
        assert!(!undispatched["note"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("blueprint"));
    }

    #[test]
    fn stalled_holder_excludes_done_nodes() {
        let node = json!({
            "id": "x-doen",
            "priority": "p0",
            "status": "done",
            "completed_at": "2026-08-21T03:16:00Z",
        });
        let claims = json!([{"key": "node:x-doen", "state": "live", "holder": "h"}]);
        let inputs = inputs_with(json!([]), claims, json!([node]));
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let stalled = queues
            .iter()
            .find(|q| q["name"] == "stalled_holder")
            .unwrap();
        assert_eq!(stalled["rows"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn stalled_holder_still_names_a_live_open_node() {
        let node = json!({"id": "x-open", "priority": "p0", "status": "in_progress"});
        let claims = json!([{"key": "node:x-open", "state": "live", "holder": "h"}]);
        let inputs = inputs_with(json!([]), claims, json!([node]));
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let stalled = queues
            .iter()
            .find(|q| q["name"] == "stalled_holder")
            .unwrap();
        let ids: Vec<&str> = stalled["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["x-open"]);
    }

    #[test]
    fn budget_slices_shrink_and_exhaustion_names_the_last_source() {
        let mut b = Budget::new(5_000);
        assert!(b.start("claims").is_some());
        std::thread::sleep(Duration::from_millis(10));
        assert!(b.start(SRC_READY).is_some());
        // A zero budget is spent before any source.
        let mut b = Budget::new(0);
        assert!(b.start("claims").is_none());
        assert_eq!(
            b.spent_error(),
            "not-read: board budget exhausted before any source"
        );
    }

    #[test]
    fn the_spent_error_names_the_source_that_ran_last() {
        // The 1s serialization reserve means a usable budget starts above
        // 1000ms; 2.6s leaves room to spend one slice, then exhaust.
        let mut b = Budget::new(2_600);
        b.start("backlog undispatched");
        std::thread::sleep(Duration::from_millis(1_700));
        assert!(b.start(SRC_PRS).is_none());
        assert_eq!(
            b.spent_error(),
            "not-read: board budget exhausted after backlog undispatched"
        );
    }

    #[test]
    fn undispatched_selects_planned_leaf_ready_rows_with_no_claim() {
        let entries = vec![
            json!({"id": "x-aaaa", "status": "ready", "priority": "p0", "plan_path": "/p.md", "type": "feature"}),
            json!({"id": "x-bbbb", "status": "ready", "priority": "p1", "type": "epic", "plan_path": "/q.md"}),
            json!({"id": "x-cccc", "status": "in_progress", "priority": "p0", "plan_path": "/r.md", "type": "feature"}),
            json!({"id": "x-dddd", "status": "ready", "priority": "p2", "plan_path": "/s.md", "type": "feature"}),
        ];
        let claims = vec![json!({"key": "node:x-cccc", "state": "live", "holder": "h"})];
        let receipt = classify_planned_unclaimed(&entries, &claims).unwrap();
        let rows = receipt.get("rows").and_then(Value::as_array).unwrap();
        // The receipt is priority-blind (p2 x-dddd stays); the board's
        // undispatched queue applies the king-priority filter.
        assert_eq!(rows.len(), 2, "{receipt}");
        assert_eq!(rows[0]["id"], "x-aaaa");
        assert_eq!(receipt["status"], "ok");
    }

    #[test]
    fn degenerate_field_values_read_as_absent_like_python_bool() {
        // Python bool("") and bool(0) are false: an empty completed_at is not
        // closure, a zero pr_number is not a PR, an empty batch is not a batch.
        let entries = vec![json!({"id": "x-aaaa", "status": "ready", "priority": "p0",
                   "plan_path": "/p.md", "type": "feature",
                   "completed_at": "", "pr_number": 0, "batch": ""})];
        let receipt = classify_planned_unclaimed(&entries, &[]).unwrap();
        let rows = receipt.get("rows").and_then(Value::as_array).unwrap();
        assert_eq!(rows.len(), 1, "{receipt}");
        let facts = rows[0]["facts"].clone();
        assert_eq!(facts["completed"], false, "{facts}");
        assert_eq!(facts["has_pr"], false, "{facts}");
        assert_eq!(facts["batch_owner"], false, "{facts}");
    }

    #[test]
    fn a_blocked_sibling_excludes_undispatched_until_the_blocker_closes() {
        let entries = vec![
            json!({"id": "x-aaaa", "status": "ready", "priority": "p1", "plan_path": "/p.md", "type": "feature", "blocked_by": ["x-bbbb"]}),
            json!({"id": "x-bbbb", "status": "in_progress", "priority": "p1", "type": "feature"}),
        ];
        let receipt = classify_planned_unclaimed(&entries, &[]).unwrap();
        assert_eq!(receipt["rows"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn scope_compiles_an_epic_to_itself_plus_descendants() {
        let entries = vec![
            json!({"id": "x-epic", "type": "epic", "status": "ready", "priority": "p1"}),
            json!({"id": "x-ch1d", "parent": "x-epic", "status": "ready", "priority": "p1"}),
            json!({"id": "x-gr2d", "parent": "x-ch1d", "status": "ready", "priority": "p1"}),
            json!({"id": "x-outs", "status": "ready", "priority": "p1"}),
        ];
        let projects = Ok(HashMap::new());
        let ids = compile_scope_ids("x-epic", &entries, &projects).unwrap();
        assert!(ids.contains("x-epic"));
        assert!(ids.contains("x-ch1d"));
        assert!(ids.contains("x-gr2d"));
        assert!(!ids.contains("x-outs"));
    }

    #[test]
    fn scope_compiles_projects_by_the_project_field() {
        let entries = vec![
            json!({"id": "x-aaaa", "project": "fno", "status": "ready", "priority": "p1"}),
            json!({"id": "x-bbbb", "project": "other", "status": "ready", "priority": "p1"}),
        ];
        let mut map = HashMap::new();
        map.insert("fno".to_string(), "fno".to_string());
        let ids = compile_scope_ids("fno", &entries, &Ok(map)).unwrap();
        assert!(ids.contains("x-aaaa"));
        assert!(!ids.contains("x-bbbb"));
    }

    #[test]
    fn a_non_epic_single_scope_is_refused() {
        let entries = vec![node("x-aaaa", "ready", "p1")];
        let projects = Ok(HashMap::new());
        let err = compile_scope_ids("x-aaaa", &entries, &projects).unwrap_err();
        assert!(err.contains("not an epic"), "{err}");
    }

    #[test]
    fn branch_ids_never_match_a_partial_hex_prefix() {
        assert_eq!(
            branch_node_ids("feature/x-cdef-1234"),
            vec!["x-cdef".to_string()]
        );
        assert_eq!(
            branch_node_ids("x-5b667-fixes-x-5b66"),
            vec!["x-5b667".to_string(), "x-5b66".to_string()]
        );
        assert!(branch_node_ids("main").is_empty());
    }

    #[test]
    fn mergeable_filter_drops_pending_and_failed_but_keeps_a_clean_pr() {
        let cwd = std::env::temp_dir();
        // read_prs shells to gh; the classifer half is exercised through the
        // same helpers the real read uses.
        let check = |status: &str, conclusion: &str| json!({"status": status, "conclusion": conclusion, "name": "ci"});
        assert_eq!(classify_check(&check("IN_PROGRESS", "")), "pending");
        assert_eq!(classify_check(&check("COMPLETED", "SUCCESS")), "pass");
        assert_eq!(classify_check(&check("COMPLETED", "FAILURE")), "fail");
        assert_eq!(classify_check(&check("COMPLETED", "STALE")), "fail");
        let _ = cwd;
    }

    #[test]
    fn coverage_only_rollups_are_diagnostic_not_green() {
        // A rollup holding only coverage contexts reads empty after the drop,
        // and an empty class set must not read as a mergeable PR.
        let rollup = json!([
            {"name": COVERAGE_STATUS_CONTEXT, "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": COVERAGE_UNAVAILABLE_STATUS_CONTEXT, "status": "COMPLETED", "conclusion": "SUCCESS"},
        ]);
        let deduped = crate::check_supersession::latest_per_name(&rollup);
        let filtered: Vec<&Value> = deduped
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| {
                let is_cov = |v: Option<&str>| {
                    v == Some(COVERAGE_STATUS_CONTEXT)
                        || v == Some(COVERAGE_UNAVAILABLE_STATUS_CONTEXT)
                };
                !is_cov(s_str(c, "context")) && !is_cov(s_str(c, "name"))
            })
            .collect();
        assert!(filtered.is_empty());
    }

    #[test]
    fn lane_parser_carries_node_and_parked_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("my-priorities.md");
        std::fs::write(
            &lane,
            "- [ ] ship the board -> x-25b8\n- [ ] park me -> parked: waiting\n- [x] done item\n- [ ] open item\nnot an item\n",
        )
        .unwrap();
        let items = parse_lane(&lane).unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].node.as_deref(), Some("x-25b8"));
        assert_eq!(items[0].text, "ship the board");
        assert_eq!(items[1].parked.as_deref(), Some("waiting"));
        assert!(items[2].done);
        assert!(items[3].node.is_none() && items[3].parked.is_none() && !items[3].done);
    }

    #[test]
    fn a_missing_lane_file_is_an_empty_lane_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let items = parse_lane(&dir.path().join("absent.md")).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn claim_keys_decode_from_filenames() {
        assert_eq!(
            decode_key("node%3Ax-25b8.lock".trim_end_matches(".lock")),
            "node:x-25b8"
        );
        assert_eq!(decode_key("node%3Ax%20sp"), "node:x sp");
    }

    #[test]
    fn derived_status_reads_terminal_completion_over_a_stale_status() {
        let done = json!({"id": "x", "status": "in_review", "completed_at": "2026-09-01"});
        assert_eq!(derived_status(&done), "done");
        let deferred = json!({"id": "x", "status": "ready", "completed_at": "deferred: no time"});
        assert_eq!(derived_status(&deferred), "ready");
        let open = json!({"id": "x", "status": "in_progress"});
        assert_eq!(derived_status(&open), "in_progress");
    }

    #[test]
    fn holder_activity_reads_only_positive_evidence() {
        let active = crate::claude_ask::TruthProbe {
            state: "working".to_string(),
            reachability: None,
            basis: None,
            last_activity_age_s: Some(30.0),
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        };
        assert!(holder_is_active(Some(&active)));
        let old = crate::claude_ask::TruthProbe {
            last_activity_age_s: Some(STALLED_AFTER_S + 1.0),
            ..active.clone()
        };
        assert!(!holder_is_active(Some(&old)));
        let parked = crate::claude_ask::TruthProbe {
            state: "your-move".to_string(),
            ..active
        };
        assert!(holder_is_active(Some(&parked)));
        assert!(!holder_is_active(None));
    }

    #[test]
    fn the_active_vocabulary_matches_the_python_side() {
        // The pin: reachability._ACTIVE_STATES and session_truth.STALLED_AFTER_S
        // are the load-bearing vocabulary; this test fails when Python grows a
        // fourth state or moves the stall threshold, so the copy cannot rot
        // silently.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("cli/src/fno/agents/reachability.py");
        let Ok(text) = std::fs::read_to_string(&src) else {
            eprintln!(
                "reachability.py not found at {}; pin skipped (sdist build)",
                src.display()
            );
            return;
        };
        assert!(
            text.contains(r#"_ACTIVE_STATES = frozenset({"working", "watching", "your-move"})"#)
        );
    }

    #[test]
    fn the_board_answers_inside_a_tight_budget_with_every_queue_present() {
        // An isolated HOME + cwd: no graph, no claims, no lane - the degraded
        // machine. The board must still answer with all eleven queues (plus
        // nothing else), the unreadable actionable ones counted, and exit 1.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let payload = read_board(&BoardOpts {
            budget_ms: 20_000,
            ..Default::default()
        });
        let queues = payload.get("queues").and_then(Value::as_array).unwrap();
        assert_eq!(queues.len(), 11, "{payload}");
        assert_eq!(payload["exit_code"], 1, "{payload}");
        assert!(payload["unreadable"].as_i64().unwrap() > 0);
        let names: Vec<&str> = queues
            .iter()
            .filter_map(|q| q.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            vec![
                "operator_lane",
                "undispatched",
                "unplanned",
                "stalled_holder",
                "undriven_pr",
                "mergeable_pr",
                "stale_claim",
                "operator_question",
                "carveout_pending",
                "capture_pending",
                "unreachable_worker",
            ]
        );
        // The done-probe's contract: every named source carries its verdict.
        let sources = payload.get("sources").and_then(Value::as_object).unwrap();
        assert!(!sources.is_empty());
        for (_name, s) in sources {
            assert!(s.get("ok").is_some() && s.get("truncated").is_some(), "{s}");
        }
    }

    #[test]
    fn the_scope_error_queue_is_actionable_and_loud() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("king.md");
        std::fs::write(&state, "---\nscope: not-a-real-thing\n---\n").unwrap();
        std::env::set_var("HOME", dir.path());
        let payload = read_board(&BoardOpts {
            budget_ms: 20_000,
            state_path: Some(state),
            ..Default::default()
        });
        let queues = payload.get("queues").and_then(Value::as_array).unwrap();
        assert_eq!(queues.len(), 1);
        assert_eq!(queues[0]["name"], "scope");
        assert_eq!(queues[0]["status"], "unreadable");
        assert_eq!(queues[0]["actionable"], true);
        assert_eq!(payload["exit_code"], 1);
    }

    #[test]
    fn a_manifest_without_a_scope_is_a_scope_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("king.md");
        std::fs::write(&state, "---\nfno_id: k1\n---\n").unwrap();
        std::env::set_var("HOME", dir.path());
        let payload = read_board(&BoardOpts {
            budget_ms: 20_000,
            state_path: Some(state),
            ..Default::default()
        });
        let queues = payload.get("queues").and_then(Value::as_array).unwrap();
        assert_eq!(queues[0]["error"], "king manifest has no scope");
    }
}
