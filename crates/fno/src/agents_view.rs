//! Registry reader for the sideline agent rows (4a-G2, brief US2).
//!
//! The mux is a READER of the fno-agents registry (brief Locked 5): an
//! off-loop interval task parses `~/.fno/agents/registry.json` and hands the
//! core loop a derived row set; the core joins rows to live panes via the
//! `mux` ref at layout time (pane-exit fact beats any badge). Nothing here
//! ever blocks the core loop, and the render path never touches the file
//! (the origin freeze class rule).
//!
//! The registry is dual-language (fno-agents Rust daemon + Python fno), and
//! its FILE is the contract this module consumes - deliberately parsed via
//! `serde_json::Value` with tolerant field access rather than importing the
//! fno-agents crate: the mux needs five fields, not the daemon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::proto::{AgentBadge, AnswerablePrompt};

/// One registry row as the sideline consumes it: badge already TTL-derived
/// (the reader knows "now"); the pane-exit fact is joined later, on the core
/// loop, where the live pane set lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryAgent {
    pub name: String,
    pub cwd: String,
    /// Registry status is terminal (exited/permanent-dead).
    pub exited: bool,
    /// In-TTL inside-leg badge; `None` = liveness-only. Never a scraped guess.
    pub badge: Option<AgentBadge>,
    pub reason: Option<String>,
    /// The `mux` ref, when this row is pane-hosted: (session, pane_id).
    pub mux: Option<(String, u64)>,
    /// (x-c929) The answerable-prompt payload from the scrape rung, present only
    /// when this row is `blocked` on a numbered menu the daemon could extract;
    /// `None` for a hook-badged block or a focus-only blocked prompt.
    pub answerable: Option<AnswerablePrompt>,
    /// The `claude attach <id>` target: the claude bg-session jobId (in
    /// `short_id` since v9) that lets a paneless watch-only row be attached into
    /// a mux pane. `None` for a row with no jobId (non-claude, or a claude row
    /// that never recorded one). Present regardless of `mux`, but only the
    /// watch-only (paneless) click path consumes it - a pane-hosted row focuses
    /// its pane instead.
    pub attach_id: Option<String>,
    /// (x-0a2e) True when this row's provenance is claude's daemon roster: a
    /// synthesized foreign session, or a registry row the roster liveness-
    /// upgraded. Renders dim; strictly read-only toward `~/.claude/**`. NOT an
    /// attachability signal - that is `attach_id.is_some() && !exited` (an
    /// external row whose pane died still carries `external: true`).
    pub external: bool,
}

/// One claude-roster worker as the sideline consumes it (three fields, not
/// the supervisor's model): `short_id` is the `claude attach <id>` jobId (the
/// first `-`-segment of the roster `sessionId`, == the roster map key), and
/// `name` is already fallback-resolved (`dispatch.seed.name` else
/// `cc-<short_id>`, the adopted-name convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterWorker {
    pub short_id: String,
    pub name: String,
    pub cwd: String,
}

/// The claude daemon roster path, mirroring fno-agents'
/// `claude_roster::daemon_dir` (`FNO_CLAUDE_DAEMON_DIR` > `$HOME/.claude/daemon`)
/// - the same env override, so tests redirect both crates' readers with one
/// variable. Mirrored, not imported: the crates share no types, the FILE is
/// the contract (same rule as the registry above).
pub fn roster_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_CLAUDE_DAEMON_DIR") {
        return PathBuf::from(v).join("roster.json");
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".claude").join("daemon").join("roster.json")
}

/// Parse the claude daemon roster into the sideline's three-field workers.
/// Tolerant `Value` access ONLY - no typed struct: the `procStart`
/// u64 -> date-string drift once zeroed every typed-parse consumer, and
/// tolerant per-field access means unread fields cannot break the parse. A
/// worker missing `sessionId` is skipped alone (tolerate-alien-row, like a
/// registry row without `name`); a missing `workers` key is an empty roster;
/// a malformed document is `None` (the caller keeps last-good foreign rows).
pub fn parse_roster(raw: &str) -> Option<Vec<RosterWorker>> {
    let doc: serde_json::Value = serde_json::from_str(raw).ok()?;
    let workers = match doc.get("workers") {
        Some(w) => w.as_object()?,
        None => return Some(Vec::new()),
    };
    let mut out = Vec::with_capacity(workers.len());
    for w in workers.values() {
        let Some(session_id) = w
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        // short_id is the roster map key == the `claude attach` jobId. A
        // sessionId that leads with `-` (defensive: upstream drift) would yield
        // an empty first segment and thus a meaningless attach target; skip that
        // worker, same as a missing sessionId.
        let short_id = session_id.split('-').next().unwrap_or(session_id);
        if short_id.is_empty() {
            continue;
        }
        let short_id = short_id.to_string();
        let cwd = w
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let name = w
            .get("dispatch")
            .and_then(|d| d.get("seed"))
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("cc-{short_id}"));
        out.push(RosterWorker {
            short_id,
            name,
            cwd,
        });
    }
    Some(out)
}

/// (x-cd67 US4) The current git branch of `cwd`, for the sideline row subline.
/// Bounded file reads only - NEVER shells `git` (the origin freeze class) and
/// NEVER runs on the core loop (the reader task resolves it off-loop). Any read
/// failure, a plain (non-git) dir, or a malformed HEAD degrades to `None`; the
/// subline then shows the cwd tail alone (AC1-ERR).
///
/// - `<cwd>/.git` a directory -> read `<cwd>/.git/HEAD`.
/// - `<cwd>/.git` a file (a linked worktree) -> follow its `gitdir: <path>`
///   pointer (relative pointers resolve against `cwd`), then read `<gitdir>/HEAD`
///   (AC3-EDGE).
/// - HEAD `ref: refs/heads/<name>` -> `<name>`; a detached 40-hex sha -> its
///   first 8 chars; anything else -> `None`.
pub fn resolve_branch(cwd: &Path) -> Option<String> {
    let dot_git = cwd.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    let git_dir = if meta.is_dir() {
        dot_git
    } else {
        // A worktree `.git` file: `gitdir: <path>` (possibly relative to cwd).
        let contents = std::fs::read_to_string(&dot_git).ok()?;
        let ptr = contents.strip_prefix("gitdir:")?.trim();
        let ptr = Path::new(ptr);
        if ptr.is_absolute() {
            ptr.to_path_buf()
        } else {
            cwd.join(ptr)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        return reference
            .trim()
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    // Detached HEAD: a bare 40-hex sha -> short form. Anything else is malformed.
    (head.len() == 40 && head.chars().all(|c| c.is_ascii_hexdigit())).then(|| head[..8].to_string())
}

/// The registry path, resolved exactly as fno-agents' `AgentsHome::from_env`
/// does (`FNO_AGENTS_HOME` > `$HOME/.fno/agents` > `./.fno/agents`).
pub fn registry_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(v).join("registry.json");
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("agents").join("registry.json")
}

/// Parse the fixed `YYYY-MM-DDThh:mm:ssZ` UTC stamp the registry writes back
/// to epoch seconds (a focused copy of fno-agents' `rfc3339_like_to_secs` -
/// Hinnant days-from-civil; the mux reads the FILE contract, not the crate).
/// Any other shape is `None`, so a malformed stamp ages the badge out (fails
/// closed) rather than pinning a stale `working`.
fn rfc3339_like_to_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let mut val = 0i64;
        for &ch in b.get(lo..hi)? {
            if !ch.is_ascii_digit() {
                return None;
            }
            val = val * 10 + i64::from(ch - b'0');
        }
        Some(val)
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

/// True while an inside-leg report is authoritative at `now_secs` (contract
/// v2 / AC-X2-2): no `ttl_ms` never self-ages; a TTL'd report expires once
/// `received_at + ttl_ms` passes; an unparseable `received_at` is expired.
fn report_is_live(received_at: &str, ttl_ms: Option<u64>, now_secs: u64) -> bool {
    let Some(ttl_ms) = ttl_ms else { return true };
    match rfc3339_like_to_secs(received_at) {
        Some(recv) => now_secs.saturating_sub(recv).saturating_mul(1000) <= ttl_ms,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// fno-truth badge source (x-4a48): the JUNIOR rung under inside_leg + screen_state.
//
// A bg /target worker between turns reads as no-badge (Idle) even while its real
// work (CI, preflight) runs externally. fno holds the truth the harness cannot:
// the `node:<id>` claim liveness + per-session loop_check recency. When neither
// senior source badged a row, `overlay_truth_badges` adds a Working badge iff the
// claim holder is verifiably live AND a loop_check fired for its session within
// the window. Everything else (stale/suspect/waiting claim) stays no-badge, same
// as today. Read-only, fail-quiet, tolerant Value parsing - no fno-agents import.
//
// Liveness mirrors `claims.rs::is_live` (a focused copy, like rfc3339_like_to_secs
// and roster_path above): host must match this host AND the pid's process must
// have started at/before the claim's `acquired_at` (the create-time guard that
// rejects a reused pid). The weaker `kill(pid, 0)` alone would badge a same-host
// reused pid Working within the recency window (codex P3); the create-time check
// closes it. Node claims are GLOBAL, so the claims root honors $FNO_CLAIMS_ROOT
// then $HOME (matching Python `global_claims_root`), NOT the cwd/canonical root.
// ---------------------------------------------------------------------------

/// name -> "loop <age>" reason for rows the fno-truth source badges Working.
pub type TruthBadges = HashMap<String, String>;

/// Claim-live + a fire within this window reads Working; older/absent stays no-badge.
const TRUTH_RECENCY_WINDOW_S: u64 = 1800; // 30 min
/// Bounded tail read of events.jsonl so an 11MB log stays cheap per render tick.
const TRUTH_EVENTS_TAIL_BYTES: u64 = 256 * 1024;

/// The `.fno` state base for events.jsonl, off the same anchor as
/// `registry_path` (`FNO_AGENTS_HOME`'s parent > `$HOME/.fno`).
fn fno_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return Path::new(&v)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".fno"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fno")
}

/// The GLOBAL claims dir, mirroring Python `global_claims_root` + `claims_dir`
/// (`$FNO_CLAIMS_ROOT` > `$HOME`, then `.fno/claims`). node:<id> claims are
/// global (like ~/.fno/graph.json), so this must NOT be the cwd/canonical root.
fn global_claims_dir() -> PathBuf {
    std::env::var_os("FNO_CLAIMS_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fno")
        .join("claims")
}

/// `target-<node-id>-<slug>` -> node id. Loose on purpose: a mis-parse yields a
/// key that does not resolve -> no badge (fail-quiet), never a wrong badge.
fn parse_node_id_from_name(name: &str) -> Option<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"^target-([a-z][a-z0-9]*-[0-9a-f]+)(?:-|$)").unwrap());
    re.captures(name).map(|c| c[1].to_string())
}

