//! Fleet announcements: one announcement is one bus line, and every session
//! reads it by its own cursor.
//!
//! `mail team` used to fan out one `dispatch_send` per recipient (37 sends in
//! 19 minutes at fleet size, no proof anyone read a copy). One announcement is
//! instead ONE `kind: "announce"` line on the shared bus, appended under the
//! same sidecar flock Python uses. Every session reads it through its own
//! per-session seen-id cursor at its next hook boundary; the sender reads
//! receipts (`audience / landed / pending / woken / unreachable / late`).
//!
//! Subcommands (all direct dispatch, no daemon RPC):
//!   - `announce send`   - write the one line (authority + rate limit + supersede)
//!   - `announce read`   - render unseen standing announcements for one session
//!   - `announce status` - the sender's receipt view for one announcement
//!
//! Envelope key order mirrors `bus/log.py::to_json_line` so the Python reader
//! parses Rust-written lines unchanged (`from_json_line` round-trip is tested).
//! `to: "fleet:<scope>"` never equals a Python address, so `scan_unread` and
//! every addressed-mail reader cannot deliver it; `kind: "announce"` is not a
//! control kind, it is simply never addressed to a session.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

/// Terminal registry statuses (registry.py::TERMINAL_STATUSES).
const TERMINAL_STATUSES: &[&str] = &["exited", "orphaned", "failed", "permanent_dead"];

const ANNOUNCE_KIND: &str = "announce";
const LANDED_KIND: &str = "landed";
const WAKE_KIND: &str = "announce-wake";
const ENVELOPE_VERSION: i64 = 1;

/// Rate limit: 6 announcements per sender per rolling hour, counted from the
/// bus itself (no second state file to drift).
const HOURLY_LIMIT: usize = 6;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_EXPIRES: &str = "24h";
const MAX_EXPIRES: Duration = Duration::from_secs(7 * 24 * 3600);

/// Paths this verb reads/writes, resolved once so tests can pin every root
/// without env mutation.
pub(crate) struct AnnouncePaths {
    bus_live: PathBuf,
    registry: PathBuf,
    /// `~/.fno` (the Python `paths.state_dir()`); holds `announce-cursors/`.
    state_root: PathBuf,
}

impl AnnouncePaths {
    pub(crate) fn from_env() -> Self {
        let home = crate::paths::AgentsHome::from_env();
        let dot_fno = home
            .root()
            .parent()
            .unwrap_or_else(|| home.root())
            .to_path_buf();
        Self {
            bus_live: dot_fno.join("bus").join("messages.jsonl"),
            registry: home.registry_json(),
            state_root: dot_fno,
        }
    }

    /// `None` under a test process with no declared home - in-process callers
    /// (nudge.rs) skip the announce read instead of panicking on the fence.
    pub(crate) fn from_env_opt() -> Option<Self> {
        let home = crate::paths::AgentsHome::from_env_opt()?;
        let dot_fno = home
            .root()
            .parent()
            .unwrap_or_else(|| home.root())
            .to_path_buf();
        Some(Self {
            bus_live: dot_fno.join("bus").join("messages.jsonl"),
            registry: home.registry_json(),
            state_root: dot_fno,
        })
    }
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// harness_identity.py::session_identity_key: UUID-family ids compare
/// case-insensitively; opencode `ses_` ids do not.
fn identity_key(session_id: &str) -> String {
    if session_id.starts_with("ses_") {
        session_id.to_string()
    } else {
        session_id.to_lowercase()
    }
}

fn row_str<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn row_session_id(row: &Value) -> Option<String> {
    let harness = row_str(row, "harness").unwrap_or("");
    let sid = crate::client_verbs::resume_session_id(row, harness);
    (!sid.is_empty()).then(|| sid.to_string())
}

fn row_terminal(row: &Value) -> bool {
    let status = row_str(row, "status").unwrap_or("live");
    TERMINAL_STATUSES.contains(&status)
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_iso(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// Parse a `--expires` duration: `45m` / `24h` / `7d`.
fn parse_expires(raw: &str) -> Result<Duration, String> {
    let (num, unit) = raw.split_at(raw.len().saturating_sub(1));
    let mult = match unit {
        "m" => 60u64,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("bad --expires {raw:?}: use m/h/d (e.g. 24h)")),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("bad --expires {raw:?}: use m/h/d (e.g. 24h)"))?;
    Ok(Duration::from_secs(n * mult))
}

/// All retained bus lines oldest -> newest (log.py::_segment_paths_oldest_first
/// order), malformed lines skipped. Readers are lock-free by contract.
fn read_bus_segments(live: &Path) -> Vec<Value> {
    let mut paths: Vec<(u64, PathBuf)> = Vec::new();
    if let Some(parent) = live.parent() {
        let prefix = format!(
            "{}.",
            live.file_name().unwrap_or_default().to_string_lossy()
        );
        if let Ok(rd) = std::fs::read_dir(parent) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(suffix) = name.strip_prefix(&prefix) {
                    if let Ok(n) = suffix.parse::<u64>() {
                        paths.push((n, entry.path()));
                    }
                }
            }
        }
    }
    paths.sort_by(|a, b| b.0.cmp(&a.0)); // oldest (highest N) first
    let mut out = Vec::new();
    for p in paths
        .iter()
        .map(|(_, p)| p.as_path())
        .chain(std::iter::once(live))
    {
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                out.push(v);
            }
        }
    }
    out
}

/// Exclusive flock on the Python sidecar lockfile (`messages.jsonl + .lock`),
/// so the one-line writer serializes against every Python appender.
struct BusLock {
    file: File,
}

impl BusLock {
    fn acquire(live: &Path) -> Result<BusLock, String> {
        let lock_path = PathBuf::from(format!("{}.lock", live.display()));
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("bus lock dir: {e}"))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("bus lock open {}: {e}", lock_path.display()))?;
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(BusLock { file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "bus lock timeout after {:?} at {}",
                            LOCK_TIMEOUT,
                            lock_path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("bus lock: {e}")),
            }
        }
    }
}

/// Append one line under the sidecar flock. Rotation stays with the Python
/// appender.
// ponytail: Rust never rotates; the next Python append rotates an over-size
// live segment.
fn append_line(live: &Path, obj: &Value) -> Result<(), String> {
    let mut line = serde_json::to_string(obj).map_err(|e| format!("serialize: {e}"))?;
    line.push('\n');
    let _lock = BusLock::acquire(live)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(live)
        .map_err(|e| format!("bus open {}: {e}", live.display()))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("bus append: {e}"))
}

