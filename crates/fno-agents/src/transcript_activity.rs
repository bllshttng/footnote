//! The transcript activity fold behind the fleet page: which sessions were
//! active in each hour, how many cargo and pytest runs started, and the
//! operator turns that said the machine slowed down.
//!
//! Incremental with a per-tick byte budget. Cursors in
//! `<state>/fleet/activity.json` carry each file's offset, a sha256 head
//! sample and the last hour counted, so a tick reads only appended bytes and
//! a replaced file never double counts. Files read newest-mtime first; the
//! first 30-day backfill spreads over many ticks instead of holding the
//! whole corpus.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::gh_budget::{lock_path, FileLock};
use crate::provenance::{
    classify, classify_turn, is_user_turn, turn_text, turn_ts_epoch, BusIndex, ClaudeSource,
    CodexSource, Provenance, TranscriptSource,
};

/// ponytail: per-tick byte budget, sized so one tick never scans the whole
/// 16.5 GiB corpus; the remainder waits for later ticks, newest files first.
pub(crate) const TICK_READ_BUDGET: u64 = 512 * 1024 * 1024;
/// ponytail: fixed read window so memory stays flat; a single line longer
/// than one chunk is skipped uncounted rather than grown around, which real
/// transcripts never produce.
pub(crate) const CHUNK: u64 = 8 * 1024 * 1024;
/// ponytail: head-sample cap for replacement detection; a small file samples
/// whole, so appends inside the cap are told from rewrites by length too.
pub(crate) const HEAD_SAMPLE_BYTES: u64 = 4096;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HourCounts {
    pub claude: u64,
    pub subagent: u64,
    pub codex: u64,
    pub cargo: u64,
    pub pytest: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Slowdown {
    pub ts: String,
    pub session: String,
    pub harness: String,
    pub text: String,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Activity {
    pub hours: BTreeMap<String, HourCounts>,
    pub slowdowns: Vec<Slowdown>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub(crate) struct FoldReceipt {
    pub files_read: u64,
    pub bytes_read: u64,
    pub pending_files: u64,
    pub pending_bytes: u64,
    pub replaced: u64,
    pub cache_reset: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Roots {
    pub claude_projects: PathBuf,
    pub codex_sessions: Option<PathBuf>,
    pub bus_log: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Claude,
    Subagent,
    Codex,
}

struct Listed {
    path: PathBuf,
    session: String,
    kind: Kind,
    mtime: u64,
    size: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Cursor {
    offset: u64,
    head: String,
    head_len: u64,
    last_hour: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    cursors: BTreeMap<String, Cursor>,
    hours: BTreeMap<String, HourCounts>,
    slowdowns: Vec<Slowdown>,
}

fn hour_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#""timestamp":"(\d{4}-\d\d-\d\dT\d\d)"#).expect("valid pattern")
    })
}

fn run_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"\b(cargo (?:test|nextest|build|clippy|check)|pytest|fno test|fno doctor test)\b",
        )
        .expect("valid pattern")
    })
}

fn slow_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(slow|slowed|slowing|crushing|laggy|lagging|hanging|frozen|freezing|sluggish)\b",
        )
        .expect("valid pattern")
    })
}

fn hour_string(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H").to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn head_sample(path: &Path, len: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; len.min(HEAD_SAMPLE_BYTES) as usize];
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    Some(hex(&Sha256::digest(&buf[..filled])))
}

fn load_cache(path: &Path) -> Result<(BTreeMap<String, Cursor>, Activity), String> {
    // A missing cache is a first run, not a reset: start empty with no note.
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((BTreeMap::new(), Activity::default()));
        }
        Err(e) => return Err(format!("unreadable: {e}")),
    };
    let file: CacheFile = serde_json::from_str(&text).map_err(|e| format!("unparseable: {e}"))?;
    if file.version != 1 {
        return Err(format!("version {} unsupported", file.version));
    }
    Ok((
        file.cursors,
        Activity {
            hours: file.hours,
            slowdowns: file.slowdowns,
        },
    ))
}

fn save_cache(cache: &Path, cursors: &BTreeMap<String, Cursor>, activity: &Activity) {
    let body = serde_json::to_string(&CacheFile {
        version: 1,
        cursors: cursors.clone(),
        hours: activity.hours.clone(),
        slowdowns: activity.slowdowns.clone(),
    })
    .unwrap_or_else(|_| "{}".to_string());
    let _ = crate::king_ledger::write_atomic(&cache.to_path_buf(), &body);
}