/// Percent-encode a claim key to its lockfile stem, matching the Python writer's
/// `urllib.parse.quote(key, safe="")` (bytes not in `[A-Za-z0-9._~-]` -> %XX).
fn encode_claim_key(key: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

/// `gethostname(2)`, matching Python `socket.gethostname()` (mirror of
/// `claims.rs::hostname`). Empty on failure -> never equals a recorded host, so
/// an unreadable hostname fails toward "not live".
fn hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Process create time in epoch ms, or None if the pid is gone/uninspectable
/// (permission denied counts as dead). Focused copy of
/// `claims.rs::process_create_time_ms`.
#[cfg(target_os = "macos")]
fn process_create_time_ms(pid: i32) -> Option<i64> {
    use std::mem;
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if written != size {
        return None;
    }
    Some((info.pbi_start_tvsec as i64) * 1000 + (info.pbi_start_tvusec as i64) / 1000)
}

#[cfg(target_os = "linux")]
fn process_create_time_ms(pid: i32) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    let starttime: i64 = after.split_whitespace().nth(19)?.parse().ok()?;
    static BTIME: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    let btime = (*BTIME.get_or_init(|| {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        stat.lines()
            .find_map(|l| l.strip_prefix("btime ").and_then(|r| r.trim().parse().ok()))
    }))?;
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if tck <= 0 {
        return None;
    }
    Some(btime * 1000 + starttime * 1000 / tck as i64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_create_time_ms(_pid: i32) -> Option<i64> {
    None
}

/// Verifiably-running claim holder (mirror of `claims.rs::is_live`): same host
/// AND the pid's process started at/before `acquired_at` (rejects a reused pid).
fn holder_is_live(host: &str, pid: i32, acquired_at: i64) -> bool {
    if host != hostname() {
        return false;
    }
    matches!(process_create_time_ms(pid), Some(create_ms) if create_ms <= acquired_at)
}

/// The session id of a live `node:<id>` claim, or None (missing / unparseable /
/// not-live / non-target holder). The lockfile is flat YAML scalars; hand-parse
/// the fields we need rather than pull in a YAML dep (tolerant by design).
fn live_claim_session(claims_dir: &Path, node_id: &str) -> Option<String> {
    let path = claims_dir.join(format!(
        "{}.lock",
        encode_claim_key(&format!("node:{node_id}"))
    ));
    let text = std::fs::read_to_string(&path).ok()?;
    let mut pid: Option<i32> = None;
    let mut acquired_at: Option<i64> = None;
    let mut host: Option<&str> = None;
    let mut holder: Option<&str> = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("pid:") {
            pid = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("acquired_at:") {
            acquired_at = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("host:") {
            host = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("holder:") {
            holder = Some(v.trim());
        }
    }
    if !holder_is_live(host?, pid?, acquired_at?) {
        return None;
    }
    holder?
        .strip_prefix("target-session:")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Read the bounded tail of `path` as a lossy UTF-8 string, dropping the partial
/// first line. Missing/unreadable -> None (caller degrades to no badges).
fn read_events_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TRUTH_EVENTS_TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(TRUTH_EVENTS_TAIL_BYTES).read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(nl) = text.find('\n') {
            text = text[nl + 1..].to_string();
        }
    }
    Some(text)
}

/// One tail pass over events.jsonl -> `{session_id: age_seconds}` for the newest
/// loop_check fire per session (mirrors read_prior_fires' tolerant filter).
fn newest_fire_ages(events_path: &Path, now_secs: u64) -> HashMap<String, u64> {
    let Some(text) = read_events_tail(events_path) else {
        return HashMap::new();
    };
    let mut newest: HashMap<String, u64> = HashMap::new(); // sid -> newest epoch secs
    for line in text.lines() {
        if !line.contains("\"loop_check\"") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("loop_check") {
            continue;
        }
        let Some(sid) = val.pointer("/data/session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(secs) = val
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(rfc3339_like_to_secs)
        else {
            continue;
        };
        newest
            .entry(sid.to_string())
            .and_modify(|e| *e = (*e).max(secs))
            .or_insert(secs);
    }
    newest
        .into_iter()
        .map(|(sid, secs)| (sid, now_secs.saturating_sub(secs)))
        .collect()
}

/// Build the fno-truth Working badges for the registry rows in `raw`, reading
/// the default claims dir + events log. Fail-quiet: any read failure yields
/// fewer/no badges, never a panic or a wrong badge.
pub fn build_truth_badges(raw: &str, now_secs: u64) -> TruthBadges {
    build_truth_badges_at(
        raw,
        now_secs,
        &global_claims_dir(),
        &fno_dir().join("events.jsonl"),
    )
}

fn build_truth_badges_at(
    raw: &str,
    now_secs: u64,
    claims_dir: &Path,
    events_path: &Path,
) -> TruthBadges {
    let mut out = TruthBadges::new();
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(raw) else {
        return out;
    };
    let Some(rows) = doc
        .get("agents")
        .or_else(|| doc.get("entries"))
        .and_then(|v| v.as_array())
    else {
        return out;
    };
    let ages = newest_fire_ages(events_path, now_secs);
    for row in rows {
        let Some(name) = row.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(node_id) = parse_node_id_from_name(name) else {
            continue;
        };
        let Some(sid) = live_claim_session(claims_dir, &node_id) else {
            continue;
        };
        if let Some(&age) = ages.get(&sid) {
            if age <= TRUTH_RECENCY_WINDOW_S {
                out.insert(name.to_string(), format!("loop {} ago", humanize_age(age)));
            }
        }
    }
    out
}

fn humanize_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

/// Overlay fno-truth Working badges onto rows no senior source badged (strictly
/// junior: only touches `badge.is_none()` non-exited rows, so a hook/scrape badge
/// always wins - AC6). Applied on the sideline render path only, never the
/// idle-authority guard (which wants raw idle).
pub fn overlay_truth_badges(rows: &mut [RegistryAgent], truth: &TruthBadges) {
    for row in rows.iter_mut() {
        if row.badge.is_none() && !row.exited {
            if let Some(reason) = truth.get(&row.name) {
                row.badge = Some(AgentBadge::Working);
                row.reason = Some(reason.clone());
            }
        }
    }
}

/// Derive the sideline row set from raw registry JSON at `now_secs`. Pure so
/// the whole lattice derivation is unit-testable without a file or a clock.
/// A malformed document yields `None` (the caller keeps its last-good rows -
/// a torn concurrent write must not blank the sideline).
pub fn derive_rows(raw: &str, now_secs: u64) -> Option<Vec<RegistryAgent>> {
    let doc: serde_json::Value = serde_json::from_str(raw).ok()?;
    let rows = doc
        .get("agents")
        .or_else(|| doc.get("entries"))?
        .as_array()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(name) = row.get("name").and_then(|v| v.as_str()) else {
            continue; // tolerate an alien row; the registry owners validate
        };
        let cwd = row.get("cwd").and_then(|v| v.as_str()).unwrap_or_default();
        let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let exited = matches!(status, "exited" | "permanent-dead" | "permanent_dead");
        let mux = row.get("mux").and_then(|m| {
            Some((
                m.get("session")?.as_str()?.to_string(),
                m.get("pane_id")?.as_u64()?,
            ))
        });
        // The claude bg jobId, when present, is the `claude attach <id>` target
        // for a paneless row. Since v9 it lives in `short_id` (the unified
        // transport key), so this must be claude-scoped: a codex/gemini row's
        // short_id is a daemon socket key, not a claude attach target. A legacy
        // `claude_short_id` row is tolerated as a fallback (raw read, no backfill).
        let is_claude = row.get("provider").and_then(|v| v.as_str()) == Some("claude");
        let attach_id = is_claude
            .then(|| {
                row.get("short_id")
                    .or_else(|| row.get("claude_short_id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .flatten()
            .map(str::to_string);
        let (badge, reason, answerable) = match row.get("inside_leg") {
            Some(leg) if !leg.is_null() => {
                let live = report_is_live(
                    leg.get("received_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    leg.get("ttl_ms").and_then(|v| v.as_u64()),
                    now_secs,
                );
                if live {
                    let badge = match leg.get("state").and_then(|v| v.as_str()) {
                        Some("working") => Some(AgentBadge::Working),
                        Some("blocked") => Some(AgentBadge::Blocked),
                        Some("done") => Some(AgentBadge::Done),
                        _ => None,
                    };
                    let reason = leg
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    // Hook-badged blocks carry no answer payload in v1 (the
                    // grammar is a scrape-rung concern); focus-only.
                    (badge, reason, None)
                } else {
                    (None, None, None) // TTL lapsed -> liveness-only (AC2-ERR)
                }
            }
            // Screen-manifest rung (v7): consulted ONLY when no inside_leg
            // report exists at all - a hook-capable row (even TTL-lapsed)
            // never falls through to a scrape verdict (per-capability
            // arbitration; the writer clears screen_state on the flip, this
            // is the reader-side defense in depth).
            _ => match row.get("screen_state") {
                Some(ss) if !ss.is_null() => {
                    let live = report_is_live(
                        ss.get("at").and_then(|v| v.as_str()).unwrap_or(""),
                        ss.get("ttl_ms").and_then(|v| v.as_u64()),
                        now_secs,
                    );
                    if live {
                        // Manifest vocabulary is working|idle|blocked. `idle`
                        // maps to no badge (a plain live row): AgentBadge has
                        // no Idle variant and adding one is a proto bump,
                        // serialized behind the v7 wire work. Anything
                        // malformed fails badge-closed.
                        let badge = match ss.get("state").and_then(|v| v.as_str()) {
                            Some("working") => Some(AgentBadge::Working),
                            Some("blocked") => Some(AgentBadge::Blocked),
                            _ => None,
                        };
                        // The matched rule doubles as the human hint, the way
                        // an inside-leg reason does.
                        let reason = badge
                            .is_some()
                            .then(|| ss.get("rule").and_then(|v| v.as_str()).map(str::to_string))
                            .flatten();
                        // Answer payload only for a live `blocked` scrape verdict
                        // that carried one; a malformed payload degrades to
                        // focus-only (extraction is additive - never blanks the
                        // badge). from_value tolerates the missing/extra field.
                        let answerable = if badge == Some(AgentBadge::Blocked) {
                            ss.get("answerable").filter(|v| !v.is_null()).and_then(|v| {
                                serde_json::from_value::<AnswerablePrompt>(v.clone()).ok()
                            })
                        } else {
                            None
                        };
                        (badge, reason, answerable)
                    } else {
                        (None, None, None) // TTL lapsed -> liveness-only
                    }
                }
                _ => (None, None, None),
            },
        };
        out.push(RegistryAgent {
            name: name.to_string(),
            cwd: cwd.to_string(),
            exited,
            badge,
            reason,
            mux,
            answerable,
            attach_id,
            external: false,
        });
    }
    // Stable order so row-set equality (the change gate) and the rendered
    // sideline are deterministic across ticks.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

/// Union the fno registry rows with claude's roster (x-0a2e). Pure so the
/// whole merge is unit-testable without files or a clock, exactly like
/// `derive_rows`. The join key is the registry row's `attach_id` (== its claude
/// jobId in `short_id`) against the roster worker's `short_id`; a registry row
/// always wins a collision (dedup registry-wins, Locked 4).
///
/// 1. Registry rows pass through.
/// 2. Liveness upgrade: an `exited` registry row whose short_id the roster
///    still lists is un-exited + `external` (attach revives a suspended
///    session, so roster presence == attachable; Locked 5). The pane-dead
///    fact stays senior in `agent_rows()`, which re-derives `exited`.
/// 3. Foreign rows: every roster worker matching no registry short_id becomes
///    a synthesized external row (paneless, attachable via `attach_id`).
/// 4. Sort by name (the determinism rule the change gate and layouts need).
pub fn merge_rows(reg_rows: Vec<RegistryAgent>, roster: &[RosterWorker]) -> Vec<RegistryAgent> {
    use std::collections::HashSet;
    let roster_ids: HashSet<&str> = roster.iter().map(|w| w.short_id.as_str()).collect();

    // Upgrade in place, then dedup the roster against the (borrowed) registry
    // ids - no per-tick String clones of the short ids (gemini review).
    let mut out = reg_rows;
    for r in &mut out {
        if r.exited {
            if let Some(id) = r.attach_id.as_deref() {
                if roster_ids.contains(id) {
                    r.exited = false;
                    r.external = true;
                    // Drop any stale inside-leg/scrape verdict that a terminal
                    // row happened to still carry: an upgraded row renders as a
                    // plain live external row (plan merge step 2), matching the
                    // synthesized-foreign shape below.
                    r.badge = None;
                    r.reason = None;
                    r.answerable = None;
                }
            }
        }
    }

    let reg_ids: HashSet<&str> = out.iter().filter_map(|r| r.attach_id.as_deref()).collect();
    let mut foreign = Vec::new();
    for w in roster {
        if reg_ids.contains(w.short_id.as_str()) {
            continue; // adopted / already owned by a registry row
        }
        foreign.push(RegistryAgent {
            name: w.name.clone(),
            cwd: w.cwd.clone(),
            exited: false,
            badge: None,
            reason: None,
            mux: None,
            answerable: None,
            attach_id: Some(w.short_id.clone()),
            external: true,
        });
    }
    drop(reg_ids); // release the borrow of `out` before extending it
    out.extend(foreign);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One tracked external session's observed liveness from `claude agents --json
/// --all` (x-7561). `Live`/`Terminal` are the mapped states; `Unknown` is a
/// tracked id PRESENT in the catalog but with an unrecognized/missing state (a
/// per-id schema drift) - distinct from absence-from-the-map (the row is gone)
/// and from a `None` map (the whole query failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedExternal {
    Live,
    Terminal,
    Unknown,
}

/// Parse `claude agents --json --all` into a tracked-id -> liveness map, keeping
/// ONLY ids in `tracked` (Domain Pitfall 4: never flood the sideline with
/// historical sessions). `working|blocked` -> Live; `stopped|done|failed` ->
/// Terminal; any other state is dropped (treated as absent). Returns `None` on
/// unparseable or schema-drifted output (not a JSON array of objects) so the
/// caller retains rows as `unknown` rather than deleting a tracked id it could
/// not observe (AC1-FR "query unavailable").
pub fn parse_claude_agents(
    raw: &str,
    tracked: &std::collections::HashSet<String>,
) -> Option<HashMap<String, ObservedExternal>> {
    // Deserialize straight into a vec of objects: a non-array body or a
    // non-object element fails the parse -> `None` (schema drift), the same
    // fail-closed behavior as a manual `as_array`/`as_object` walk but without
    // the intermediate `Value` tree or the array clone.
    let arr: Vec<serde_json::Map<String, serde_json::Value>> =
        serde_json::from_str(raw.trim()).ok()?;
    let mut out = HashMap::new();
    for obj in arr {
        let Some(id) = obj.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !tracked.contains(id) {
            continue;
        }
        let observed = match obj.get("state").and_then(|v| v.as_str()) {
            Some("working") | Some("blocked") => ObservedExternal::Live,
            Some("stopped") | Some("done") | Some("failed") => ObservedExternal::Terminal,
            // A new/malformed/missing state for a PRESENT tracked id is per-id
            // schema drift, NOT absence (codex P2): keep it in the map as
            // `Unknown` so reconcile holds the record rather than deleting it as
            // authoritatively gone.
            _ => ObservedExternal::Unknown,
        };
        out.insert(id.to_string(), observed);
    }
    Some(out)
}

/// Reconcile persisted external-lifecycle records against the observed catalog
/// (x-7561, the AC1-FR/AC3-FR table). `observed` is `None` when the `claude
/// agents` query failed or schema-drifted: every non-`unknown` record is
/// retained as `unknown` (permitting only a safe stop retry), never deleted.
/// Otherwise, per record by `(persisted state, observed)`:
///
/// - absent (not in map): deleted as removed;
/// - live: `stopping`/`failed`/`unknown` -> `failed` (stop retry); `stopped` ->
///   deleted (roster owns the live row again); `removing` -> `unknown` anomaly;
/// - terminal: any -> `stopped`, except `removing` -> `stopped` (rm retry).
///
/// Pure so the whole table is unit-testable without a subprocess or the store.
/// Returns the new record set plus bounded per-record notices for transitions
/// that need operator attention.
pub fn reconcile_external(
    persisted: Vec<crate::squad_store::ExternalLifecycle>,
    observed: Option<&HashMap<String, ObservedExternal>>,
) -> (Vec<crate::squad_store::ExternalLifecycle>, Vec<String>) {
    use crate::squad_store::ExternalState as S;
    let mut out = Vec::new();
    let mut notices = Vec::new();
    for mut r in persisted {
        let Some(map) = observed else {
            // Query unavailable: hold every row as unknown (safe stop retry).
            if r.state != S::Unknown {
                r.state = S::Unknown;
                notices.push(format!("{}: external state unknown - retry stop", r.name));
            }
            out.push(r);
            continue;
        };
        match map.get(&r.attach_id) {
            None => {
                // Authoritative absence: the session is gone -> removed. An
                // in-flight action resolving to gone is worth one notice.
                if matches!(r.state, S::Stopping | S::Removing) {
                    notices.push(format!("{}: removed", r.name));
                }
            }
            Some(ObservedExternal::Live) => match r.state {
                S::Stopped => { /* clear tombstone: the live roster owns the row */ }
                S::Removing => {
                    r.state = S::Unknown;
                    notices.push(format!("{}: external state unknown - retry stop", r.name));
                    out.push(r);
                }
                _ => {
                    r.state = S::Failed;
                    notices.push(format!("{}: stop failed - retry", r.name));
                    out.push(r);
                }
            },
            Some(ObservedExternal::Terminal) => {
                let was_action = matches!(r.state, S::Stopping | S::Removing);
                if r.state == S::Removing {
                    notices.push(format!("{}: remove interrupted - retry", r.name));
                } else if was_action {
                    notices.push(format!("{}: stopped", r.name));
                }
                r.state = S::Stopped;
                r.reason = None;
                out.push(r);
            }
            Some(ObservedExternal::Unknown) => {
                // Present but indeterminate (per-id schema drift): hold the
                // record as unknown, never delete it as absent (codex P2). Same
                // safe-stop-retry rest state as the whole-query-unavailable case.
                if r.state != S::Unknown {
                    r.state = S::Unknown;
                    notices.push(format!("{}: external state unknown - retry stop", r.name));
                }
                out.push(r);
            }
        }
    }
    (out, notices)
}

/// The reader's between-tick memory. The interval task itself lives in
/// server.rs (it owns the `CoreMsg` sender); this holds the mtime-gated
/// document caches (registry + roster) and the last-sent MERGED row set so
/// the derivation stays pure and unit-testable here.
#[derive(Default)]
pub struct ReaderState {
    reg_raw: Option<String>,
    reg_stamp: Option<(std::time::SystemTime, u64)>,
    roster_raw: Option<String>,
    roster_stamp: Option<(std::time::SystemTime, u64)>,
    /// Last successfully-derived rows per source, so a torn concurrent write
    /// keeps that source's last-good instead of blanking it (the merged
    /// `last_sent` alone can't distinguish which source went stale).
    last_good_reg: Option<Vec<RegistryAgent>>,
    last_good_roster: Option<Vec<RosterWorker>>,
    last_sent: Option<Vec<RegistryAgent>>,
}

impl ReaderState {
    /// The stamp of the currently-cached registry document (the reader's
    /// mtime+len gate for the registry read).
    pub fn reg_stamp(&self) -> Option<(std::time::SystemTime, u64)> {
        self.reg_stamp
    }

    /// The stamp of the currently-cached roster document (the reader's
    /// mtime+len gate for the roster read).
    pub fn roster_stamp(&self) -> Option<(std::time::SystemTime, u64)> {
        self.roster_stamp
    }

    /// One tick: fold fresh stats/reads of BOTH files (taken OFF the core loop
    /// by the caller, each behind its own mtime+len gate) and return the
    /// merged row set to publish, or `None` when the merged set is unchanged.
    /// TTL aging re-derives from the cached registry every tick, so a badge
    /// can lapse without a file write. For each source: a torn/garbage
    /// document keeps that source's last-good rows; a vanished file empties
    /// them (the two cases are distinct, AC2-FR).
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        reg_stamp: Option<(std::time::SystemTime, u64)>,
        reg_read: impl FnOnce() -> Option<String>,
        roster_stamp: Option<(std::time::SystemTime, u64)>,
        roster_read: impl FnOnce() -> Option<String>,
        now_secs: u64,
    ) -> Option<Vec<RegistryAgent>> {
        // Advance the cached stamp ONLY when the read resolves (fresh bytes, or
        // a confirmed vanish). A changed stamp whose read came back empty is a
        // raced/failed read: leave the stamp behind so the next tick's scan gate
        // (stamp != cached) re-attempts the SAME stamp instead of freezing the
        // last-good rows until an unrelated later write happens to move mtime.
        if reg_stamp != self.reg_stamp {
            match (reg_read(), reg_stamp) {
                (Some(raw), _) => {
                    self.reg_raw = Some(raw);
                    self.reg_stamp = reg_stamp;
                }
                (None, None) => {
                    self.reg_raw = None; // vanished
                    self.reg_stamp = None;
                }
                (None, Some(_)) => {} // raced/failed read: keep last-good AND retry next tick
            }
        }
        if roster_stamp != self.roster_stamp {
            match (roster_read(), roster_stamp) {
                (Some(raw), _) => {
                    self.roster_raw = Some(raw);
                    self.roster_stamp = roster_stamp;
                }
                (None, None) => {
                    self.roster_raw = None;
                    self.roster_stamp = None;
                }
                (None, Some(_)) => {}
            }
        }

        let mut reg_rows = match &self.reg_raw {
            Some(raw) => derive_rows(raw, now_secs)
                .or_else(|| self.last_good_reg.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        // fno-truth junior badge (x-4a48): fill the no-badge/Idle gap for a
        // bg /target worker between turns from its claim + loop_check recency.
        if let Some(raw) = &self.reg_raw {
            overlay_truth_badges(&mut reg_rows, &build_truth_badges(raw, now_secs));
        }
        self.last_good_reg = Some(reg_rows.clone());

        let roster = match &self.roster_raw {
            Some(raw) => parse_roster(raw)
                .or_else(|| self.last_good_roster.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        self.last_good_roster = Some(roster.clone());

        let rows = merge_rows(reg_rows, &roster);
        if self.last_sent.as_ref() != Some(&rows) {
            self.last_sent = Some(rows.clone());
            Some(rows)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(rows: &str) -> String {
        format!(r#"{{"schema_version": 6, "agents": [{rows}]}}"#)
    }

    const NOW: u64 = 1_800_000_000; // 2027-01-15T08:00:00Z-ish

    #[test]
    fn agent_rows_badge_lattice_derives_from_registry() {
        // In-TTL report -> badge; lapsed TTL -> liveness-only (AC2-ERR);
        // terminal status -> exited; mux ref carried through.
        let raw = reg(&format!(
            r#"{{"name":"badged","cwd":"/w","status":"live",
                 "mux":{{"session":"main","pane_id":7}},
                 "inside_leg":{{"state":"blocked","seq":3,"reason":"perm prompt",
                                "received_at":"{recent}","ttl_ms":60000}}}},
               {{"name":"lapsed","cwd":"/w","status":"live",
                 "inside_leg":{{"state":"working","seq":9,
                                "received_at":"2020-01-01T00:00:00Z","ttl_ms":60000}}}},
               {{"name":"gone","cwd":"/x","status":"exited"}}"#,
            recent = "2027-01-15T07:59:30Z",
        ));
        // NOW after the recent stamp but inside its 60s TTL.
        let now = rfc3339_like_to_secs("2027-01-15T08:00:00Z").unwrap();
        let rows = derive_rows(&raw, now).unwrap();
        assert_eq!(rows.len(), 3);
        let badged = rows.iter().find(|r| r.name == "badged").unwrap();
        assert_eq!(badged.badge, Some(AgentBadge::Blocked));
        assert_eq!(badged.reason.as_deref(), Some("perm prompt"));
        assert_eq!(badged.mux, Some(("main".into(), 7)));
        assert!(!badged.exited);
        let lapsed = rows.iter().find(|r| r.name == "lapsed").unwrap();
        assert_eq!(lapsed.badge, None, "TTL lapse ages to liveness-only");
        let gone = rows.iter().find(|r| r.name == "gone").unwrap();
        assert!(gone.exited);
    }

    #[test]
    fn agent_rows_no_ttl_report_never_self_ages() {
        let raw = reg(r#"{"name":"pinless","cwd":"/w","status":"live",
                "inside_leg":{"state":"done","seq":1,
                              "received_at":"2020-01-01T00:00:00Z"}}"#);
        let rows = derive_rows(&raw, NOW).unwrap();
        assert_eq!(rows[0].badge, Some(AgentBadge::Done));
    }

    #[test]
    fn claude_short_id_becomes_the_attach_target() {
        // A claude bg row's jobId (`claude_short_id`) is the `claude attach <id>`
        // target; a row without one (or with an empty one) is not attachable.
        let raw = reg(
            r#"{"name":"bg","cwd":"/w","status":"live","provider":"claude","short_id":"c19cd2c3"},
               {"name":"plain","cwd":"/w","status":"live"}"#,
        );
        let rows = derive_rows(&raw, NOW).unwrap();
        let bg = rows.iter().find(|r| r.name == "bg").unwrap();
        assert_eq!(bg.attach_id.as_deref(), Some("c19cd2c3"));
        let plain = rows.iter().find(|r| r.name == "plain").unwrap();
        assert_eq!(plain.attach_id, None, "no jobId -> not attachable");
    }

    #[test]
    fn agent_rows_malformed_doc_is_none_and_alien_rows_skip() {
        assert_eq!(derive_rows("not json", NOW), None);
        assert_eq!(derive_rows(r#"{"agents": 3}"#, NOW), None);
        // A row without a name is skipped, not fatal.
        let rows = derive_rows(
            &reg(r#"{"cwd":"/w"}, {"name":"ok","cwd":"/w","status":"live"}"#),
            NOW,
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ok");
    }

    #[test]
    fn agent_rows_screen_state_rung_badges_only_hookless_rows() {
        // The screen-manifest rung (v7): a hook-less row with a fresh scrape
        // verdict badges (blocked/working); `idle` renders as a plain live
        // row (no AgentBadge::Idle until the next proto bump); a lapsed or
        // malformed verdict ages to liveness-only; and ANY inside_leg report
        // - even TTL-lapsed - keeps a leftover verdict from badging (the hook
        // is unconditionally senior).
        let raw = reg(&format!(
            r#"{{"name":"scraped-blocked","cwd":"/w","status":"live",
                 "screen_state":{{"state":"blocked","rule":"permission_prompt",
                                  "seq":2,"at":"{recent}","ttl_ms":120000}}}},
               {{"name":"scraped-idle","cwd":"/w","status":"live",
                 "screen_state":{{"state":"idle","rule":"idle_prompt",
                                  "seq":1,"at":"{recent}","ttl_ms":120000}}}},
               {{"name":"scraped-lapsed","cwd":"/w","status":"live",
                 "screen_state":{{"state":"working","rule":"busy","seq":1,
                                  "at":"2020-01-01T00:00:00Z","ttl_ms":120000}}}},
               {{"name":"scraped-corrupt","cwd":"/w","status":"live",
                 "screen_state":{{"state":"working","rule":"busy","seq":1,
                                  "at":"garbage","ttl_ms":120000}}}},
               {{"name":"hook-wins","cwd":"/w","status":"live",
                 "inside_leg":{{"state":"working","seq":9,
                                "received_at":"2020-01-01T00:00:00Z","ttl_ms":60000}},
                 "screen_state":{{"state":"blocked","rule":"leftover","seq":1,
                                  "at":"{recent}","ttl_ms":120000}}}}"#,
            recent = "2027-01-15T07:59:30Z",
        ));
        let now = rfc3339_like_to_secs("2027-01-15T08:00:00Z").unwrap();
        let rows = derive_rows(&raw, now).unwrap();
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        let blocked = get("scraped-blocked");
        assert_eq!(blocked.badge, Some(AgentBadge::Blocked));
        assert_eq!(blocked.reason.as_deref(), Some("permission_prompt"));
        assert_eq!(get("scraped-idle").badge, None, "idle = plain live row");
        assert_eq!(get("scraped-lapsed").badge, None, "TTL lapse ages out");
        assert_eq!(get("scraped-corrupt").badge, None, "corrupt stamp closed");
        assert_eq!(
            get("hook-wins").badge,
            None,
            "a hook-capable row (even lapsed) never badges from a scrape verdict"
        );
    }

    #[test]
    fn agent_rows_malformed_stamp_fails_badge_closed() {
        let raw = reg(r#"{"name":"bad-stamp","cwd":"/w","status":"live",
                "inside_leg":{"state":"working","seq":1,
                              "received_at":"garbage","ttl_ms":60000}}"#);
        let rows = derive_rows(&raw, NOW).unwrap();
        assert_eq!(rows[0].badge, None, "corrupt stamp must not pin a badge");
    }

    #[test]
    fn agent_rows_are_name_sorted_for_deterministic_layouts() {
        let raw = reg(r#"{"name":"zeta","cwd":"/w","status":"live"},
               {"name":"alpha","cwd":"/w","status":"live"}"#);
        let rows = derive_rows(&raw, NOW).unwrap();
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[1].name, "zeta");
    }

    // The roster parser (x-0a2e task 1.1): tolerant Value access over
    // claude's roster.json. Cites the "torn roster" and "missing sessionId"
    // Failure Modes bullets.
    #[test]
    fn parse_roster_live_shape_yields_three_field_workers() {
        let raw = r#"{"workers":{
            "ab12cd34":{"sessionId":"ab12cd34-9f00-4a2b-8888-000000000001",
                        "cwd":"/w","procStart":1751000000,
                        "dispatch":{"source":"shell","seed":{"name":"think-x-9999"}}},
            "ef56ab78":{"sessionId":"ef56ab78-1111-4a2b-8888-000000000002",
                        "cwd":"/x","dispatch":{"source":"fleet","seed":{}}}}}"#;
        let mut workers = parse_roster(raw).unwrap();
        workers.sort_by(|a, b| a.short_id.cmp(&b.short_id));
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0].short_id, "ab12cd34");
        assert_eq!(workers[0].name, "think-x-9999");
        assert_eq!(workers[0].cwd, "/w");
        // Missing seed.name falls back to the adopted-name convention (AC4-EDGE).
        assert_eq!(workers[1].name, "cc-ef56ab78");
    }

    #[test]
    fn parse_roster_tolerates_field_drift_and_alien_workers() {
        // procStart as a date STRING (the drift that once zeroed typed
        // parsers) must not fail the parse; a worker without sessionId is
        // skipped alone, never the document.
        let raw = r#"{"workers":{
            "ab12cd34":{"sessionId":"ab12cd34-1","cwd":"/w",
                        "procStart":"2026-07-06T10:00:00Z"},
            "orphan":{"cwd":"/x"},
            "empty":{"sessionId":"","cwd":"/y"},
            "dashlead":{"sessionId":"-nope","cwd":"/z"}}}"#;
        let workers = parse_roster(raw).unwrap();
        assert_eq!(
            workers.len(),
            1,
            "orphan, empty, and dash-leading ids all skip"
        );
        assert_eq!(workers[0].short_id, "ab12cd34");
    }

    #[test]
    fn parse_roster_garbage_is_none_and_missing_workers_is_empty() {
        // Garbage doc -> None (caller keeps last-good, AC1-ERR); a document
        // without a workers key (or with an empty map) is a VALID empty
        // roster, not a parse failure.
        assert_eq!(parse_roster("not json"), None);
        assert_eq!(parse_roster(r#"{"workers": 3}"#), None);
        assert_eq!(parse_roster("{}"), Some(Vec::new()));
        assert_eq!(parse_roster(r#"{"workers":{}}"#), Some(Vec::new()));
    }

    // ---- Union merge + dual-doc ReaderState (x-0a2e task 1.2) ----

    fn worker(short: &str, name: &str, cwd: &str) -> RosterWorker {
        RosterWorker {
            short_id: short.into(),
            name: name.into(),
            cwd: cwd.into(),
        }
    }

    #[test]
    fn merge_appends_foreign_rows_and_sorts(/* AC1-HP */) {
        let reg = derive_rows(
            &reg(r#"{"name":"mmm","cwd":"/w","status":"live","provider":"claude","short_id":"aa11bb22"}"#),
            NOW,
        )
        .unwrap();
        let roster = vec![
            worker("cc33dd44", "think-x-9999", "/w"),
            worker("aa11bb22", "already-owned", "/w"), // dedup: registry wins
        ];
        let rows = merge_rows(reg, &roster);
        // Two rows: the registry row + one foreign; the roster twin of the
        // owned session is suppressed (AC1-EDGE dedup).
        assert_eq!(rows.len(), 2);
        // Name-sorted: "mmm" < "think-x-9999".
        assert_eq!(rows[0].name, "mmm");
        assert!(!rows[0].external, "owned registry row is not external");
        let foreign = &rows[1];
        assert_eq!(foreign.name, "think-x-9999");
        assert!(foreign.external);
        assert_eq!(foreign.attach_id.as_deref(), Some("cc33dd44"));
        assert!(!foreign.exited);
        assert_eq!(foreign.mux, None);
    }

    #[test]
    fn merge_upgrades_exited_registry_row_present_in_roster(/* AC3-HP */) {
        let reg = derive_rows(
            &reg(r#"{"name":"stale","cwd":"/w","status":"exited","provider":"claude","short_id":"ab12cd34"}"#),
            NOW,
        )
        .unwrap();
        assert!(reg[0].exited, "derive keeps it exited");
        let rows = merge_rows(reg, &[worker("ab12cd34", "n", "/w")]);
        assert_eq!(
            rows.len(),
            1,
            "no duplicate foreign row for the upgraded id"
        );
        assert!(!rows[0].exited, "roster presence un-exits it");
        assert!(rows[0].external);
        assert_eq!(rows[0].name, "stale", "keeps its registry name");
        assert_eq!(rows[0].attach_id.as_deref(), Some("ab12cd34"));
    }

    #[test]
    fn merge_empty_roster_is_byte_equal_to_registry_only(/* AC3-EDGE */) {
        let raw = reg(
            r#"{"name":"z","cwd":"/w","status":"live","provider":"claude","short_id":"aa11bb22"},
                        {"name":"a","cwd":"/x","status":"exited"}"#,
        );
        let reg = derive_rows(&raw, NOW).unwrap();
        let merged = merge_rows(reg.clone(), &[]);
        assert_eq!(
            merged, reg,
            "no roster => registry-only derivation, verbatim"
        );
        assert!(merged.iter().all(|r| !r.external));
    }

    // ReaderState: two mtime-gated docs, merged change gate, per-source
    // last-good on a torn write vs empty on a vanished file (AC1-ERR, AC2-FR).
    fn stamp(n: u64) -> Option<(std::time::SystemTime, u64)> {
        Some((std::time::UNIX_EPOCH + std::time::Duration::from_secs(n), n))
    }

    #[test]
    fn reader_publishes_union_on_first_tick_then_idles() {
        let mut st = ReaderState::default();
        let reg = reg(r#"{"name":"r","cwd":"/w","status":"live"}"#);
        let roster = r#"{"workers":{"ab12cd34":{"sessionId":"ab12cd34-1","cwd":"/w",
                        "dispatch":{"seed":{"name":"foreign"}}}}}"#
            .to_string();
        let rows = st
            .tick(
                stamp(1),
                || Some(reg.clone()),
                stamp(1),
                || Some(roster.clone()),
                NOW,
            )
            .expect("first tick publishes");
        assert_eq!(rows.len(), 2);
        // Idle tick: same stamps, nothing read, merged set unchanged.
        assert!(st.tick(stamp(1), || None, stamp(1), || None, NOW).is_none());
    }

    #[test]
    fn reader_torn_roster_keeps_last_good_vanished_empties(/* AC2-FR */) {
        let mut st = ReaderState::default();
        let reg = reg(r#"{"name":"r","cwd":"/w","status":"live"}"#);
        let roster =
            r#"{"workers":{"ab12cd34":{"sessionId":"ab12cd34-1","cwd":"/w"}}}"#.to_string();
        let first = st
            .tick(
                stamp(1),
                || Some(reg.clone()),
                stamp(1),
                || Some(roster),
                NOW,
            )
            .unwrap();
        assert_eq!(first.len(), 2, "registry row + foreign");
        // Torn roster (new stamp, garbage bytes): foreign row persists.
        let torn = st.tick(stamp(1), || None, stamp(2), || Some("garbage".into()), NOW);
        assert!(
            torn.is_none(),
            "last-good keeps merged set identical => no publish"
        );
        // Vanished roster (stamp -> None): foreign row disappears, registry stays.
        let gone = st
            .tick(stamp(1), || None, None, || None, NOW)
            .expect("vanish changes the merged set");
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].name, "r");
        assert!(!gone[0].external);
    }

    #[test]
    fn reader_retries_same_stamp_after_a_raced_read() {
        // A changed stamp whose read came back empty (raced/failed) must NOT
        // advance the cached stamp: the next tick at the SAME stamp with a
        // successful read has to publish the fresh content, not stay frozen.
        let mut st = ReaderState::default();
        st.tick(
            stamp(1),
            || Some(reg(r#"{"name":"a","cwd":"/w","status":"live"}"#)),
            None,
            || None,
            NOW,
        );
        let raced = st.tick(stamp(2), || None, None, || None, NOW);
        assert!(
            raced.is_none(),
            "a raced read keeps last-good => no publish"
        );
        let healed = st
            .tick(
                stamp(2),
                || Some(reg(r#"{"name":"b","cwd":"/w","status":"live"}"#)),
                None,
                || None,
                NOW,
            )
            .expect("the same stamp retries and publishes once the read succeeds");
        assert_eq!(healed.len(), 1);
        assert_eq!(
            healed[0].name, "b",
            "stamp did not freeze on the failed read"
        );
    }

    #[test]
    fn merge_upgrade_clears_a_stale_badge() {
        // An exited registry row can still carry a live inside-leg badge; the
        // liveness upgrade drops it so the row renders as a plain external row
        // (plan merge step 2), never a badged one.
        let reg = derive_rows(
            &reg(
                r#"{"name":"x","cwd":"/w","status":"exited","provider":"claude","short_id":"ab12cd34",
                     "inside_leg":{"state":"working","received_at":"2020-01-01T00:00:00Z"}}"#,
            ),
            NOW,
        )
        .unwrap();
        assert!(
            reg[0].exited && reg[0].badge.is_some(),
            "precondition: exited + badged"
        );
        let rows = merge_rows(reg, &[worker("ab12cd34", "n", "/w")]);
        assert!(rows[0].external && !rows[0].exited);
        assert_eq!(rows[0].badge, None, "upgrade drops the stale badge");
        assert_eq!(rows[0].reason, None);
    }

    #[test]
    fn reader_roster_only_change_republishes() {
        let mut st = ReaderState::default();
        let reg = reg(r#"{"name":"r","cwd":"/w","status":"live"}"#);
        st.tick(stamp(1), || Some(reg), stamp(1), || Some("{}".into()), NOW);
        // A worker appears in the roster only; registry untouched.
        let roster =
            r#"{"workers":{"ab12cd34":{"sessionId":"ab12cd34-1","cwd":"/w"}}}"#.to_string();
        let rows = st
            .tick(stamp(1), || None, stamp(2), || Some(roster), NOW)
            .expect("roster-only change publishes");
        assert_eq!(rows.len(), 2);
    }

    // x-c929: a live `blocked` scrape verdict with an `answerable` payload parses
    // it onto the row; a blocked verdict without one, and a hook-badged block,
    // are both focus-only (no answer payload in v1).
    #[test]
    fn agent_rows_carry_answerable_payload_on_blocked_scrape() {
        let fp = ["7"; 32].join(",");
        let raw = reg(&format!(
            r#"{{"name":"answerable","cwd":"/w","status":"live",
                 "screen_state":{{"state":"blocked","rule":"permission_prompt",
                   "seq":2,"at":"{recent}","ttl_ms":120000,
                   "answerable":{{"prompt":"Do you want to proceed?",
                     "options":[{{"idx":"1","label":"Yes","keystroke":[49]}},
                                {{"idx":"2","label":"No","keystroke":[50]}}],
                     "fingerprint":[{fp}],"region_lines":8}}}}}},
               {{"name":"focus-only","cwd":"/w","status":"live",
                 "screen_state":{{"state":"blocked","rule":"live_blocked_form",
                   "seq":1,"at":"{recent}","ttl_ms":120000}}}},
               {{"name":"hook-blocked","cwd":"/w","status":"live",
                 "inside_leg":{{"state":"blocked","seq":3,
                   "received_at":"{recent}","ttl_ms":60000}}}}"#,
            recent = "2027-01-15T07:59:30Z",
        ));
        let now = rfc3339_like_to_secs("2027-01-15T08:00:00Z").unwrap();
        let rows = derive_rows(&raw, now).unwrap();
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        let a = get("answerable");
        assert_eq!(a.badge, Some(AgentBadge::Blocked));
        let ans = a
            .answerable
            .as_ref()
            .expect("answerable parsed onto the row");
        assert_eq!(ans.options.len(), 2);
        assert_eq!(ans.options[0].keystroke, b"1");
        assert_eq!(ans.region_lines, 8);
        // Blocked but no payload -> focus-only.
        assert_eq!(get("focus-only").badge, Some(AgentBadge::Blocked));
        assert!(get("focus-only").answerable.is_none());
        // A hook-badged block carries no answer payload in v1.
        assert_eq!(get("hook-blocked").badge, Some(AgentBadge::Blocked));
        assert!(get("hook-blocked").answerable.is_none());
    }

    // -------------------------------------------------------------------
    // fno-truth junior badge (x-4a48)
    // -------------------------------------------------------------------
    fn plain_row(name: &str, badge: Option<AgentBadge>, exited: bool) -> RegistryAgent {
        RegistryAgent {
            name: name.into(),
            cwd: "/w".into(),
            exited,
            badge,
            reason: None,
            mux: None,
            answerable: None,
            attach_id: None,
            external: false,
        }
    }

    #[test]
    fn parse_node_id_from_worker_name() {
        assert_eq!(
            parse_node_id_from_name("target-x-4a48-fleet-status").as_deref(),
            Some("x-4a48")
        );
        assert_eq!(
            parse_node_id_from_name("target-ab-1a2b3c4d-slug").as_deref(),
            Some("ab-1a2b3c4d")
        );
        assert_eq!(
            parse_node_id_from_name("target-x-4a48").as_deref(),
            Some("x-4a48")
        );
        assert_eq!(parse_node_id_from_name("phasestall"), None);
        assert_eq!(parse_node_id_from_name("worker-x-4a48-foo"), None);
    }

    #[test]
    fn encode_claim_key_matches_python_quote() {
        assert_eq!(encode_claim_key("node:x-4a48"), "node%3Ax-4a48");
    }

    #[test]
    fn overlay_badges_no_badge_row_and_respects_seniority() {
        // AC2-HP: a no-badge row gets Working from truth.
        // AC6-EDGE: a scrape-Blocked row is never overridden (junior).
        let mut rows = vec![
            plain_row("idle-worker", None, false),
            plain_row("scraped", Some(AgentBadge::Blocked), false),
            plain_row("done-worker", None, true), // exited -> never badged
        ];
        let mut truth = TruthBadges::new();
        truth.insert("idle-worker".into(), "loop 2m ago".into());
        truth.insert("scraped".into(), "loop 1m ago".into());
        truth.insert("done-worker".into(), "loop 3m ago".into());
        overlay_truth_badges(&mut rows, &truth);

        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        assert_eq!(get("idle-worker").badge, Some(AgentBadge::Working));
        assert_eq!(get("idle-worker").reason.as_deref(), Some("loop 2m ago"));
        assert_eq!(get("scraped").badge, Some(AgentBadge::Blocked)); // scrape wins
        assert_eq!(get("done-worker").badge, None); // exited stays no-badge
    }

    // ---- build_truth_badges_at over real claim + events files ----
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("fno-truth-{}-{tag}", std::process::id()));
            std::fs::create_dir_all(d.join("claims")).unwrap();
            Tmp(d)
        }
        fn claims(&self) -> PathBuf {
            self.0.join("claims")
        }
        fn events(&self) -> PathBuf {
            self.0.join("events.jsonl")
        }
        // A LIVE claim: this host + acquired_at far in the future so the current
        // pid's create time is <= acquired_at (passes the pid-reuse guard).
        fn write_claim(&self, node_id: &str, pid: i32, sid: &str) {
            self.write_claim_full(node_id, pid, sid, &hostname(), 9_999_999_999_999);
        }
        fn write_claim_full(
            &self,
            node_id: &str,
            pid: i32,
            sid: &str,
            host: &str,
            acquired_at: i64,
        ) {
            let name = format!("{}.lock", encode_claim_key(&format!("node:{node_id}")));
            std::fs::write(
                self.claims().join(name),
                format!(
                    "schema_version: 1\nkey: node:{node_id}\nholder: target-session:{sid}\n\
                     acquired_at: {acquired_at}\npid: {pid}\nhost: {host}\nexpires_at: 9999999999999\n"
                ),
            )
            .unwrap();
        }
        fn write_event(&self, sid: &str, ts: &str) {
            let line =
                format!(r#"{{"ts":"{ts}","type":"loop_check","data":{{"session_id":"{sid}"}}}}"#);
            let mut body = std::fs::read_to_string(self.events()).unwrap_or_default();
            body.push_str(&line);
            body.push('\n');
            std::fs::write(self.events(), body).unwrap();
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SID: &str = "20260709T001358Z-cl21834-267287";

    #[test]
    fn truth_badge_live_claim_recent_fire_is_working() {
        // AC1-HP e2e: live pid (this process) + a fire 2m ago -> Working.
        let t = Tmp::new("live");
        t.write_claim("x-4a48", std::process::id() as i32, SID);
        t.write_event(SID, "2026-07-09T01:05:00Z");
        let now = rfc3339_like_to_secs("2026-07-09T01:07:00Z").unwrap();
        let raw = reg(r#"{"name":"target-x-4a48-fleet","cwd":"/w","status":"live"}"#);
        let badges = build_truth_badges_at(&raw, now, &t.claims(), &t.events());
        assert_eq!(
            badges.get("target-x-4a48-fleet").map(String::as_str),
            Some("loop 2m ago")
        );
    }

    #[test]
    fn truth_badge_dead_pid_no_badge() {
        // AC3b/AC4: a dead-pid claim (suspect/stale) never earns a Working badge.
        let t = Tmp::new("dead");
        t.write_claim("x-4a48", 0x7fff_fff0, SID); // implausible pid -> dead
        t.write_event(SID, "2026-07-09T01:05:00Z");
        let now = rfc3339_like_to_secs("2026-07-09T01:07:00Z").unwrap();
        let raw = reg(r#"{"name":"target-x-4a48-fleet","cwd":"/w","status":"live"}"#);
        let badges = build_truth_badges_at(&raw, now, &t.claims(), &t.events());
        assert!(badges.is_empty());
    }

    #[test]
    fn truth_badge_reused_pid_no_badge() {
        // codex P3: holder exited and the pid was reused. The current process is
        // alive under that pid, but it STARTED after the claim's acquired_at, so
        // the create-time guard rejects it even with a recent fire.
        let t = Tmp::new("reuse");
        t.write_claim_full("x-4a48", std::process::id() as i32, SID, &hostname(), 1);
        t.write_event(SID, "2026-07-09T01:05:00Z");
        let now = rfc3339_like_to_secs("2026-07-09T01:07:00Z").unwrap();
        let raw = reg(r#"{"name":"target-x-4a48-fleet","cwd":"/w","status":"live"}"#);
        let badges = build_truth_badges_at(&raw, now, &t.claims(), &t.events());
        assert!(badges.is_empty());
    }

    #[test]
    fn truth_badge_cross_host_no_badge() {
        // A claim recorded on another host never badges here (its pid namespace
        // is not ours), even if the local pid happens to be live.
        let t = Tmp::new("xhost");
        t.write_claim_full(
            "x-4a48",
            std::process::id() as i32,
            SID,
            "some-other-host",
            9_999_999_999_999,
        );
        t.write_event(SID, "2026-07-09T01:05:00Z");
        let now = rfc3339_like_to_secs("2026-07-09T01:07:00Z").unwrap();
        let raw = reg(r#"{"name":"target-x-4a48-fleet","cwd":"/w","status":"live"}"#);
        let badges = build_truth_badges_at(&raw, now, &t.claims(), &t.events());
        assert!(badges.is_empty());
    }

    #[test]
    fn truth_badge_live_claim_stale_fire_no_badge() {
        // AC3-EDGE: claim-live but no recent fire -> waiting -> no sideline badge.
        let t = Tmp::new("staleFire");
        t.write_claim("x-4a48", std::process::id() as i32, SID);
        t.write_event(SID, "2026-07-09T01:05:00Z");
        let now = rfc3339_like_to_secs("2026-07-09T02:00:00Z").unwrap(); // 55m later
        let raw = reg(r#"{"name":"target-x-4a48-fleet","cwd":"/w","status":"live"}"#);
        let badges = build_truth_badges_at(&raw, now, &t.claims(), &t.events());
        assert!(badges.is_empty());
    }

    #[test]
    fn truth_badge_missing_signals_is_empty() {
        // AC5-ERR: no claim + no events file -> no badges, no panic.
        let t = Tmp::new("missing");
        let now = rfc3339_like_to_secs("2026-07-09T01:07:00Z").unwrap();
        let raw = reg(r#"{"name":"target-x-4a48-fleet","cwd":"/w","status":"live"}"#);
        let badges = build_truth_badges_at(&raw, now, &t.claims(), &t.events());
        assert!(badges.is_empty());
    }

    #[test]
    fn truth_badge_non_target_name_skipped() {
        // AC7-FR: a name with no parseable node id changes nothing.
        let t = Tmp::new("nontarget");
        t.write_claim("x-4a48", std::process::id() as i32, SID);
        t.write_event(SID, "2026-07-09T01:05:00Z");
        let now = rfc3339_like_to_secs("2026-07-09T01:07:00Z").unwrap();
        let raw = reg(r#"{"name":"worker-frontend","cwd":"/w","status":"live"}"#);
        let badges = build_truth_badges_at(&raw, now, &t.claims(), &t.events());
        assert!(badges.is_empty());
    }

    #[test]
    fn newest_fire_wins_and_missing_events_empty() {
        let t = Tmp::new("newest");
        t.write_event(SID, "2026-07-09T01:00:00Z");
        t.write_event(SID, "2026-07-09T01:05:00Z"); // newest
        t.write_event(SID, "2026-07-09T01:02:00Z");
        let now = rfc3339_like_to_secs("2026-07-09T01:07:00Z").unwrap();
        let ages = newest_fire_ages(&t.events(), now);
        assert_eq!(ages.get(SID), Some(&120)); // 01:05 -> 2m
        let empty = newest_fire_ages(&t.0.join("nope.jsonl"), now);
        assert!(empty.is_empty());
    }

    // -- external lifecycle reconcile (x-7561) --------------------------------

    use crate::squad_store::{ExternalLifecycle, ExternalState};

    fn tracked(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn record(id: &str, state: ExternalState) -> ExternalLifecycle {
        ExternalLifecycle {
            attach_id: id.into(),
            name: format!("row-{id}"),
            cwd: "/w".into(),
            state,
            generation: 1,
            updated_at: String::new(),
            reason: None,
        }
    }

    #[test]
    fn parse_claude_agents_filters_to_tracked_and_maps_state() {
        // Domain Pitfall 4: only tracked ids survive, so the historical rows the
        // daemon returns never flood the sideline.
        let raw = r#"[
            {"id":"deadbeef","state":"working"},
            {"id":"cafef00d","state":"stopped"},
            {"id":"11112222","state":"blocked"},
            {"id":"99998888","state":"done"},
            {"id":"aaaabbbb","state":"working"}
        ]"#;
        let got = parse_claude_agents(
            raw,
            &tracked(&["deadbeef", "cafef00d", "11112222", "99998888"]),
        )
        .unwrap();
        assert_eq!(got.get("deadbeef"), Some(&ObservedExternal::Live));
        assert_eq!(got.get("11112222"), Some(&ObservedExternal::Live));
        assert_eq!(got.get("cafef00d"), Some(&ObservedExternal::Terminal));
        assert_eq!(got.get("99998888"), Some(&ObservedExternal::Terminal));
        assert!(!got.contains_key("aaaabbbb"), "untracked id is dropped");
    }

    #[test]
    fn parse_claude_agents_maps_unknown_state_to_unknown_not_absence() {
        // codex P2: a tracked id PRESENT with a new/malformed/missing state is
        // per-id schema drift, kept in the map as `Unknown` - never dropped
        // (which reconcile would read as absence and delete).
        let raw = r#"[
            {"id":"deadbeef","state":"paused"},
            {"id":"cafef00d"}
        ]"#;
        let got = parse_claude_agents(raw, &tracked(&["deadbeef", "cafef00d"])).unwrap();
        assert_eq!(got.get("deadbeef"), Some(&ObservedExternal::Unknown));
        assert_eq!(got.get("cafef00d"), Some(&ObservedExternal::Unknown));
    }

    #[test]
    fn reconcile_holds_a_record_observed_unknown() {
        // codex P2: a tracked id observed as Unknown (present but indeterminate)
        // is HELD as unknown, never deleted as absent.
        let mut obs = HashMap::new();
        obs.insert("deadbeef".to_string(), ObservedExternal::Unknown);
        let (out, _) = reconcile_external(
            vec![record("deadbeef", ExternalState::Stopping)],
            Some(&obs),
        );
        assert_eq!(out.len(), 1, "an unknown-observed record is not deleted");
        assert_eq!(out[0].state, ExternalState::Unknown);
    }

    #[test]
    fn parse_claude_agents_none_on_schema_drift() {
        // A non-array (or an unparseable body) is schema drift -> None, so the
        // caller retains rows as unknown instead of deleting them (AC1-FR).
        assert!(parse_claude_agents(r#"{"agents":[]}"#, &tracked(&["deadbeef"])).is_none());
        assert!(parse_claude_agents("not json", &tracked(&["deadbeef"])).is_none());
        // An empty array is a valid observation (nothing tracked is live).
        assert!(parse_claude_agents("[]", &tracked(&["deadbeef"]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reconcile_stopping_resolves_by_observation() {
        // AC1-FR: stopping + terminal -> stopped; + live -> failed (retry); +
        // absent -> removed (dropped).
        let mut obs = HashMap::new();
        obs.insert("deadbeef".to_string(), ObservedExternal::Terminal);
        let (out, _) = reconcile_external(
            vec![record("deadbeef", ExternalState::Stopping)],
            Some(&obs),
        );
        assert_eq!(out[0].state, ExternalState::Stopped);

        obs.insert("deadbeef".to_string(), ObservedExternal::Live);
        let (out, _) = reconcile_external(
            vec![record("deadbeef", ExternalState::Stopping)],
            Some(&obs),
        );
        assert_eq!(out[0].state, ExternalState::Failed);

        // absent from the map -> deleted as removed.
        let (out, _) = reconcile_external(
            vec![record("deadbeef", ExternalState::Stopping)],
            Some(&HashMap::new()),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn reconcile_removing_returns_to_stopped_for_retry() {
        // AC3-FR: removing + terminal -> stopped (explicit rm retry); + absent ->
        // deleted as already removed.
        let mut obs = HashMap::new();
        obs.insert("deadbeef".to_string(), ObservedExternal::Terminal);
        let (out, _) = reconcile_external(
            vec![record("deadbeef", ExternalState::Removing)],
            Some(&obs),
        );
        assert_eq!(out[0].state, ExternalState::Stopped);

        let (out, _) = reconcile_external(
            vec![record("deadbeef", ExternalState::Removing)],
            Some(&HashMap::new()),
        );
        assert!(out.is_empty(), "removing + absent is fully removed");
    }

    #[test]
    fn reconcile_stopped_live_clears_and_terminal_remains() {
        // A stopped tombstone seen live again clears (roster owns the row); seen
        // terminal it remains a tombstone.
        let mut obs = HashMap::new();
        obs.insert("deadbeef".to_string(), ObservedExternal::Live);
        let (out, _) =
            reconcile_external(vec![record("deadbeef", ExternalState::Stopped)], Some(&obs));
        assert!(out.is_empty(), "a stopped row that is live again clears");

        obs.insert("deadbeef".to_string(), ObservedExternal::Terminal);
        let (out, _) =
            reconcile_external(vec![record("deadbeef", ExternalState::Stopped)], Some(&obs));
        assert_eq!(out[0].state, ExternalState::Stopped);
    }

    #[test]
    fn reconcile_unavailable_holds_everything_unknown() {
        // AC1-FR "query unavailable": every non-unknown row is held as unknown
        // (safe stop retry), never deleted; an already-unknown row stays put.
        let (out, _) = reconcile_external(
            vec![
                record("deadbeef", ExternalState::Removing),
                record("cafef00d", ExternalState::Unknown),
            ],
            None,
        );
        assert_eq!(
            out.len(),
            2,
            "nothing is deleted when the query is unavailable"
        );
        assert!(out.iter().all(|r| r.state == ExternalState::Unknown));
    }

    // ---- resolve_branch (x-cd67 US4) ----

    fn branch_tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fno-branch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolve_branch_reads_plain_checkout_head() {
        let cwd = branch_tmp("plain");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(resolve_branch(&cwd), Some("main".into()));
        // A slash-bearing branch keeps only its leaf name.
        std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/feature/x-cd67\n").unwrap();
        assert_eq!(resolve_branch(&cwd), Some("x-cd67".into()));
        std::fs::remove_dir_all(&cwd).unwrap();
    }

    #[test]
    fn resolve_branch_detached_head_shortens_sha() {
        let cwd = branch_tmp("detached");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::write(
            cwd.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(resolve_branch(&cwd), Some("01234567".into()));
        std::fs::remove_dir_all(&cwd).unwrap();
    }

    #[test]
    fn resolve_branch_follows_worktree_gitdir_redirect() {
        // AC3-EDGE: a linked-worktree `.git` FILE points at the real gitdir.
        let root = branch_tmp("wt");
        let real_gitdir = root.join(".git/worktrees/x-cd67");
        std::fs::create_dir_all(&real_gitdir).unwrap();
        std::fs::write(real_gitdir.join("HEAD"), "ref: refs/heads/x-cd67\n").unwrap();
        let cwd = root.join("checkout");
        std::fs::create_dir_all(&cwd).unwrap();
        // A relative gitdir pointer resolves against cwd.
        std::fs::write(cwd.join(".git"), "gitdir: ../.git/worktrees/x-cd67\n").unwrap();
        assert_eq!(resolve_branch(&cwd), Some("x-cd67".into()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_branch_degrades_on_no_git_and_malformed_head() {
        // AC1-ERR: a plain dir with no .git -> None (poll must not error).
        let cwd = branch_tmp("nogit");
        assert_eq!(resolve_branch(&cwd), None);
        // A malformed HEAD (neither ref: nor 40-hex) -> None.
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::write(cwd.join(".git/HEAD"), "garbage not a ref\n").unwrap();
        assert_eq!(resolve_branch(&cwd), None);
        // A worktree pointer whose gitdir target vanished -> None (pruned wt).
        std::fs::write(cwd.join(".git/HEAD.tmp"), "x").unwrap(); // noise
        let dangling = branch_tmp("dangling");
        std::fs::write(dangling.join(".git"), "gitdir: /nonexistent/gitdir\n").unwrap();
        assert_eq!(resolve_branch(&dangling), None);
        std::fs::remove_dir_all(&cwd).unwrap();
        std::fs::remove_dir_all(&dangling).unwrap();
    }
}