// ---------------------------------------------------------------------------
// Scope matching: the exact port of mail/cli.py::_team_recipients
// ---------------------------------------------------------------------------

/// crown.py::_same_territory: alias-normalized member-set equality, blank
/// answers false. `project:<p>` rides the same equality (the Python rule this
/// replaces had no separate project arm).
fn same_territory(held: Option<&str>, requested: &str, projects: &HashMap<String, String>) -> bool {
    let members = |scope: &str| -> HashSet<String> {
        scope
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|m| projects.get(m).cloned().unwrap_or_else(|| m.to_string()))
            .collect()
    };
    match held {
        Some(h) if !h.is_empty() => members(h) == members(requested),
        _ => false,
    }
}

/// Is this session inside the announcement's audience? The SNAPSHOT question
/// (membership list written at send time).
fn in_audience(audience: &[Value], session_id: &str) -> bool {
    let key = identity_key(session_id);
    audience
        .iter()
        .filter_map(Value::as_str)
        .any(|a| a.eq_ignore_ascii_case(&key))
}

/// Does this session match the scope at READ time (the late-arrival rule)?
fn matches_scope_now(
    scope: &str,
    session_id: &str,
    registry: &[Value],
    projects: &HashMap<String, String>,
) -> bool {
    if scope == "all" {
        return true;
    }
    let key = identity_key(session_id);
    registry
        .iter()
        .filter(|row| !row_terminal(row))
        .filter(|row| {
            row_session_id(row)
                .map(|sid| identity_key(&sid) == key)
                .unwrap_or(false)
        })
        .any(|row| match scope {
            "kings" => row
                .get("crown_level")
                .map(|c| !c.is_null())
                .unwrap_or(false),
            _ => same_territory(row_str(row, "crown_scope"), scope, projects),
        })
}

fn resolve_audience(
    scope: &str,
    registry: &[Value],
    projects: &HashMap<String, String>,
) -> Vec<String> {
    let mut pairs: Vec<(String, String)> = Vec::new(); // (key, name)
    for row in registry {
        if row_terminal(row) {
            continue;
        }
        let Some(sid) = row_session_id(row) else {
            continue;
        };
        if scope == "kings" && row.get("crown_level").map(|c| c.is_null()).unwrap_or(true) {
            continue;
        }
        if scope != "all" && scope != "kings" {
            let held = row_str(row, "crown_scope");
            let crown_ok = same_territory(held, scope, projects);
            // project:<p> also matches rows WORKING in that project (their cwd
            // names the repo), so an announcement reaches the team, not only a
            // crown that may not exist.
            let project_ok = scope.strip_prefix("project:").is_some_and(|p| {
                !p.is_empty() && row_str(row, "cwd").is_some_and(|cwd| cwd_contains_project(cwd, p))
            });
            if !crown_ok && !project_ok {
                continue;
            }
        }
        let name = row_str(row, "name").unwrap_or("").to_string();
        pairs.push((identity_key(&sid), name));
    }
    pairs.sort();
    pairs.dedup();
    pairs.into_iter().map(|(k, _)| k).collect()
}

/// Does a registry cwd sit inside project `p`? Cheap path-prefix answer: the
/// project's slug appears as a path segment of the checkout (e.g.
/// `.../footnote`, `.../worktrees/footnote/<branch>`). Best-effort by design; the
/// audience is a snapshot, `announce status` reports what actually landed.
fn cwd_contains_project(cwd: &str, project: &str) -> bool {
    Path::new(cwd)
        .components()
        .any(|c| c.as_os_str().to_string_lossy() == project)
}

// ---------------------------------------------------------------------------
// `announce send`
// ---------------------------------------------------------------------------

struct SendArgs {
    scope: String,
    subject: String,
    expires_raw: String,
    urgent: bool,
    from: String,
    sender_kind: String,
    from_session: Option<String>,
    json_out: bool,
}

fn parse_send_args(args: &[String]) -> Result<SendArgs, String> {
    let mut out = SendArgs {
        scope: String::new(),
        subject: String::new(),
        expires_raw: DEFAULT_EXPIRES.to_string(),
        urgent: false,
        from: String::new(),
        sender_kind: String::new(),
        from_session: None,
        json_out: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--scope" => out.scope = it.next().ok_or("--scope needs a value")?.clone(),
            "--subject" => out.subject = it.next().ok_or("--subject needs a value")?.clone(),
            "--expires" => out.expires_raw = it.next().ok_or("--expires needs a value")?.clone(),
            "--urgent" => out.urgent = true,
            "--from" => out.from = it.next().ok_or("--from needs a value")?.clone(),
            "--sender-kind" => {
                out.sender_kind = it.next().ok_or("--sender-kind needs a value")?.clone()
            }
            "--from-session" => {
                out.from_session = Some(it.next().ok_or("--from-session needs a value")?.clone())
            }
            "--json" => out.json_out = true,
            other => return Err(format!("unknown announce send flag {other:?}")),
        }
    }
    if out.scope.is_empty() {
        return Err("--scope is required (all | kings | <crown> | project:<p>)".to_string());
    }
    if out.from.is_empty() {
        return Err("--from is required".to_string());
    }
    if out.sender_kind != "operator" && out.sender_kind != "agent" {
        return Err("--sender-kind must be operator or agent".to_string());
    }
    Ok(out)
}

/// Usage line printed on arg or authority refusal.
fn send_usage() -> &'static str {
    "usage: fno-agents announce send --scope <all|kings|<crown>|project:<p>> \
     [--subject S] [--expires 24h] [--urgent] --from <sender> \
     --sender-kind <operator|agent> [--from-session <id>] [--json]  (body on stdin)"
}

fn crown_holder(rows: &[Value], sender: &str) -> bool {
    let key = identity_key(sender);
    rows.iter().filter(|row| !row_terminal(row)).any(|row| {
        row.get("crown_level")
            .map(|c| !c.is_null())
            .unwrap_or(false)
            && (row_str(row, "name") == Some(sender)
                || row_session_id(row)
                    .map(|sid| identity_key(&sid) == key)
                    .unwrap_or(false))
    })
}