fn list_files(roots: &Roots, now: DateTime<Utc>, window_days: u64) -> Vec<Listed> {
    let mut out = Vec::new();
    let claude = ClaudeSource {
        cwd: PathBuf::new(),
        all_projects: true,
        projects_dir: roots.claude_projects.clone(),
    };
    for s in claude.sessions(window_days) {
        out.push(Listed {
            path: s.path,
            session: s.session_id,
            kind: Kind::Claude,
            mtime: s.mtime,
            size: s.size,
        });
    }
    // Subagent transcripts nest under the parent session's directory, one
    // level the session listing never walks.
    let cutoff = now.timestamp().max(0) as u64 - window_days * 86_400;
    let projects = &roots.claude_projects;
    if let Ok(slugs) = std::fs::read_dir(projects) {
        for slug in slugs.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let Ok(sessions) = std::fs::read_dir(&slug) else {
                continue;
            };
            for session in sessions.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
                let dir = session.join("subagents");
                let Ok(files) = std::fs::read_dir(&dir) else {
                    continue;
                };
                let parent = session
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                for f in files.flatten() {
                    let path = f.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Ok(meta) = std::fs::metadata(&path) else {
                        continue;
                    };
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if mtime < cutoff {
                        continue;
                    }
                    out.push(Listed {
                        path,
                        session: parent.clone(),
                        kind: Kind::Subagent,
                        mtime,
                        size: meta.len(),
                    });
                }
            }
        }
    }
    let codex = CodexSource {
        sessions_dir: roots.codex_sessions.clone(),
        cwd: None,
    };
    for s in codex.sessions(window_days) {
        out.push(Listed {
            path: s.path,
            session: s.session_id,
            kind: Kind::Codex,
            mtime: s.mtime,
            size: s.size,
        });
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    out
}

/// One incremental pass over every transcript inside the window, bounded by
/// `budget` read bytes (`None` reads everything, the verb's `--backfill`).
/// Persists cursors to `cache` (mode 600) so the next run reads only new
/// bytes. Holds the fleet cache lock for the whole read-modify-write, so a
/// caller and the daemon arm never fold at once.
pub(crate) fn fold(
    cache: &Path,
    roots: &Roots,
    now: DateTime<Utc>,
    window_days: u64,
    budget: Option<u64>,
) -> Result<(Activity, FoldReceipt), String> {
    let _guard = FileLock::acquire(&lock_path(cache));
    let mut receipt = FoldReceipt::default();
    let bus = BusIndex::load(&roots.bus_log);
    let (mut cursors, mut activity) = match load_cache(cache) {
        Ok(pair) => pair,
        Err(why) => {
            receipt.cache_reset = Some(why);
            (BTreeMap::new(), Activity::default())
        }
    };
    let mut seen: HashSet<(String, String)> = activity
        .slowdowns
        .iter()
        .map(|s| (s.session.clone(), s.ts.clone()))
        .collect();
    let floor = hour_string(now - chrono::Duration::days(window_days as i64));
    for file in list_files(roots, now, window_days) {
        let key = file.path.display().to_string();
        let mut cur = match cursors.get(&key) {
            Some(c) => {
                let sampled = head_sample(&file.path, c.head_len);
                if file.size < c.offset || sampled.as_deref() != Some(c.head.as_str()) {
                    receipt.replaced += 1;
                    Cursor {
                        offset: file.size,
                        head: sampled.unwrap_or_default(),
                        head_len: c.head_len,
                        last_hour: c.last_hour.clone(),
                    }
                } else {
                    c.clone()
                }
            }
            None => {
                let head_len = file.size.min(HEAD_SAMPLE_BYTES);
                Cursor {
                    offset: 0,
                    head: head_sample(&file.path, head_len).unwrap_or_default(),
                    head_len,
                    last_hour: None,
                }
            }
        };
        let mut remaining = budget
            .unwrap_or(u64::MAX)
            .saturating_sub(receipt.bytes_read);
        if remaining > 0 && cur.offset < file.size {
            if let Ok(mut f) = std::fs::File::open(&file.path) {
                let mut buf = vec![0u8; CHUNK as usize];
                let mut last_hour = cur.last_hour.clone();
                let mut consumed = 0u64;
                while cur.offset + consumed < file.size && remaining > 0 {
                    let want =
                        (remaining.min(CHUNK)).min(file.size - cur.offset - consumed) as usize;
                    if f.seek(SeekFrom::Start(cur.offset + consumed)).is_err() {
                        break;
                    }
                    let mut filled = 0;
                    while filled < want {
                        match f.read(&mut buf[filled..want]) {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            Err(_) => break,
                        }
                    }
                    if filled == 0 {
                        break;
                    }
                    let slice = &buf[..filled];
                    let keep = match slice.iter().rposition(|b| *b == b'\n') {
                        Some(pos) => pos + 1,
                        // A trailing line with no newline waits for the next
                        // tick. A full chunk with no newline is an oversized
                        // line: skip it uncounted so the fold stays live.
                        None => {
                            if filled == want && want == CHUNK as usize {
                                filled
                            } else {
                                0
                            }
                        }
                    };
                    if keep == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&slice[..keep]);
                    for line in text.lines() {
                        process_line(
                            line,
                            &file,
                            &floor,
                            &bus,
                            &mut last_hour,
                            &mut activity,
                            &mut seen,
                        );
                    }
                    consumed += keep as u64;
                    receipt.bytes_read += keep as u64;
                    remaining -= keep as u64;
                }
                cur.offset += consumed;
                cur.last_hour = last_hour;
            }
        }
        receipt.files_read += 1;
        if cur.offset < file.size {
            receipt.pending_files += 1;
            receipt.pending_bytes += file.size - cur.offset;
        }
        cursors.insert(key, cur);
    }
    cursors.retain(|k, _| Path::new(k).exists());
    save_cache(&cache, &cursors, &activity);
    Ok((activity, receipt))
}

