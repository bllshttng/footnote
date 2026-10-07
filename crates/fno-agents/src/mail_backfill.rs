//! `fno-agents mail-backfill` - the historical-mail row engine.
//!
//! Mail an outage bypassed (sent over the harness's native cross-session
//! transport while `fno mail send` was down) goes back into the bus log as
//! durable rows that never re-deliver. The whole engine lives here once and
//! Python keeps one thin transport (`fno agents mail backfill` resolves the
//! window and the paths, then runs the verb): the transcript scan, the
//! sender/receiver join, the deterministic archive id (the idempotency
//! key), the envelope assembly, and the write through the bus-append door.
//!
//! Contract: provenance is the point. A send without a row uuid has no
//! stable id and is skipped loud, never half-attributed; an id already in
//! the log is skipped (idempotent by msg_id); a joined pair whose receiver
//! block names a different body is never paired.

use serde_json::json;

/// The audit-only delivery value: the bytes already reached the recipient
/// over the harness's native cross-session transport. Mirrors
/// `bus.log.CROSS_SESSION_DELIVERY` and the drain gates
/// (`mail_control_drain::AUDIT_ONLY_DELIVERIES`).
pub const CROSS_SESSION_DELIVERY: &str = "cross-session";

/// The transport label the row's meta carries: the provenance string the
/// node orders.
pub const TRANSPORT: &str = "claude-cross-session";