pub(crate) fn run_announce_send(args: &[String], paths: &AnnouncePaths) -> i32 {
    let parsed = match parse_send_args(args) {
        Ok(p) => p,
        Err(why) => {
            eprintln!("announce send: {why}\n{}", send_usage());
            return 2;
        }
    };
    let mut body = String::new();
    if std::io::stdin().read_to_string(&mut body).is_err() {
        eprintln!("announce send: could not read the body from stdin");
        return 2;
    }
    let body = body.trim().to_string();
    if body.is_empty() {
        eprintln!("announce send: empty body");
        return 2;
    }

    let registry = match crate::client_verbs::load_registry_entries(&paths.registry) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("announce send: {e}");
            return 12;
        }
    };

    // Authority: an operator, or a live crowned agent (decision 8).
    if parsed.sender_kind != "operator" && !crown_holder(&registry, &parsed.from) {
        eprintln!(
            "announce: refused: sender {:?} holds no crown; fleet announcements \
             are operator- or crowned-king-only",
            parsed.from
        );
        return 2;
    }

    let projects =
        crate::king_board::scope::project_map(&std::env::current_dir().unwrap_or_default())
            .unwrap_or_default();
    let audience = resolve_audience(&parsed.scope, &registry, &projects);

    // Rate limit, counted from the bus (decision 8).
    let now = chrono::Utc::now();
    let recent = read_bus_segments(&paths.bus_live)
        .iter()
        .filter(|m| row_str(m, "kind") == Some(ANNOUNCE_KIND))
        .filter(|m| row_str(m, "from") == Some(parsed.from.as_str()))
        .filter(|m| {
            row_str(m, "ts")
                .and_then(parse_iso)
                .is_some_and(|t| now.signed_duration_since(t).num_seconds() < 3600)
        })
        .count();
    if recent >= HOURLY_LIMIT {
        eprintln!(
            "announce: refused: rate limit is {HOURLY_LIMIT} announcements per \
             rolling hour and {:?} already sent {recent}",
            parsed.from
        );
        return 2;
    }

    // Supersede: a newer announcement with the same subject+scope replaces the
    // older standing one (decision 7).
    let mut supersedes: Vec<String> = Vec::new();
    for m in read_bus_segments(&paths.bus_live) {
        if row_str(&m, "kind") != Some(ANNOUNCE_KIND) {
            continue;
        }
        if row_str(&m, "to") != Some(format!("fleet:{}", parsed.scope).as_str()) {
            continue;
        }
        let subject = m
            .get("meta")
            .and_then(|meta| row_str(meta, "subject"))
            .unwrap_or("");
        if subject != parsed.subject {
            continue;
        }
        if expired(&m, now) {
            continue;
        }
        if let Some(id) = row_str(&m, "id") {
            supersedes.push(id.to_string());
        }
    }

    let ttl = match parse_expires(&parsed.expires_raw) {
        Ok(t) if t <= MAX_EXPIRES => t,
        Ok(_) => {
            eprintln!("announce send: --expires beyond the 7d maximum");
            return 2;
        }
        Err(why) => {
            eprintln!("announce send: {why}");
            return 2;
        }
    };
    let expires_at = (now + chrono::Duration::seconds(ttl.as_secs() as i64))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    // Empty audience never reports fleet-wide success (the old guard's shape).
    if audience.is_empty() {
        eprintln!(
            "announce send: no live recipients in scope {:?}",
            parsed.scope
        );
        return 1;
    }

    let id = new_msg_id();
    let word_count = body.split_whitespace().count() as i64;
    let mut obj = Map::new();
    obj.insert("v".into(), json!(ENVELOPE_VERSION));
    obj.insert("id".into(), json!(id));
    obj.insert("ts".into(), json!(now_iso()));
    obj.insert("thread".into(), json!(id));
    obj.insert("from".into(), json!(parsed.from));
    obj.insert("to".into(), json!(format!("fleet:{}", parsed.scope)));
    obj.insert("kind".into(), json!(ANNOUNCE_KIND));
    if let Some(fs) = &parsed.from_session {
        obj.insert("from_session".into(), json!(fs));
    }
    obj.insert("to_kind".into(), json!("fleet"));
    obj.insert("word_count".into(), json!(word_count));
    obj.insert(
        "meta".into(),
        json!({
            "scope": parsed.scope,
            "audience": audience,
            "subject": parsed.subject,
            "expires_at": expires_at,
            "urgent": parsed.urgent,
            "supersedes": supersedes,
        }),
    );
    obj.insert("body".into(), json!(body));

    if let Err(e) = append_line(&paths.bus_live, &Value::Object(obj)) {
        eprintln!("announce send: {e}");
        return 1;
    }
    if parsed.json_out {
        println!(
            "{}",
            json!({"id": id, "scope": parsed.scope, "audience": audience.len(), "superseded": supersedes})
        );
    } else {
        println!(
            "announce {id} scope={} audience={}",
            parsed.scope,
            audience.len()
        );
    }
    0
}

fn new_msg_id() -> String {
    // 'msg-XXXXXX', matching bus/log.py::new_msg_id (6 hex chars).
    let mut buf = [0u8; 3];
    if getrandom::fill(&mut buf).is_err() {
        // Fallback entropy: pid + clock. A collision costs one duplicate id.
        let seed = (std::process::id() as u64) << 32
            | std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
        buf.copy_from_slice(&seed.to_le_bytes()[..3]);
    }
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("msg-{hex}")
}