fn count_runs(text: &str, hour: &str, activity: &mut Activity) {
    for m in run_re().find_iter(text) {
        let e = activity.hours.entry(hour.to_string()).or_default();
        if m.as_str().starts_with("cargo") {
            e.cargo += 1;
        } else {
            e.pytest += 1;
        }
    }
}

fn codex_call_text(line: &str) -> Option<String> {
    let row = serde_json::from_str::<Value>(line).ok()?;
    let p = row.get("payload")?;
    match p.get("type").and_then(|v| v.as_str())? {
        "custom_tool_call" | "function_call" | "local_shell_call" => {}
        _ => return None,
    }
    ["input", "arguments", "action"].iter().find_map(|k| {
        p.get(*k).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    })
}

fn process_line(
    line: &str,
    file: &Listed,
    floor: &str,
    bus: &BusIndex,
    last_hour: &mut Option<String>,
    activity: &mut Activity,
    seen: &mut HashSet<(String, String)>,
) {
    let Some(m) = hour_re().captures(line).and_then(|c| c.get(1)) else {
        return;
    };
    let hour = m.as_str();
    if hour < floor {
        return;
    }
    let new_hour = last_hour.as_ref().is_none_or(|h| hour > h.as_str());
    if new_hour {
        let e = activity.hours.entry(hour.to_string()).or_default();
        match file.kind {
            Kind::Claude => e.claude += 1,
            Kind::Subagent => e.subagent += 1,
            Kind::Codex => e.codex += 1,
        }
        *last_hour = Some(hour.to_string());
    }
    if line.contains("\"tool_use\"") && file.kind == Kind::Claude {
        for cmd in crate::bash_census::commands_in_line(line) {
            count_runs(&cmd, hour, activity);
        }
    } else if file.kind == Kind::Codex {
        if let Some(text) = codex_call_text(line) {
            count_runs(&text, hour, activity);
        }
    }
    if file.kind == Kind::Subagent || !line.contains("\"user\"") {
        return;
    }
    let Ok(obj) = serde_json::from_str(&line) else {
        return;
    };
    if !is_user_turn(&obj) {
        return;
    }
    // The bus join drops a mail row injected as a user turn: a body delivered
    // to this session reads as a relay, never as the operator speaking.
    if classify_turn(&obj, bus, &file.session) != Provenance::Operator {
        return;
    }
    let Ok(cleaned) = classify(&turn_text(&obj)) else {
        return;
    };
    if cleaned.chars().count() >= 2000 || !slow_re().is_match(&cleaned) {
        return;
    }
    let Some(secs) = turn_ts_epoch(&obj) else {
        return;
    };
    let ts = rfc3339(secs);
    if seen.insert((file.session.clone(), ts.clone())) {
        activity.slowdowns.push(Slowdown {
            ts,
            session: file.session.clone(),
            harness: if file.kind == Kind::Codex {
                "codex".to_string()
            } else {
                "claude".to_string()
            },
            text: cleaned.chars().take(240).collect(),
        });
    }
}