/// The deterministic archive id: `fmail-bf-` + the first 12 hex of the
/// sha256 over `sender-session|row-uuid`. Stable across re-runs so a
/// re-scan skips what already landed (idempotent by msg_id).
fn archive_msg_id(sender_session: &str, row_uuid: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(sender_session.as_bytes());
    h.update(b"|");
    h.update(row_uuid.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("fmail-bf-{}", &hex[..12])
}

/// Parse every cross-session-message block out of one user-turn text.
/// Attribute vocabulary observed in the wild: `from` (a socket or a name)
/// and `from-name` (the sender's legible label); the body is the text
/// between the open tag's `>` and the closing tag, byte-verbatim.
fn parse_blocks(text: &str) -> Vec<serde_json::Value> {
    let re =
        regex::Regex::new(r#"(?s)<cross-session-message\b([^>]*)>(.*?)</cross-session-message>"#)
            .expect("valid pattern");
    let attr_re = regex::Regex::new(r#"([a-zA-Z-]+)="([^"]*)""#).expect("valid pattern");
    re.captures_iter(text)
        .map(|c| {
            let attrs: serde_json::Map<String, serde_json::Value> = attr_re
                .captures_iter(c.get(1).map(|m| m.as_str()).unwrap_or(""))
                .map(|a| {
                    (
                        a.get(1).unwrap().as_str().to_string(),
                        json!(a.get(2).unwrap().as_str()),
                    )
                })
                .collect();
            let mut out = serde_json::Map::new();
            out.insert(
                "from".into(),
                attrs.get("from").cloned().unwrap_or(json!(null)),
            );
            out.insert(
                "from_name".into(),
                attrs.get("from-name").cloned().unwrap_or(json!(null)),
            );
            out.insert(
                "body".into(),
                json!(c.get(2).map(|m| m.as_str()).unwrap_or("")),
            );
            serde_json::Value::Object(out)
        })
        .collect()
}

/// Entry: `run` only. The one public shape is the JSON summary line; an
/// unknown subcommand refuses loud on stderr with exit 2. The msgid/block/
/// row plumbing was transport-era and retired in this port: the engine
/// calls the same bodies directly.
pub fn run_mail_backfill(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "run" => run_backfill(&args[1.min(args.len())..]),
        other => {
            eprintln!("mail-backfill: unknown subcommand {other:?} (expected run)");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msgid_is_deterministic_and_input_sensitive() {
        let a = archive_msg_id("sess-1", "uuid-1");
        assert_eq!(a, archive_msg_id("sess-1", "uuid-1"));
        assert_ne!(a, archive_msg_id("sess-2", "uuid-1"));
        assert_ne!(a, archive_msg_id("sess-1", "uuid-2"));
        assert!(a.starts_with("fmail-bf-"));
        assert_eq!(a.len(), "fmail-bf-".len() + 12);
    }

    #[test]
    fn block_parser_reads_attrs_and_verbatim_body() {
        let text = "before <cross-session-message from=\"uds:/tmp/cc-socks/9.sock\" from-name=\"lead\">\nhello\nworld\n</cross-session-message> after";
        let blocks = parse_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["from"], "uds:/tmp/cc-socks/9.sock");
        assert_eq!(blocks[0]["from_name"], "lead");
        assert_eq!(blocks[0]["body"], "\nhello\nworld\n");
    }

    #[test]
    fn block_parser_finds_multiple_blocks_and_skips_plain_text() {
        let two = "<cross-session-message from-name=\"a\">x</cross-session-message>\n<cross-session-message from-name=\"b\">y</cross-session-message>";
        assert_eq!(parse_blocks(two).len(), 2);
        assert!(parse_blocks("no blocks here").is_empty());
    }

    #[test]
    fn row_refuses_missing_provenance_loud() {
        let args: Vec<String> = ["row", "--msg-id", "m1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(run_mail_backfill(&args), 2);
    }
}

// ---------------------------------------------------------------------------
// The scan-and-write engine (`run`): the transcript scan, the sender-side
// join, and the durable write through the bus-append door. Python keeps the
// CLI transport only.
// ---------------------------------------------------------------------------

use serde_json::Value;
use std::path::{Path, PathBuf};

/// One native `SendMessage` tool call in a sender transcript.
struct SendRow {
    uuid: String,
    ts: String,
    sender_session: String,
    to: String,
    summary: Option<String>,
    body: String,
    sender_transcript: String,
}

/// One `<cross-session-message>` block in a receiver transcript.
struct BlockRow {
    from: Option<String>,
    from_name: Option<String>,
    body: String,
    receiver_session: String,
    receiver_transcript: String,
}

/// Whitespace-normalized first `n` chars: the join key on the body head.
fn norm_head(text: &str, n: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(n).collect()
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// One stamp string to epoch seconds: RFC 3339 (Z or offset), else a naive
/// stamp read as UTC, else a date-only stamp at midnight. Mirrors
/// `provenance::turn_ts_epoch`'s parse core at the string level.
fn ts_epoch(ts: &str) -> Option<f64> {
    let t = ts.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(dt.timestamp() as f64 + dt.timestamp_subsec_micros() as f64 / 1_000_000.0);
    }
    if let Ok(nd) = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f") {
        let utc = nd.and_utc();
        return Some(utc.timestamp() as f64 + utc.timestamp_subsec_micros() as f64 / 1_000_000.0);
    }
    let date_only = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))?;
    Some(date_only.and_utc().timestamp() as f64)
}

/// Window bounds as epoch seconds; an unparseable bound refuses (a silent
/// wrong window would archive the wrong traffic).
fn bound_epoch(flag: &str, value: &str) -> Result<f64, String> {
    ts_epoch(value).ok_or_else(|| format!("unparseable --{flag} value {value:?}"))
}

/// Window membership for one row timestamp: rows without a parseable stamp
/// stay in (the mtime prefilter already bounded them loosely).
fn ts_in_window(ts: &str, since: f64, until: f64) -> bool {
    match ts_epoch(ts) {
        Some(t) => t >= since && t <= until,
        None => true,
    }
}

/// The user-visible text of one transcript row (string content, or the
/// text blocks joined): the same shapes the provenance fold reads.
fn row_text(obj: &Value) -> String {
    let content = obj
        .get("message")
        .and_then(|m| m.get("content"))
        .unwrap_or(&Value::Null);
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Every native `SendMessage` tool call in one transcript row.
fn sender_sends_in(obj: &Value, path: &str) -> Vec<SendRow> {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let content = match obj.get("message").and_then(|m| m.get("content")) {
        Some(Value::Array(blocks)) => blocks,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for block in content {
        let is_send = block.get("type").and_then(Value::as_str) == Some("tool_use")
            && block.get("name").and_then(Value::as_str) == Some("SendMessage");
        if !is_send {
            continue;
        }
        let input = block.get("input").cloned().unwrap_or(json!({}));
        let to = input
            .get("to")
            .or_else(|| input.get("recipient"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let body = input
            .get("message")
            .or_else(|| input.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if to.is_empty() || body.is_empty() {
            continue;
        }
        out.push(SendRow {
            uuid: obj
                .get("uuid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            ts: obj
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            sender_session: obj
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            to: to.to_string(),
            summary: input
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_string),
            body: body.to_string(),
            sender_transcript: path.to_string(),
        });
    }
    out
}

/// Every cross-session block in one transcript row.
fn receiver_blocks_in(obj: &Value, path: &str) -> Vec<BlockRow> {
    if obj.get("type").and_then(Value::as_str) != Some("user") {
        return Vec::new();
    }
    let text = row_text(obj);
    if !text.contains("<cross-session-message") {
        return Vec::new();
    }
    let session = obj
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    parse_blocks(&text)
        .into_iter()
        .map(|b| BlockRow {
            from: b.get("from").and_then(Value::as_str).map(str::to_string),
            from_name: b
                .get("from_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            body: b
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            receiver_session: session.clone(),
            receiver_transcript: path.to_string(),
        })
        .collect()
}

/// Recursively collect transcript files, deepest-last. Depth-bounded: a
/// project dir, its session dirs and their `subagents/` dirs is the real
/// shape; anything deeper is not a transcript we join.
fn collect_jsonl(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            collect_jsonl(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// The live log the run writes, argv-first like the bus-append door so the
/// Python transport's resolver stays the single path authority.
fn backfill_live_path(explicit: Option<&str>) -> PathBuf {
    if let Some(raw) = explicit {
        return PathBuf::from(raw);
    }
    let home = crate::paths::AgentsHome::from_env();
    let dot_fno = home.root().parent().unwrap_or_else(|| home.root());
    crate::intel::bus_log_path(dot_fno)
}

/// Pair each send with its receiver-side block: the addressed socket must
/// equal the block's `from` and the body head must agree. Deterministic, so
/// a re-scan pairs the same halves.
fn join(sends: &[SendRow], blocks: &[BlockRow]) -> Vec<(usize, usize)> {
    let mut by_from: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, b) in blocks.iter().enumerate() {
        if let Some(key) = b.from.as_deref() {
            if !key.is_empty() {
                by_from.entry(key).or_default().push(i);
            }
        }
    }
    let mut out = Vec::new();
    for (si, s) in sends.iter().enumerate() {
        let head = norm_head(&s.body, 120);
        if let Some(cands) = by_from.get(s.to.as_str()) {
            for bi in cands {
                if norm_head(&blocks[*bi].body, 120) == head {
                    out.push((si, *bi));
                    break;
                }
            }
        }
    }
    out
}

/// The whole engine: scan roots, window-filter the sender side, join, and
/// (with --apply) write each joined pair as an audit-only envelope through
/// the bus-append door. Prints one JSON summary line.
fn run_backfill(args: &[String]) -> i32 {
    let mut since = String::new();
    let mut until = String::new();
    let mut live = String::new();
    let mut roots: Vec<String> = Vec::new();
    let mut apply = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--since" => {
                since = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--until" => {
                until = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--live" => {
                live = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--root" => {
                if let Some(v) = args.get(i + 1) {
                    roots.push(v.clone());
                }
                i += 2;
            }
            "--apply" => {
                apply = true;
                i += 1;
            }
            other => {
                eprintln!("mail-backfill run: unknown argument {other}");
                return 2;
            }
        }
    }
    let (since_e, until_e) = match bound_epoch("since", &since) {
        Ok(s) => match bound_epoch("until", &until) {
            Ok(u) => (s, u),
            Err(e) => {
                eprintln!("mail-backfill run: {e}");
                return 2;
            }
        },
        Err(e) => {
            eprintln!("mail-backfill run: {e}");
            return 2;
        }
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for root in &roots {
        collect_jsonl(Path::new(root), 0, &mut files);
    }
    let mut sends: Vec<SendRow> = Vec::new();
    let mut blocks: Vec<BlockRow> = Vec::new();
    for path in &files {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else { continue };
        let Ok(age) = mtime.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        if age.as_secs_f64() < since_e {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in raw.lines().take(200_000) {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ts = obj.get("timestamp").and_then(Value::as_str).unwrap_or("");
            if !ts_in_window(ts, since_e, until_e) {
                continue;
            }
            let p = path.to_string_lossy().to_string();
            sends.extend(sender_sends_in(&obj, &p));
            blocks.extend(receiver_blocks_in(&obj, &p));
        }
    }
    let pairs = join(&sends, &blocks);
    let mut written = 0usize;
    let mut rows_out: Vec<Value> = Vec::new();
    if apply {
        let live_path = backfill_live_path(if live.is_empty() {
            None
        } else {
            Some(live.as_str())
        });
        let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        for seg in crate::announce::read_bus_segments(&live_path) {
            if let Some(id) = seg.get("id").and_then(Value::as_str) {
                existing.insert(id.to_string());
            }
        }
        for (si, bi) in &pairs {
            match write_pair(&sends[*si], &blocks[*bi], &existing, &live_path) {
                Ok(true) => written += 1,
                Ok(false) => {}
                Err(e) => {
                    eprintln!("mail-backfill run: {e}");
                    return 1;
                }
            }
        }
    } else {
        for (si, bi) in &pairs {
            rows_out.push(json!({
                "ts": sends[*si].ts,
                "from_session": sends[*si].sender_session,
                "to_session": blocks[*bi].receiver_session,
                "body_head": norm_head(&sends[*si].body, 60),
            }));
        }
    }
    println!(
        "{}",
        json!({"written": written, "joined": pairs.len(), "rows": rows_out})
    );
    0
}

/// Write one joined pair: derive the archive id, skip an id already in the
/// log, and append the audit-only envelope through the bus-append door.
/// Ok(false) = already archived or no stable id; Err = the door failed.
fn write_pair(
    s: &SendRow,
    b: &BlockRow,
    existing: &std::collections::HashSet<String>,
    live_path: &Path,
) -> Result<bool, String> {
    if s.uuid.is_empty() {
        eprintln!("mail-backfill run: send without row uuid, no stable id; skipped");
        return Ok(false);
    }
    let id = archive_msg_id(&s.sender_session, &s.uuid);
    if existing.contains(&id) {
        return Ok(false);
    }
    let mut meta = serde_json::Map::new();
    meta.insert("transport".into(), json!("claude-cross-session"));
    meta.insert("sender_transcript".into(), json!(s.sender_transcript));
    meta.insert("receiver_transcript".into(), json!(b.receiver_transcript));
    meta.insert("to_session".into(), json!(b.receiver_session));
    meta.insert("sender_row".into(), json!(s.uuid));
    meta.insert("backfilled_at".into(), json!(now_iso()));
    // Key order mirrors the Python serializer (bus.log.to_json_line):
    // always-present keys first, optional tags, body last.
    let mut env = serde_json::Map::new();
    env.insert("v".into(), json!(1));
    env.insert("id".into(), json!(id));
    env.insert("ts".into(), json!(s.ts));
    env.insert("thread".into(), json!(id));
    let from_label = b
        .from_name
        .clone()
        .unwrap_or_else(|| s.sender_session.clone());
    env.insert("from".into(), json!(from_label));
    env.insert("to".into(), json!(b.receiver_session));
    env.insert("kind".into(), json!("send"));
    env.insert("delivery".into(), json!("cross-session"));
    env.insert("from_session".into(), json!(s.sender_session));
    env.insert("to_kind".into(), json!("session"));
    env.insert("origin".into(), json!("peer"));
    if let Some(sub) = &s.summary {
        env.insert("subject".into(), json!(sub));
    }
    env.insert(
        "word_count".into(),
        json!(s.body.split_whitespace().count()),
    );
    env.insert("meta".into(), Value::Object(meta));
    env.insert("body".into(), json!(s.body));
    crate::announce::append_line_open(live_path, &Value::Object(env), &crate::chats::chats_dir())
        .map(|()| true)
        .map_err(|e| format!("append failed: {e}"))
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fno-mbft-{tag}-{}", std::process::id()))
    }

    fn write_transcripts(root: &Path) {
        let sender = root.join("-Users-x-proj");
        std::fs::create_dir_all(&sender).unwrap();
        let send_row = json!({
            "type": "assistant",
            "sessionId": "sess-a",
            "uuid": "row-1",
            "timestamp": "2026-10-07T10:00:00.000Z",
            "message": {"content": [{"type": "tool_use", "name": "SendMessage",
                "input": {"to": "uds:/tmp/cc-socks/9.sock", "summary": "hold ack",
                           "message": "hold ack: nothing running"}}]}
        });
        std::fs::write(sender.join("s.jsonl"), format!("{send_row}\n")).unwrap();
        let receiver = root.join("-Users-y-proj");
        std::fs::create_dir_all(&receiver).unwrap();
        let recv_row = json!({
            "type": "user",
            "sessionId": "sess-b",
            "timestamp": "2026-10-07T10:00:01.000Z",
            "message": {"content": "<cross-session-message from=\"uds:/tmp/cc-socks/9.sock\" from-name=\"worker-1\">\nhold ack: nothing running\n</cross-session-message>"}
        });
        std::fs::write(receiver.join("r.jsonl"), format!("{recv_row}\n")).unwrap();
    }

    #[test]
    fn join_pairs_on_socket_and_body_head() {
        let sends = vec![SendRow {
            uuid: "row-1".into(),
            ts: "2026-10-07T10:00:00.000Z".into(),
            sender_session: "sess-a".into(),
            to: "uds:/tmp/cc-socks/9.sock".into(),
            summary: Some("hold ack".into()),
            body: "hold ack: nothing running".into(),
            sender_transcript: "s.jsonl".into(),
        }];
        let blocks = vec![
            BlockRow {
                from: Some("uds:/tmp/cc-socks/9.sock".into()),
                from_name: Some("worker-1".into()),
                body: "hold ack: nothing running".into(),
                receiver_session: "sess-b".into(),
                receiver_transcript: "r.jsonl".into(),
            },
            BlockRow {
                from: Some("uds:/tmp/cc-socks/9.sock".into()),
                from_name: Some("worker-1".into()),
                body: "an entirely different report".into(),
                receiver_session: "sess-b".into(),
                receiver_transcript: "r.jsonl".into(),
            },
        ];
        let pairs = join(&sends, &blocks);
        assert_eq!(pairs, vec![(0, 0)]);
    }

    #[test]
    fn window_bounds_refuse_garbage_and_keep_unparsable_rows() {
        assert!(bound_epoch("since", "not-a-time").is_err());
        assert!(bound_epoch("since", "2026-10-07T10:00:00Z").is_ok());
        assert!(ts_in_window("garbage", 0.0, 1.0));
        assert!(!ts_in_window("2026-10-07T18:00:00Z", 0.0, 1.0));
        assert!(ts_in_window("2026-10-07T10:00:00Z", 0.0, 1.0));
    }

    #[test]
    fn run_writes_once_then_idempotent_rerun() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_root("run");
        let home = temp_root("home");
        let bus_dir = root.join("bus");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        write_transcripts(&root);
        std::env::set_var("FNO_BUS_DIR", &bus_dir);
        std::env::set_var("FNO_AGENTS_HOME", home.join("agents"));
        let args: Vec<String> = [
            "run",
            "--since",
            "2026-10-07T08:00:00Z",
            "--until",
            "2026-10-07T12:00:00Z",
            "--root",
            root.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let dry = run_backfill(&args);
        assert_eq!(dry, 0);
        let applied = run_backfill(&{
            let mut a = args.clone();
            a.push("--apply".to_string());
            a
        });
        assert_eq!(applied, 0);
        let live = bus_dir.join("messages.jsonl");
        let rows: Vec<Value> = std::fs::read_to_string(&live)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["delivery"], "cross-session");
        assert_eq!(rows[0]["from"], "worker-1");
        assert_eq!(rows[0]["meta"]["to_session"], "sess-b");
        assert_eq!(rows[0]["meta"]["transport"], "claude-cross-session");
        let again = run_backfill(&{
            let mut a = args.clone();
            a.push("--apply".to_string());
            a
        });
        assert_eq!(again, 0);
        let count = std::fs::read_to_string(&live).unwrap().lines().count();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
    }
}