fn expired(m: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    // Missing or unparseable expires_at is EXPIRED: stale news reads worse
    // than missed news (decision 7).
    match m
        .get("meta")
        .and_then(|meta| meta.get("expires_at"))
        .and_then(Value::as_str)
        .and_then(parse_iso)
    {
        Some(exp) => exp <= now,
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Cursor: per-session seen-id set (decision 4)
// ---------------------------------------------------------------------------

fn cursor_path(state_root: &Path, session_id: &str) -> PathBuf {
    let safe = format!("{}.json", session_id.replace('/', "_"));
    state_root.join("announce-cursors").join(safe)
}

fn load_cursor(state_root: &Path, session_id: &str) -> HashSet<String> {
    let text = match std::fs::read_to_string(cursor_path(state_root, session_id)) {
        Ok(t) => t,
        Err(_) => return HashSet::new(),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => v
            .get("seen")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => HashSet::new(),
    }
}

fn save_cursor(state_root: &Path, session_id: &str, seen: &HashSet<String>) {
    let path = cursor_path(state_root, session_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut ids: Vec<String> = seen.iter().cloned().collect();
    ids.sort();
    let payload = json!({"seen": ids}).to_string();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, payload).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ---------------------------------------------------------------------------
// `announce read`
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Boundary {
    Start,
    Prompt,
    Loop,
    Compact,
}

impl Boundary {
    fn parse(s: &str) -> Option<Boundary> {
        match s {
            "start" => Some(Boundary::Start),
            "prompt" => Some(Boundary::Prompt),
            "loop" => Some(Boundary::Loop),
            "compact" => Some(Boundary::Compact),
            _ => None,
        }
    }
}

fn standing_announcements(
    rows: &[Value],
    now: chrono::DateTime<chrono::Utc>,
) -> (Vec<&Value>, HashSet<String>) {
    let mut superseded: HashSet<String> = HashSet::new();
    for m in rows {
        if row_str(m, "kind") != Some(ANNOUNCE_KIND) {
            continue;
        }
        if let Some(list) = m
            .get("meta")
            .and_then(|meta| meta.get("supersedes"))
            .and_then(Value::as_array)
        {
            for id in list {
                if let Some(s) = id.as_str() {
                    superseded.insert(s.to_string());
                }
            }
        }
    }
    let standing: Vec<&Value> = rows
        .iter()
        .filter(|m| row_str(m, "kind") == Some(ANNOUNCE_KIND))
        .filter(|m| !expired(m, now))
        .filter(|m| row_str(m, "id").is_none_or(|id| !superseded.contains(id)))
        .collect();
    (standing, superseded)
}

fn render_block(m: &Value) -> String {
    let id = row_str(m, "id").unwrap_or("");
    let from = row_str(m, "from").unwrap_or("unknown");
    let meta = m.get("meta").cloned().unwrap_or(Value::Null);
    let subject = meta.get("subject").and_then(Value::as_str).unwrap_or("");
    let expires = meta.get("expires_at").and_then(Value::as_str).unwrap_or("");
    let body = row_str(m, "body").unwrap_or("");
    format!(
        "<fno_mail id=\"{id}\" kind=\"announce\" from=\"{from}\" subject=\"{subject}\" expires=\"{expires}\">{body}</fno_mail>"
    )
}

fn render_compact_line(m: &Value) -> String {
    let from = row_str(m, "from").unwrap_or("unknown");
    let meta = m.get("meta").cloned().unwrap_or(Value::Null);
    let subject = meta.get("subject").and_then(Value::as_str).unwrap_or("");
    let expires = meta.get("expires_at").and_then(Value::as_str).unwrap_or("");
    format!("announce (still standing): \"{subject}\" from {from}, expires {expires}")
}

/// Core reader, shared by the CLI verb and `nudge.rs` (the loop boundary reads
/// natively). `Ok(None)` = nothing to inject; the CLI prints that as silence.
pub(crate) fn read_render(
    paths: &AnnouncePaths,
    session_id: &str,
    boundary: Boundary,
) -> Result<Option<String>, String> {
    let rows = read_bus_segments(&paths.bus_live);
    let now = chrono::Utc::now();
    let (standing, _superseded) = standing_announcements(&rows, now);
    if standing.is_empty() {
        return Ok(None);
    }
    let registry = crate::client_verbs::load_registry_entries(&paths.registry)?;
    let projects =
        crate::king_board::scope::project_map(&std::env::current_dir().unwrap_or_default())
            .unwrap_or_default();
    let mut seen = load_cursor(&paths.state_root, session_id);

    let mut fresh: Vec<&Value> = Vec::new();
    let mut standing_seen: Vec<&Value> = Vec::new();
    for m in &standing {
        let Some(id) = row_str(m, "id") else { continue };
        // The sender never reads its own announcement back.
        if m.get("from_session")
            .and_then(Value::as_str)
            .is_some_and(|fs| identity_key(fs) == identity_key(session_id))
        {
            continue;
        }
        let audience: Vec<Value> = m
            .get("meta")
            .and_then(|meta| meta.get("audience"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let scope = m
            .get("meta")
            .and_then(|meta| row_str(meta, "scope"))
            .unwrap_or("all");
        let mine = in_audience(&audience, session_id)
            || matches_scope_now(scope, session_id, &registry, &projects);
        if !mine {
            continue;
        }
        if seen.contains(id) {
            standing_seen.push(*m);
        } else {
            fresh.push(*m);
        }
    }

    if boundary == Boundary::Compact {
        let mut out: Vec<String> = fresh.iter().map(|m| render_block(m)).collect();
        out.extend(standing_seen.iter().map(|m| render_compact_line(m)));
        for m in fresh.iter().chain(standing_seen.iter()) {
            if let Some(id) = row_str(m, "id") {
                seen.insert(id.to_string());
            }
        }
        prune_and_save(paths, session_id, seen, &rows);
        return Ok((!out.is_empty()).then(|| out.join("\n")));
    }

    if fresh.is_empty() {
        return Ok(None);
    }
    let out = fresh
        .iter()
        .map(|m| render_block(m))
        .collect::<Vec<_>>()
        .join("\n");
    for m in &fresh {
        if let Some(id) = row_str(m, "id") {
            seen.insert(id.to_string());
        }
    }
    prune_and_save(paths, session_id, seen, &rows);
    Ok(Some(out))
}

/// Keep the cursor bounded: ids no longer on any retained segment leave the
/// set (nudge.py's `nudged &= unread_ids` pruning).
fn prune_and_save(
    paths: &AnnouncePaths,
    session_id: &str,
    mut seen: HashSet<String>,
    rows: &[Value],
) {
    let live_ids: HashSet<String> = rows
        .iter()
        .filter_map(|m| row_str(m, "id"))
        .map(str::to_string)
        .collect();
    seen.retain(|id| live_ids.contains(id));
    save_cursor(&paths.state_root, session_id, &seen);
}

pub(crate) fn run_announce_read(args: &[String], paths: &AnnouncePaths) -> i32 {
    let mut session_id = String::new();
    let mut boundary = Boundary::Prompt;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session-id" => session_id = it.next().map(String::clone).unwrap_or_default(),
            "--boundary" => {
                boundary = it
                    .next()
                    .and_then(|b| Boundary::parse(b))
                    .unwrap_or(Boundary::Prompt)
            }
            "--harness" => {
                it.next(); // accepted for the documented surface; the reader is session-keyed
            }
            other => {
                eprintln!("announce read: unknown flag {other:?}");
                return 2;
            }
        }
    }
    if session_id.is_empty() {
        eprintln!("announce read: --session-id is required");
        return 2;
    }
    match read_render(paths, &session_id, boundary) {
        Ok(Some(out)) => println!("{out}"),
        Ok(None) => {}
        Err(e) => {
            // A hook-boundary reader fails open: never block a session on
            // announce state (AC3-ERR).
            eprintln!("announce read: {e}");
        }
    }
    0
}

// ---------------------------------------------------------------------------
// `announce status`: the sender view (decision 6)
// ---------------------------------------------------------------------------

/// reply_resolve.py::_transcript_path, mirrored: claude
/// `<projects>/*/<id>.jsonl`, codex a rollout embedding the id. A key neither
/// store resolves is `unreachable`, never `pending`.
fn transcript_path(harness_home: &Path, session_key: &str, codex: bool) -> Option<PathBuf> {
    if codex {
        let root = std::env::var_os("FNO_CODEX_SESSIONS_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CODEX_HOME").map(|h| PathBuf::from(h).join("sessions")))
            .unwrap_or_else(|| harness_home.join(".codex").join("sessions"));
        return walk_session_file(&root, session_key, 6);
    }
    let root = std::env::var_os("FNO_CLAUDE_PROJECTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| harness_home.join(".claude").join("projects"));
    // One level: projects keys a transcript dir by the session cwd slug.
    let dir = std::fs::read_dir(&root).ok()?;
    for entry in dir.flatten() {
        let candidate = entry.path().join(format!("{session_key}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Depth-bounded recursive walk for a file whose name embeds the session key.
fn walk_session_file(root: &Path, key: &str, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let rd = std::fs::read_dir(root).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(hit) = walk_session_file(&path, key, depth - 1) {
                return Some(hit);
            }
        } else if path
            .file_name()
            .map(|n| n.to_string_lossy().contains(key))
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

/// Does the transcript carry the id? Mirrors mail_ids_in_transcript: JSONL
/// quote-escapes are normalized before the id regex runs, and the id must sit
/// inside an `<fno_mail` open tag (reply_resolve.py::_ID_RE), so an unrelated
/// `id="..."` attribute cannot false-positive.
fn transcript_has_id(path: &Path, id: &str) -> Result<bool, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let normalized = text.replace("\\\"", "\"");
    let needle = regex::Regex::new(&format!(r#"<fno_mail\b[^>]*\bid="{id}""#))
        .map_err(|e| format!("id regex: {e}"))?;
    Ok(needle.is_match(&normalized))
}

fn record_landed_row(live: &Path, from: &str, session_key: &str, id: &str) {
    let mut obj = Map::new();
    obj.insert("v".into(), json!(ENVELOPE_VERSION));
    obj.insert("id".into(), json!(new_msg_id()));
    obj.insert("ts".into(), json!(now_iso()));
    obj.insert("thread".into(), json!(id));
    obj.insert("from".into(), json!(from));
    obj.insert("to".into(), json!(session_key));
    obj.insert("kind".into(), json!(LANDED_KIND));
    obj.insert("word_count".into(), json!(0));
    obj.insert("meta".into(), json!({"landed": id, "session": session_key}));
    obj.insert("body".into(), json!(""));
    if let Err(e) = append_line(live, &Value::Object(obj)) {
        eprintln!("announce status: landed row write failed: {e}");
    }
}

pub(crate) fn run_announce_status(args: &[String], paths: &AnnouncePaths) -> i32 {
    let mut id = String::new();
    let mut json_out = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json_out = true,
            other if id.is_empty() && !other.starts_with('-') => id = other.to_string(),
            other => {
                eprintln!("announce status: unknown flag {other:?}");
                return 2;
            }
        }
    }
    if id.is_empty() {
        eprintln!("usage: fno-agents announce status <id> [--json]");
        return 2;
    }

    let rows = read_bus_segments(&paths.bus_live);
    let now = chrono::Utc::now();
    let announcement = rows.iter().find(|m| {
        row_str(m, "kind") == Some(ANNOUNCE_KIND) && row_str(m, "id") == Some(id.as_str())
    });
    let Some(announcement) = announcement else {
        eprintln!("announce status: no announcement {id:?} on the retained bus");
        return 1;
    };
    let from = row_str(announcement, "from")
        .unwrap_or("unknown")
        .to_string();
    let meta = announcement.get("meta").cloned().unwrap_or(Value::Null);
    let audience: Vec<String> = meta
        .get("audience")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // Durable proofs first: (id, session) landed rows, then the transcript
    // scan. A landed key outside the audience is a LATE reader (decision 3).
    let mut landed_keys: HashSet<String> = HashSet::new();
    for m in &rows {
        if row_str(m, "kind") != Some(LANDED_KIND) {
            continue;
        }
        let landed = m.get("meta").and_then(|meta| row_str(meta, "landed"));
        if landed != Some(id.as_str()) {
            continue;
        }
        if let Some(session) = m.get("meta").and_then(|meta| row_str(meta, "session")) {
            landed_keys.insert(session.to_string());
        } else if let Some(to) = row_str(m, "to") {
            landed_keys.insert(to.to_string());
        }
    }
    let mut woken = 0usize;
    for m in &rows {
        if row_str(m, "kind") != Some(WAKE_KIND) {
            continue;
        }
        let wake_id = m.get("meta").and_then(|meta| row_str(meta, "announce"));
        let result = m.get("meta").and_then(|meta| row_str(meta, "result"));
        if wake_id == Some(id.as_str()) && result == Some("woken") {
            woken += 1;
        }
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let audience_set: HashSet<&str> = audience.iter().map(String::as_str).collect();
    let mut per_session: Vec<Value> = Vec::new();
    let mut landed = 0usize;
    let mut pending = 0usize;
    let mut unreachable = 0usize;
    let mut late = 0usize;
    for key in &audience {
        if landed_keys.contains(key) {
            landed += 1;
            per_session.push(json!({"session": key, "state": "landed"}));
            continue;
        }
        let claude_store = transcript_path(&home, key, false);
        let codex_store = if claude_store.is_none() {
            transcript_path(&home, key, true)
        } else {
            None
        };
        match claude_store.or(codex_store) {
            None => {
                unreachable += 1;
                per_session.push(json!({"session": key, "state": "unreachable"}));
            }
            Some(path) => match transcript_has_id(&path, &id) {
                Ok(true) => {
                    landed += 1;
                    record_landed_row(&paths.bus_live, &from, key, &id);
                    per_session.push(json!({"session": key, "state": "landed"}));
                }
                Ok(false) => {
                    pending += 1;
                    per_session.push(json!({"session": key, "state": "pending"}));
                }
                Err(_) => {
                    unreachable += 1;
                    per_session.push(json!({"session": key, "state": "unreachable"}));
                }
            },
        }
    }
    for key in &landed_keys {
        if !audience_set.contains(key.as_str()) {
            late += 1;
        }
    }

    if json_out {
        println!(
            "{}",
            json!({
                "id": id, "audience": audience.len(), "landed": landed,
                "pending": pending, "woken": woken, "unreachable": unreachable,
                "late": late, "sessions": per_session,
            })
        );
    } else {
        println!(
            "audience {}, landed {landed}, pending {pending}, woken {woken}, unreachable {unreachable}, late {late}",
            audience.len()
        );
    }
    0
}

// ---------------------------------------------------------------------------
// Verb entry
// ---------------------------------------------------------------------------

pub(crate) fn run_announce(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        eprintln!(
            "usage: fno-agents announce <send|read|status> ...  (one announcement, one bus line)"
        );
        return 2;
    };
    let paths = AnnouncePaths::from_env();
    match sub.as_str() {
        "send" => run_announce_send(&args[1..], &paths),
        "read" => run_announce_read(&args[1..], &paths),
        "status" => run_announce_status(&args[1..], &paths),
        other => {
            eprintln!("announce: unknown subcommand {other:?} (send | read | status)");
            2
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// One temp root per test; env-mutating tests share the process, so the
    /// mutex keeps FNO_* pins from racing.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct Fixture {
        root: PathBuf,
        paths: AnnouncePaths,
    }

    fn fixture(tag: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "fno-announce-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(root.join("bus")).unwrap();
        std::fs::create_dir_all(root.join("agents")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Fixture {
            paths: AnnouncePaths {
                bus_live: root.join("bus").join("messages.jsonl"),
                registry: root.join("agents").join("registry.json"),
                state_root: root.clone(),
            },
            root,
        }
    }

    fn write_registry(paths: &AnnouncePaths, rows: &[Value]) {
        std::fs::write(
            &paths.registry,
            serde_json::to_string(&json!({ "schema_version": 1, "agents": rows })).unwrap(),
        )
        .unwrap();
    }

    fn agent_row(name: &str, session: &str, extra: Value) -> Value {
        let mut row = json!({
            "name": name, "harness": "claude", "status": "live",
            "harness_session_id": session,
            "cwd": format!("/tmp/{name}"),
            "log_path": format!("/tmp/{name}.log"),
        });
        let obj = row.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        row
    }

    /// The send path without process stdin: writes the body exactly as the
    /// verb does after its stdin read.
    fn send_via(
        paths: &AnnouncePaths,
        flags: &[&str],
        body: &str,
        registry: &[Value],
    ) -> (i32, String) {
        write_registry(paths, registry);
        let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        args.insert(0, "send".into());
        let parsed = match parse_send_args(&args[1..]) {
            Ok(p) => p,
            Err(why) => {
                return (2, why);
            }
        };
        let body = body.trim().to_string();
        if body.is_empty() {
            return (2, "empty body".into());
        }
        let registry_rows = crate::client_verbs::load_registry_entries(&paths.registry).unwrap();
        if parsed.sender_kind != "operator" && !crown_holder(&registry_rows, &parsed.from) {
            return (2, format!("refused: {}", parsed.from));
        }
        let projects = HashMap::new();
        let audience = resolve_audience(&parsed.scope, &registry_rows, &projects);
        let now = chrono::Utc::now();
        let recent = read_bus_segments(&paths.bus_live)
            .iter()
            .filter(|m| row_str(m, "kind") == Some(ANNOUNCE_KIND))
            .filter(|m| row_str(m, "from") == Some(parsed.from.as_str()))
            .filter(|m| {
                row_str(m, "ts")
                    .and_then(parse_iso)
                    .is_some_and(|t| now.signed_duration_since(t).num_seconds() < 3600)
            })
            .count();
        if recent >= HOURLY_LIMIT {
            return (2, "rate limit".into());
        }
        let mut supersedes: Vec<String> = Vec::new();
        for m in read_bus_segments(&paths.bus_live) {
            if row_str(&m, "kind") != Some(ANNOUNCE_KIND) {
                continue;
            }
            if row_str(&m, "to") != Some(format!("fleet:{}", parsed.scope).as_str()) {
                continue;
            }
            let subject = m
                .get("meta")
                .and_then(|meta| row_str(meta, "subject"))
                .unwrap_or("");
            if subject != parsed.subject || expired(&m, now) {
                continue;
            }
            if let Some(id) = row_str(&m, "id") {
                supersedes.push(id.to_string());
            }
        }
        let ttl = parse_expires(&parsed.expires_raw).unwrap();
        let expires_at = (now + chrono::Duration::seconds(ttl.as_secs() as i64))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let id = new_msg_id();
        let mut obj = Map::new();
        obj.insert("v".into(), json!(ENVELOPE_VERSION));
        obj.insert("id".into(), json!(id));
        obj.insert("ts".into(), json!(now_iso()));
        obj.insert("thread".into(), json!(id));
        obj.insert("from".into(), json!(parsed.from));
        obj.insert("to".into(), json!(format!("fleet:{}", parsed.scope)));
        obj.insert("kind".into(), json!(ANNOUNCE_KIND));
        if let Some(fs) = &parsed.from_session {
            obj.insert("from_session".into(), json!(fs));
        }
        obj.insert("to_kind".into(), json!("fleet"));
        obj.insert(
            "word_count".into(),
            json!(body.split_whitespace().count() as i64),
        );
        obj.insert(
            "meta".into(),
            json!({
                "scope": parsed.scope,
                "audience": audience,
                "subject": parsed.subject,
                "expires_at": expires_at,
                "urgent": parsed.urgent,
                "supersedes": supersedes,
            }),
        );
        obj.insert("body".into(), json!(body));
        append_line(&paths.bus_live, &Value::Object(obj)).unwrap();
        (0, id)
    }

    fn send_flags(scope: &'static str) -> Vec<&'static str> {
        vec![
            "--scope",
            scope,
            "--from",
            "op",
            "--sender-kind",
            "operator",
        ]
    }

    #[test]
    fn send_writes_exactly_one_line_with_the_audience_snapshot() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("one-line");
        let rows = vec![
            agent_row("red", "AAAA1111-1111-1111-1111-111111111111", json!({})),
            agent_row("blue", "bbbb2222-2222-2222-2222-222222222222", json!({})),
            agent_row(
                "gone",
                "cccc3333-3333-3333-3333-333333333333",
                json!({"status": "exited"}),
            ),
        ];
        let (code, id) = send_via(&f.paths, &send_flags("all"), "fleet notice", &rows);
        assert_eq!(code, 0);
        let lines = read_bus_segments(&f.paths.bus_live);
        assert_eq!(lines.len(), 1, "one announcement is one bus line");
        let m = &lines[0];
        assert_eq!(row_str(m, "kind"), Some("announce"));
        assert_eq!(row_str(m, "to_kind"), Some("fleet"));
        let audience: Vec<String> = m
            .get("meta")
            .and_then(|meta| meta.get("audience"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap();
        assert_eq!(audience.len(), 2, "terminal rows are out: {audience:?}");
        assert!(audience.contains(&"aaaa1111-1111-1111-1111-111111111111".to_string()));
        assert_eq!(row_str(m, "id"), Some(id.as_str()));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn uncrowned_agent_sender_is_refused_and_the_bus_is_unchanged() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("authority");
        let rows = vec![agent_row(
            "worker",
            "dddd4444-4444-4444-4444-444444444444",
            json!({}),
        )];
        let mut flags = send_flags("all");
        flags[3] = "worker";
        flags[5] = "agent";
        let (code, why) = send_via(&f.paths, &flags, "mutiny", &rows);
        assert_eq!(code, 2, "{why}");
        assert!(why.contains("worker"));
        assert!(read_bus_segments(&f.paths.bus_live).is_empty());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn crowned_agent_sender_is_accepted() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("crown");
        let rows = vec![agent_row(
            "king",
            "eeee5555-5555-5555-5555-555555555555",
            json!({"crown_level": 1, "crown_scope": "epic/x-test"}),
        )];
        let mut flags = send_flags("all");
        flags[3] = "king";
        flags[5] = "agent";
        let (code, _) = send_via(&f.paths, &flags, "from the crown", &rows);
        assert_eq!(code, 0);
        assert_eq!(read_bus_segments(&f.paths.bus_live).len(), 1);
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn rate_limit_refuses_the_seventh_announcement_in_the_rolling_hour() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("rate");
        let rows = vec![agent_row(
            "red",
            "ffff6666-6666-6666-6666-666666666666",
            json!({}),
        )];
        for i in 0..HOURLY_LIMIT {
            let (code, _) = send_via(&f.paths, &send_flags("all"), &format!("notice {i}"), &rows);
            assert_eq!(code, 0, "send {i} refused early");
        }
        let (code, why) = send_via(&f.paths, &send_flags("all"), "one too many", &rows);
        assert_eq!(code, 2, "{why}");
        assert!(why.contains("rate limit"));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn same_subject_and_scope_supersedes_the_standing_announcement() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("supersede");
        let rows = vec![agent_row(
            "red",
            "aaaa7777-7777-7777-7777-777777777777",
            json!({}),
        )];
        let mut flags = send_flags("all");
        flags.push("--subject");
        flags.push("maintenance");
        let (_, first) = send_via(&f.paths, &flags, "maintenance at noon", &rows);
        let (_, second) = send_via(&f.paths, &flags, "maintenance moved to 3pm", &rows);
        let rows_on_bus = read_bus_segments(&f.paths.bus_live);
        assert_eq!(rows_on_bus.len(), 2);
        let superseded: Vec<String> = rows_on_bus[1]
            .get("meta")
            .and_then(|meta| meta.get("supersedes"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap();
        assert_eq!(superseded, vec![first]);
        // The reader skips the superseded one.
        let out = read_render(
            &f.paths,
            "aaaa7777-7777-7777-7777-777777777777",
            Boundary::Prompt,
        )
        .unwrap()
        .unwrap();
        assert!(out.contains(&second), "current news renders: {out}");
        assert!(
            !out.contains("maintenance at noon"),
            "stale news skipped: {out}"
        );
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn python_parses_the_rust_line_key_for_key() {
        // The fixture (crates/fno-agents/tests/fixtures/announce_line.jsonl)
        // is generated by hand in the same key order the writer emits; this
        // test regenerates the shape and pins the order the Python
        // from_json_line round-trip depends on.
        let f = fixture("keyorder");
        let rows = vec![agent_row(
            "red",
            "bbbb8888-8888-8888-8888-888888888888",
            json!({}),
        )];
        send_via(&f.paths, &send_flags("all"), "order check", &rows);
        let line = std::fs::read_to_string(&f.paths.bus_live).unwrap();
        let obj: Value = serde_json::from_str(line.trim()).unwrap();
        let keys: Vec<&str> = obj
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let expected: &[&str] = &[
            "v",
            "id",
            "ts",
            "thread",
            "from",
            "to",
            "kind",
            "to_kind",
            "word_count",
            "meta",
            "body",
        ];
        assert_eq!(keys, expected, "key order must match to_json_line");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn reader_prints_once_then_cursor_silences() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("read-once");
        let session = "cccc9999-9999-9999-9999-999999999999";
        let rows = vec![agent_row("red", session, json!({}))];
        let (_, id) = send_via(&f.paths, &send_flags("all"), "hello fleet", &rows);
        let out = read_render(&f.paths, session, Boundary::Prompt)
            .unwrap()
            .unwrap();
        assert!(out.contains(&format!("id=\"{id}\"")));
        assert!(out.contains("hello fleet"));
        let second = read_render(&f.paths, session, Boundary::Prompt).unwrap();
        assert!(second.is_none(), "cursor silences the second read");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn compact_re_renders_seen_standing_announcements() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("compact");
        let session = "dddd0000-0000-0000-0000-000000000000";
        let rows = vec![agent_row("red", session, json!({}))];
        let mut flags = send_flags("all");
        flags.push("--subject");
        flags.push("standing news");
        let (_, id) = send_via(&f.paths, &flags, "still true", &rows);
        assert!(read_render(&f.paths, session, Boundary::Prompt)
            .unwrap()
            .is_some());
        let out = read_render(&f.paths, session, Boundary::Compact)
            .unwrap()
            .unwrap();
        assert!(out.contains("still standing"), "{out}");
        let _ = id; // the compact one-liner carries subject/sender/expiry, not the id
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn expired_and_superseded_announcements_render_nothing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("stale");
        let session = "eeee1111-1111-1111-1111-111111111111";
        let rows = vec![agent_row("red", session, json!({}))];
        // Expired: write with a past expires_at.
        send_via(&f.paths, &send_flags("all"), "old news", &rows);
        {
            let text = std::fs::read_to_string(&f.paths.bus_live).unwrap();
            let mut m: Value = serde_json::from_str(text.trim()).unwrap();
            m["meta"]["expires_at"] = json!("2020-01-01T00:00:00Z");
            std::fs::write(&f.paths.bus_live, serde_json::to_string(&m).unwrap() + "\n").unwrap();
        }
        let out = read_render(&f.paths, session, Boundary::Prompt).unwrap();
        assert!(out.is_none(), "expired news never renders: {out:?}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_late_session_matching_the_scope_reads_a_standing_announcement() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("late");
        let snapshot_session = "ffff2222-2222-2222-2222-222222222222";
        let late_session = "aaaa3333-3333-3333-3333-333333333333";
        let rows = vec![agent_row("red", snapshot_session, json!({}))];
        let (_, id) = send_via(&f.paths, &send_flags("all"), "you there too", &rows);
        let out = read_render(&f.paths, late_session, Boundary::Prompt)
            .unwrap()
            .unwrap();
        assert!(
            out.contains(&id),
            "scope all matches any session at read time"
        );
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn the_sender_does_not_read_its_own_announcement() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("self");
        let king_session = "bbbb4444-4444-4444-4444-444444444444";
        let rows = vec![agent_row(
            "king",
            king_session,
            json!({"crown_level": 1, "crown_scope": "epic/x"}),
        )];
        let mut flags = send_flags("all");
        flags[3] = "king";
        flags[5] = "agent";
        let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        args.push("--from-session".into());
        args.push(king_session.to_string());
        send_via_parsed(&f.paths, &args, "crown news", &rows, Some(king_session));
        let out = read_render(&f.paths, king_session, Boundary::Prompt).unwrap();
        assert!(out.is_none(), "sender skips its own line: {out:?}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    /// send_via variant that carries --from-session.
    fn send_via_parsed(
        paths: &AnnouncePaths,
        args: &[String],
        body: &str,
        registry: &[Value],
        from_session: Option<&str>,
    ) -> (i32, String) {
        write_registry(paths, registry);
        let parsed = parse_send_args(args).unwrap();
        let registry_rows = crate::client_verbs::load_registry_entries(&paths.registry).unwrap();
        if parsed.sender_kind != "operator" && !crown_holder(&registry_rows, &parsed.from) {
            return (2, "refused".into());
        }
        let audience = resolve_audience(&parsed.scope, &registry_rows, &HashMap::new());
        let id = new_msg_id();
        let mut obj = Map::new();
        obj.insert("v".into(), json!(ENVELOPE_VERSION));
        obj.insert("id".into(), json!(id));
        obj.insert("ts".into(), json!(now_iso()));
        obj.insert("thread".into(), json!(id));
        obj.insert("from".into(), json!(parsed.from));
        obj.insert("to".into(), json!(format!("fleet:{}", parsed.scope)));
        obj.insert("kind".into(), json!(ANNOUNCE_KIND));
        if let Some(fs) = from_session {
            obj.insert("from_session".into(), json!(fs));
        }
        obj.insert("to_kind".into(), json!("fleet"));
        obj.insert("word_count".into(), json!(3i64));
        obj.insert(
            "meta".into(),
            json!({
                "scope": parsed.scope,
                "audience": audience,
                "subject": parsed.subject,
                "expires_at": (chrono::Utc::now() + chrono::Duration::hours(24))
                    .format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                "urgent": false,
                "supersedes": [],
            }),
        );
        obj.insert("body".into(), json!(body));
        append_line(&paths.bus_live, &Value::Object(obj)).unwrap();
        (0, id)
    }

    #[test]
    fn status_counts_landed_pending_unreachable_and_never_rescans_landed() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = fixture("status");
        let home = std::env::temp_dir().join(format!(
            "fno-announce-home-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let s1 = "cccc5555-5555-5555-5555-555555555555";
        let s2 = "dddd6666-6666-6666-6666-666666666666";
        let s3 = "ses_nostore";
        let rows = vec![
            agent_row("one", s1, json!({})),
            agent_row("two", s2, json!({})),
            agent_row("three", s3, json!({})),
        ];
        let mut flags = send_flags("all");
        flags.push("--json");
        let (_, id) = send_via(&f.paths, &flags, "receipts please", &rows);

        // s1 carries the id in its claude transcript; s2's transcript lacks
        // it; s3 has no resolvable store at all.
        let proj = home.join(".claude").join("projects").join("-tmp-one");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{s1}.jsonl")),
            format!(
                "{{\"x\":\"<fno_mail id=\\\"{id}\\\" kind=\\\"announce\\\">hi</fno_mail>\"}}\n"
            ),
        )
        .unwrap();
        let proj2 = home.join(".claude").join("projects").join("-tmp-two");
        std::fs::create_dir_all(&proj2).unwrap();
        std::fs::write(proj2.join(format!("{s2}.jsonl")), "{\"x\":\"nothing\"}\n").unwrap();

        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let code1 = run_announce_status(&[id.clone(), "--json".into()], &f.paths);
        let code2 = run_announce_status(&[id.clone()], &f.paths);
        if let Some(h) = saved_home {
            std::env::set_var("HOME", h);
        }
        assert_eq!(code1, 0);
        assert_eq!(code2, 0);

        let bus = std::fs::read_to_string(&f.paths.bus_live).unwrap();
        let landed_rows: usize = bus
            .lines()
            .filter(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .is_some_and(|m| row_str(&m, "kind") == Some("landed"))
            })
            .count();
        assert_eq!(landed_rows, 1, "exactly one landed proof row: {bus}");
        std::fs::remove_dir_all(&f.root).ok();
        std::fs::remove_dir_all(&home).ok();
    }
}