fn rfc3339(secs: f64) -> String {
    let whole = secs.floor() as i64;
    DateTime::from_timestamp(whole, ((secs - whole as f64) * 1e9) as u32)
        .unwrap_or(DateTime::UNIX_EPOCH)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-transcript-activity-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    const SLOW: &str = "everything is so slow";

    fn claude_user(ts: &str, text: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"{ts}\",\"message\":{{\"content\":\"{text}\"}}}}\n"
        )
    }

    fn codex_user(ts: &str, text: &str) -> String {
        format!(
            "{{\"timestamp\":\"{ts}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    }

    fn tool_use(ts: &str, command: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{ts}\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{{\"command\":\"{command}\"}}}}]}}}}\n"
        )
    }

    fn roots(dir: &Path) -> Roots {
        Roots {
            claude_projects: dir.join("projects"),
            codex_sessions: Some(dir.join("codex")),
            bus_log: dir.join("bus/messages.jsonl"),
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 19, 17, 0, 0).unwrap()
    }

    fn fixture(roots: &Roots) {
        let projects = roots.claude_projects.clone();
        write(
            &projects.join("-proj/s-1.jsonl"),
            &format!(
                "{}{}{}{}",
                claude_user("2026-09-19T15:02:00Z", SLOW),
                tool_use("2026-09-19T15:05:00Z", "cargo test"),
                claude_user("2026-09-19T16:02:00Z", "hello"),
                tool_use("2026-09-19T16:05:00Z", "cargo build"),
            ),
        );
        write(
            &projects.join("-proj/s-1/subagents/a.jsonl"),
            &claude_user("2026-09-19T15:10:00Z", "so slow"),
        );
        write(
            &roots
                .codex_sessions
                .clone()
                .unwrap()
                .join("rollout-2026-09-19T15-11111111-2222-3333-4444-555555555555.jsonl"),
            &format!(
                "{{\"timestamp\":\"2026-09-19T15:20:00Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"session_meta\",\"session_id\":\"codess\",\"cwd\":\"/x\"}}}}\n{{\"timestamp\":\"2026-09-19T15:25:00Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"custom_tool_call\",\"input\":\"tools.exec_command({{cmd:\\\"pytest -q\\\"}})\"}}}}\n{}",
                codex_user("2026-09-19T15:30:00Z", "codex hello")
            ),
        );
    }

    #[test]
    fn ac1_counts_hours_runs_and_one_slowdown() {
        let dir = tmp_dir("ac1");
        let r = roots(&dir);
        fixture(&r);
        let cache = dir.join("fleet/activity.json");
        let (activity, receipt) = fold(&cache, &r, now(), 30, None).unwrap();
        let h15 = activity.hours.get("2026-09-19T15").unwrap();
        assert_eq!(h15.claude, 1);
        assert_eq!(h15.subagent, 1);
        assert_eq!(h15.codex, 1);
        assert_eq!(h15.cargo, 1);
        assert_eq!(h15.pytest, 1);
        let h16 = activity.hours.get("2026-09-19T16").unwrap();
        assert_eq!(h16.claude, 1);
        assert_eq!(h16.codex, 0);
        assert_eq!(activity.slowdowns.len(), 1, "exactly one slowdown");
        let s = &activity.slowdowns[0];
        assert_eq!(s.text, SLOW);
        assert_eq!(s.harness, "claude");
        assert_eq!(s.session, "s-1");
        assert_eq!(receipt.replaced, 0);
        assert_eq!(receipt.pending_bytes, 0);
        assert!(receipt.cache_reset.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac2_appends_count_once_per_new_hour() {
        let dir = tmp_dir("ac2");
        let r = roots(&dir);
        fixture(&r);
        let cache = dir.join("fleet/activity.json");
        fold(&cache, &r, now(), 30, None).unwrap();
        let main = r.claude_projects.join("-proj/s-1.jsonl");
        let mut body = std::fs::read_to_string(&main).unwrap();
        body.push_str(&claude_user("2026-09-19T15:40:00Z", "again"));
        body.push_str(&tool_use("2026-09-19T17:05:00Z", "cargo test"));
        std::fs::write(&main, body).unwrap();
        let (activity, receipt) = fold(&cache, &r, now(), 30, None).unwrap();
        assert_eq!(activity.hours.get("2026-09-19T15").unwrap().claude, 1);
        assert_eq!(activity.hours.get("2026-09-19T16").unwrap().claude, 1);
        let h17 = activity.hours.get("2026-09-19T17").unwrap();
        assert_eq!(h17.claude, 1);
        assert_eq!(h17.cargo, 1);
        assert_eq!(receipt.replaced, 0);
        assert_eq!(activity.slowdowns.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac3_budget_splits_across_runs_equaling_one() {
        let dir = tmp_dir("ac3");
        let r = roots(&dir);
        fixture(&r);
        let cache = dir.join("fleet/activity.json");
        let (_a1, r1) = fold(&cache, &r, now(), 30, Some(160)).unwrap();
        assert!(r1.pending_files >= 1, "budget stops on a line boundary");
        assert!(r1.pending_bytes > 0);
        let (a2, r2) = fold(&cache, &r, now(), 30, None).unwrap();
        assert!(r2.cache_reset.is_none());
        let (whole, _wr) = {
            let dir2 = tmp_dir("ac3-whole");
            let r2r = roots(&dir2);
            fixture(&r2r);
            let out = fold(&dir2.join("fleet/activity.json"), &r2r, now(), 30, None).unwrap();
            let _ = std::fs::remove_dir_all(&dir2);
            out
        };
        // The two runs together must equal one unbounded fold exactly.
        assert_eq!(a2, whole);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac3_trailing_partial_line_waits() {
        let dir = tmp_dir("ac3p");
        let r = roots(&dir);
        let projects = &r.claude_projects;
        write(
            &projects.join("-proj/s-2.jsonl"),
            &format!(
                "{}{{\"type\":\"user\",\"timestamp\":\"2026-09-19T15:02:00Z\",",
                claude_user("2026-09-19T14:00:00Z", "hi")
            ),
        );
        let cache = dir.join("fleet/activity.json");
        let (activity, receipt) = fold(&cache, &r, now(), 30, None).unwrap();
        assert_eq!(activity.hours.len(), 1, "the partial line is not consumed");
        assert!(receipt.pending_bytes > 0);
        let main = projects.join("-proj/s-2.jsonl");
        let mut body = std::fs::read_to_string(&main).unwrap();
        body.push_str("\"message\":{\"content\":\"done\"}}\n");
        std::fs::write(&main, body).unwrap();
        let (activity, receipt) = fold(&cache, &r, now(), 30, None).unwrap();
        assert!(activity.hours.contains_key("2026-09-19T15"));
        assert_eq!(receipt.pending_bytes, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac4_subagent_notification_and_bus_rows_are_not_slowdowns() {
        let dir = tmp_dir("ac4");
        let r = roots(&dir);
        let projects = &r.claude_projects;
        // A subagent's user turn is the parent's prompt, never a person.
        write(
            &projects.join("-proj/s-3/subagents/a.jsonl"),
            &claude_user("2026-09-19T15:10:00Z", "so slow"),
        );
        // A task-notification row is relay traffic, not a person; the same
        // transcript also carries the bus-delivered turn tested below.
        // A user turn whose text was delivered to this session over the bus
        // is a relay row, not the operator speaking.
        write(
            &r.bus_log,
            &format!(
                "{{\"id\":\"m1\",\"body\":\"so slow\",\"meta\":{{\"to_session\":\"s-3\"}}}}\n"
            ),
        );
        write(
            &projects.join("-proj/s-3.jsonl"),
            &format!(
                "{}{}",
                claude_user(
                    "2026-09-19T15:11:00Z",
                    "<task-notification>so slow</task-notification>"
                ),
                claude_user("2026-09-19T15:12:00Z", "so slow")
            ),
        );
        let cache = dir.join("fleet/activity.json");
        let (activity, _) = fold(&cache, &r, now(), 30, None).unwrap();
        assert!(activity.slowdowns.is_empty());
        assert_eq!(activity.hours.get("2026-09-19T15").unwrap().subagent, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaced_file_never_double_counts() {
        let dir = tmp_dir("replaced");
        let r = roots(&dir);
        fixture(&r);
        let cache = dir.join("fleet/activity.json");
        fold(&cache, &r, now(), 30, None).unwrap();
        let main = r.claude_projects.join("-proj/s-1.jsonl");
        std::fs::write(&main, &claude_user("2026-09-19T18:00:00Z", "new life")).unwrap();
        let (activity, receipt) = fold(&cache, &r, now(), 30, None).unwrap();
        assert_eq!(receipt.replaced, 1);
        assert!(activity.hours.get("2026-09-19T18").is_none());
        assert_eq!(activity.slowdowns.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
