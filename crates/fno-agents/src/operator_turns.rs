//! The operator turn queue: which of this session's operator turns are still
//! undispositioned?
//!
//! The transcript is the ingest source, not the system of record: every turn
//! is held here as it is seen, keyed by turn id, and the ack ledger
//! (`<capture-dir>/<session>.jsonl`, written by `fno inbox user ack`)
//! disposes them. A per-session scan cursor
//! (`<capture-dir>/<session>.scan.json`, a deletable cache) remembers how far
//! the last read got, so no turn is lost to distance from EOF and later reads
//! parse only appended bytes.
//!
//! The reader rides the `compaction` action (as its `operator-turns`
//! sub-action) because the client action list may only shrink, and compaction
//! already reads one session's transcript and owns session-keyed state files.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;

// The classifier is the promoted `provenance` module: same functions, same
// reason names, so the queue's skip counters and its 17 regression tests are
// unchanged by the move.
use crate::provenance::{classify, is_user_turn, turn_text, turn_ts_epoch};

/// A held turn in the scan cursor; the payload adds the rendered excerpt.
#[derive(Debug, Clone, Serialize)]
struct StoredTurn {
    turn_id: String,
    ts_epoch: Option<f64>,
    text: String,
}

#[derive(Debug, Serialize)]
struct QueuePayload {
    depth: usize,
    oldest_age_s: Option<i64>,
    oldest_excerpt: Option<String>,
    oldest_turn_id: Option<String>,
    skipped: BTreeMap<String, u64>,
    turns: Vec<PayloadTurn>,
    cursor_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct PayloadTurn {
    turn_id: String,
    ts_epoch: Option<f64>,
    text: String,
    excerpt: String,
    stand_down: bool,
}

struct QueueOutcome {
    payload: QueuePayload,
    /// Exact stderr lines the CLI entry echoes.
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ScanState {
    transcript: String,
    offset: u64,
    line_no: u64,
    head_sha256: String,
    turns: Vec<StoredTurn>,
    skipped: BTreeMap<String, u64>,
}

/// The excerpt width. Retained beside the cursor machinery the queue owns.
const EXCERPT_CHARS: usize = 160;
const HEAD_SAMPLE_BYTES: u64 = 4096;

fn scan_path(capture_dir: &Path, session: &str) -> PathBuf {
    capture_dir.join(format!("{session}.scan.json"))
}

fn ledger_path(capture_dir: &Path, session: &str) -> PathBuf {
    capture_dir.join(format!("{session}.jsonl"))
}

fn stand_down_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)overstay|stand(ing)?[ -]down|step(ping)?[ -]down|abdicat|(end|stop) (the|your|this) reign|compact\w* (\w+ ){0,3}(degraded|diminish)",
        )
        .expect("valid pattern")
    })
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 over the first `min(offset, 4096)` bytes of `prefix`, the head
/// sample read once per invocation: the rotation tripwire. An in-place
/// rewrite at the same or larger size invalidates the cursor.
fn head_digest(prefix: &[u8], offset: u64) -> String {
    let cut = (offset as usize).min(prefix.len());
    let mut h = Sha256::new();
    h.update(&prefix[..cut]);
    to_hex(&h.finalize())
}

/// A stable ledger id: the row's own uuid/id, else a digest. The digest folds
/// the row's line number in, so byte-identical duplicate rows still get
/// distinct ids and one ack can never dispose two turns. sha1 (not sha256)
/// keeps every derived id an existing ledger already acked, and the absent
/// timestamp renders as the literal `None` exactly as the Python seed did.
fn turn_id(obj: &Value, text: &str, line_no: usize) -> String {
    for key in ["uuid", "id"] {
        if let Some(Value::String(s)) = obj.get(key) {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    let ts = match obj.get("timestamp") {
        Some(Value::String(s)) => s.clone(),
        _ => "None".to_string(),
    };
    let mut h = Sha1::new();
    h.update(format!("{line_no}:{ts}:{text}").as_bytes());
    let hex = to_hex(&h.finalize());
    format!("derived-{}", &hex[..12])
}

/// One-line excerpt; whitespace runs collapse so a row stays one row.
fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT_CHARS {
        flat
    } else {
        let cut: String = flat.chars().take(EXCERPT_CHARS - 1).collect();
        format!("{cut}\u{2026}")
    }
}

fn fresh_state(transcript: &Path) -> ScanState {
    ScanState {
        transcript: transcript.display().to_string(),
        offset: 0,
        line_no: 0,
        head_sha256: String::new(),
        turns: Vec::new(),
        skipped: BTreeMap::new(),
    }
}

/// The saved cursor, else fresh: missing, invalid, wrong-typed, alien,
/// past-EOF, head-mismatched, or head-less all reset. `head_prefix` is the
/// transcript's first `min(size, 4096)` bytes, read once by the caller.
fn load_state(
    path: &Path,
    transcript_str: &str,
    head_prefix: &[u8],
    transcript_len: u64,
) -> ScanState {
    let fresh = fresh_state(Path::new(transcript_str));
    let Ok(raw) = std::fs::read_to_string(path) else {
        return fresh;
    };
    let Ok(saved) = serde_json::from_str::<Value>(&raw) else {
        return fresh;
    };
    let Some(obj) = saved.as_object() else {
        return fresh;
    };
    if obj.get("transcript").and_then(|v| v.as_str()) != Some(transcript_str) {
        return fresh;
    }
    let Some(offset) = obj.get("offset").and_then(|v| v.as_u64()) else {
        return fresh;
    };
    if offset > transcript_len {
        return fresh;
    }
    let Some(line_no) = obj.get("line_no").and_then(|v| v.as_u64()) else {
        return fresh;
    };
    let Some(Value::Array(raw_turns)) = obj.get("turns") else {
        return fresh;
    };
    let Some(Value::Object(raw_skipped)) = obj.get("skipped") else {
        return fresh;
    };
    let Some(head) = obj.get("head_sha256").and_then(|v| v.as_str()) else {
        return fresh;
    };
    if head != head_digest(head_prefix, offset) {
        return fresh;
    }
    let mut skipped = BTreeMap::new();
    for (k, v) in raw_skipped {
        if let Some(n) = v.as_u64() {
            skipped.insert(k.clone(), n);
        }
    }
    let turns = raw_turns
        .iter()
        .filter_map(|t| {
            let o = t.as_object()?;
            let turn_id = o.get("turn_id")?.as_str()?.to_string();
            Some(StoredTurn {
                turn_id,
                ts_epoch: o.get("ts_epoch").and_then(|v| v.as_f64()),
                text: o
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    ScanState {
        transcript: transcript_str.to_string(),
        offset,
        line_no,
        head_sha256: head.to_string(),
        turns,
        skipped,
    }
}

/// Atomic best-effort save; failure costs speed on the next read, never
/// correctness.
fn save_state(path: &Path, state: &ScanState) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let body = serde_json::to_string(state).map_err(|e| format!("state encode failed: {e}"))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("scan"),
        std::process::id()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename into place: {e}"))
}

/// Every line of the ack ledger that parses as an object with a string
/// `turn_id`; an unreadable ledger is an empty set.
fn read_acked_turn_ids(path: &Path) -> HashSet<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    let mut acked = HashSet::new();
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = row.get("turn_id").and_then(|v| v.as_str()) {
            acked.insert(id.to_string());
        }
    }
    acked
}

fn build_payload(state: ScanState, now_epoch: f64, cursor_error: Option<String>) -> QueuePayload {
    let turns: Vec<PayloadTurn> = state
        .turns
        .iter()
        .map(|t| PayloadTurn {
            turn_id: t.turn_id.clone(),
            ts_epoch: t.ts_epoch,
            text: t.text.clone(),
            excerpt: excerpt(&t.text),
            stand_down: stand_down_re().is_match(&t.text),
        })
        .collect();
    let oldest = state.turns.first();
    QueuePayload {
        depth: turns.len(),
        oldest_age_s: oldest.and_then(|t| t.ts_epoch.map(|ts| ((now_epoch - ts) as i64).max(0))),
        oldest_excerpt: oldest.map(|t| excerpt(&t.text)),
        oldest_turn_id: oldest.map(|t| t.turn_id.clone()),
        skipped: state.skipped,
        turns,
        cursor_error,
    }
}

/// The pending turns (oldest first) and skip tally. # ponytail: the first
/// read is one full pass (1.07s at 220MB) with no mid-scan checkpoint -
/// checkpoint every 64MB in the loop if a transcript ever nears the hook's
/// 10s budget. A race is safe: two readers starting from the same offset save
/// in either order, and each saved state holds every turn in `[0, offset)`,
/// so a stale save only makes the next read parse more bytes.
fn read_queue(
    session: &str,
    transcript: &Path,
    capture_dir: &Path,
    now_epoch: f64,
) -> Result<QueueOutcome, String> {
    let unreadable =
        |e: std::io::Error| format!("transcript {} unreadable: {e}", transcript.display());
    let mut file = std::fs::File::open(transcript).map_err(unreadable)?;
    let size = file.metadata().map_err(unreadable)?.len();
    // One head sample serves every rotation check this read makes.
    let want = size.min(HEAD_SAMPLE_BYTES) as usize;
    let mut head = vec![0u8; want];
    file.read_exact(&mut head).map_err(unreadable)?;
    let spath = scan_path(capture_dir, session);
    let transcript_str = transcript.display().to_string();
    let mut state = load_state(&spath, &transcript_str, &head, size);
    file.seek(SeekFrom::Start(state.offset))
        .map_err(unreadable)?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw).map_err(unreadable)?;
    if let Some(cut) = raw.iter().rposition(|&b| b == b'\n') {
        let kept = &raw[..cut + 1];
        let rows: Vec<&[u8]> = kept[..cut].split(|&b| b == b'\n').collect();
        let known: HashSet<String> = state.turns.iter().map(|t| t.turn_id.clone()).collect();
        for (i, row) in rows.iter().enumerate() {
            let Ok(obj) = serde_json::from_str::<Value>(&String::from_utf8_lossy(row)) else {
                continue;
            };
            if !obj.is_object() || !is_user_turn(&obj) {
                continue;
            }
            let text = match classify(&turn_text(&obj)) {
                Ok(t) => t,
                Err(reason) => {
                    *state.skipped.entry(reason.to_string()).or_insert(0) += 1;
                    continue;
                }
            };
            let id = turn_id(&obj, &text, state.line_no as usize + i);
            if known.contains(&id) {
                continue;
            }
            state.turns.push(StoredTurn {
                turn_id: id,
                ts_epoch: turn_ts_epoch(&obj),
                text,
            });
        }
        state.offset += kept.len() as u64;
        state.line_no += rows.len() as u64;
    }
    // A torn trailing row waits for its newline above, so a read never parses
    // a half-written row and derived turn ids stay stable across reads.
    let acked = read_acked_turn_ids(&ledger_path(capture_dir, session));
    state.turns.retain(|t| !acked.contains(&t.turn_id));
    state.head_sha256 = head_digest(&head, state.offset);
    let mut warnings = Vec::new();
    let mut cursor_error = None;
    if let Err(e) = save_state(&spath, &state) {
        cursor_error = Some(format!("{}: {e}", spath.display()));
        warnings.push(format!(
            "fno-agents compaction operator-turns: scan cursor not saved at {}: {e}",
            spath.display()
        ));
    }
    Ok(QueueOutcome {
        payload: build_payload(state, now_epoch, cursor_error),
        warnings,
    })
}

pub(crate) fn pending_stand_down(
    session: &str,
    transcript: &Path,
    capture_dir: &Path,
    now_epoch: f64,
) -> Result<Vec<(String, String)>, String> {
    let outcome = read_queue(session, transcript, capture_dir, now_epoch)?;
    Ok(outcome
        .payload
        .turns
        .into_iter()
        .filter(|turn| turn.stand_down)
        .map(|turn| (turn.turn_id, turn.excerpt))
        .collect())
}

pub(crate) fn capture_dir(cwd: &Path) -> Option<PathBuf> {
    std::env::var_os("FNO_OPERATOR_CAPTURE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::agents_config::state_dir(cwd).map(|dir| dir.join("operator-capture")))
}

pub(crate) fn session_queue_depth(
    get: &impl Fn(&str) -> Option<String>,
    home: &crate::paths::AgentsHome,
    cwd: &Path,
    now_epoch: f64,
) -> Result<usize, String> {
    let session_pin = get("FNO_OPERATOR_SESSION_ID").filter(|value| !value.trim().is_empty());
    let harness_pin = get("FNO_OPERATOR_HARNESS").filter(|value| !value.trim().is_empty());
    let identity = (session_pin.is_none() || harness_pin.is_none())
        .then(|| crate::spawn_context::resolve_self_identity(get, None, None, home));
    let session = session_pin
        .or_else(|| {
            identity
                .as_ref()
                .and_then(|identity| identity.session_id.clone())
        })
        .ok_or_else(|| "no resolvable session identity".to_string())?;
    let harness = harness_pin
        .or_else(|| {
            identity
                .as_ref()
                .and_then(|identity| identity.harness.clone())
        })
        .unwrap_or_else(|| "claude".to_string());
    let transcript = if let Some(path) =
        get("FNO_OPERATOR_TRANSCRIPT").filter(|value| !value.trim().is_empty())
    {
        PathBuf::from(path)
    } else {
        match harness.as_str() {
            "claude" => crate::claude_drive::find_transcript(&session),
            "codex" => crate::codex_store::codex_rollout_path(None, &session),
            _ => return Err(format!("harness {harness} keeps no transcript file")),
        }
        .ok_or_else(|| format!("no transcript found for {harness} session {session}"))?
    };
    let capture_dir = get("FNO_OPERATOR_CAPTURE_DIR")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| capture_dir(cwd))
        .ok_or_else(|| "no operator capture directory could be resolved".to_string())?;
    read_queue(&session, &transcript, &capture_dir, now_epoch).map(|outcome| outcome.payload.depth)
}

/// CLI entry: `fno-agents compaction operator-turns --session <id>
/// --transcript <path> --capture-dir <dir>`. Session and transcript
/// resolution stay in Python; this binary resolves neither.
pub fn run(args: &[String]) -> i32 {
    let (Some(session), Some(transcript), Some(capture_dir)) = (
        crate::compaction::flag_value(args, "--session"),
        crate::compaction::flag_value(args, "--transcript"),
        crate::compaction::flag_value(args, "--capture-dir"),
    ) else {
        eprintln!("usage: compaction operator-turns --session <id> --transcript <path> --capture-dir <dir>");
        return 2;
    };
    // The id lands in a path join; a crafted id must not escape the capture dir.
    if session.contains('/') || session.contains('\\') || session.contains("..") {
        eprintln!("compaction operator-turns: --session must be a bare id (no '/', '\\', '..'): {session}");
        return 2;
    }
    match read_queue(
        &session,
        Path::new(&transcript),
        Path::new(&capture_dir),
        chrono::Utc::now().timestamp() as f64,
    ) {
        Ok(outcome) => {
            for line in &outcome.warnings {
                eprintln!("{line}");
            }
            println!(
                "{}",
                serde_json::to_string(&outcome.payload).unwrap_or_default()
            );
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// CLI entry: `fno-agents compaction ack --session <id> --turn <id>
/// --outcome <o> [--why <w>] --capture-dir <dir>`. Session resolution stays
/// in Python; this binary resolves nothing.
pub fn run_ack(args: &[String]) -> i32 {
    let (Some(session), Some(turn), Some(outcome), Some(capture_dir)) = (
        crate::compaction::flag_value(args, "--session"),
        crate::compaction::flag_value(args, "--turn"),
        crate::compaction::flag_value(args, "--outcome"),
        crate::compaction::flag_value(args, "--capture-dir"),
    ) else {
        eprintln!("usage: compaction ack --session <id> --turn <id> --outcome <o> [--why <w>] --capture-dir <dir>");
        return 2;
    };
    // The id lands in a path join; a crafted id must not escape the capture dir.
    if session.contains('/') || session.contains('\\') || session.contains("..") {
        eprintln!("compaction ack: --session must be a bare id (no '/', '\\', '..'): {session}");
        return 2;
    }
    let why = crate::compaction::flag_value(args, "--why").unwrap_or_default();
    let home = crate::paths::AgentsHome::from_env();
    match ack_turn(
        &home,
        Path::new(&capture_dir),
        &session,
        &turn,
        &outcome,
        &why,
    ) {
        Ok(row) => {
            println!("{}", serde_json::to_string(&row).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// The write behind `fno inbox user ack`. The kinds are law, capture, node
/// and answer; `nothing` stands alone. An answer ack also records one
/// `user_ask_answered` row in the machine question index, which is what the
/// attention arm folds into the file sink as a closed line.
pub fn ack_turn(
    home: &crate::paths::AgentsHome,
    capture_dir: &Path,
    session: &str,
    turn_id: &str,
    outcome: &str,
    why: &str,
) -> Result<Value, String> {
    const KINDS: [&str; 4] = ["law", "capture", "node", "answer"];
    let (kind, ref_part) = match outcome.trim().split_once(':') {
        Some((k, r)) => (k.trim().to_string(), r.trim().to_string()),
        None => (outcome.trim().to_string(), String::new()),
    };
    let outcome = if kind == "nothing" && ref_part.is_empty() {
        "nothing".to_string()
    } else if KINDS.contains(&kind.as_str()) && !ref_part.is_empty() {
        outcome.trim().to_string()
    } else {
        let legal = KINDS
            .iter()
            .map(|k| format!("{k}:<ref>"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "invalid --outcome {outcome:?}. Must be nothing or {legal}"
        ));
    };
    let row = json!({
        "turn_id": turn_id,
        "ts": chrono::Utc::now().to_rfc3339(),
        "outcome": outcome,
        "ref": if ref_part.is_empty() { None } else { Some(ref_part.clone()) },
        "why": if why.trim().is_empty() { None } else { Some(why.trim().to_string()) },
    });
    let path = ledger_path(capture_dir, session);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    writeln!(f, "{row}").map_err(|e| e.to_string())?;

    if kind == "answer" {
        let answered = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "type": "user_ask_answered",
            "source": "target",
            "data": {
                "session_id": session,
                "turn_id": turn_id,
                "excerpt": excerpt_of(capture_dir, session, turn_id),
                "answer": ref_part,
                "answered_at": chrono::Utc::now().to_rfc3339(),
            }
        });
        crate::provider_cap::append_questions_row(
            &crate::provider_cap::questions_path(home),
            &answered,
        );
    }
    Ok(row)
}

/// Best-effort: the scan cursor's stored text for the acked turn, cut to the
/// excerpt width. A rotated or never-scanned turn answers empty.
fn excerpt_of(capture_dir: &Path, session: &str, turn_id: &str) -> String {
    let Ok(raw) = std::fs::read_to_string(scan_path(capture_dir, session)) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return String::new();
    };
    v.get("turns")
        .and_then(Value::as_array)
        .and_then(|turns| {
            turns
                .iter()
                .find(|t| t.get("turn_id").and_then(Value::as_str) == Some(turn_id))
        })
        .and_then(|t| t.get("text").and_then(Value::as_str))
        .map(|text| text.chars().take(EXCERPT_CHARS).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::is_bare_command;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    /// A fixed "now": 2026-09-06T21:00:00Z, the fixture rows' timestamp.
    const NOW: f64 = 1788728400.0;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-operator-turns-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user_row(text: Value, uuid: &str) -> Value {
        user_row_ts(text, uuid, "2026-09-06T21:00:00.000Z")
    }

    fn user_row_ts(text: Value, uuid: &str, ts: &str) -> Value {
        json!({"type": "user", "uuid": uuid, "timestamp": ts,
               "message": {"role": "user", "content": text}})
    }

    fn write_jsonl(path: &Path, rows: &[Value]) {
        let body = rows
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(path, body).unwrap();
    }

    fn append(path: &Path, extra: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(extra.as_bytes()).unwrap();
    }

    fn read(dir: &Path, transcript: &Path) -> QueueOutcome {
        read_queue("s", transcript, dir, NOW).unwrap()
    }

    fn ids(out: &QueueOutcome) -> Vec<String> {
        out.payload
            .turns
            .iter()
            .map(|t| t.turn_id.clone())
            .collect()
    }

    fn read_raw(name: &str, rows: &[Value]) -> QueueOutcome {
        let dir = tmp_dir(name);
        let tp = dir.join("transcript.jsonl");
        write_jsonl(&tp, rows);
        read(&dir, &tp)
    }

    fn pinned_operator_turns(
        dir: &Path,
        transcript: &Path,
    ) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([
            ("FNO_OPERATOR_SESSION_ID".into(), "fixture-session".into()),
            ("FNO_OPERATOR_HARNESS".into(), "claude".into()),
            (
                "FNO_OPERATOR_TRANSCRIPT".into(),
                transcript.display().to_string(),
            ),
            ("FNO_OPERATOR_CAPTURE_DIR".into(), dir.display().to_string()),
        ])
    }

    #[test]
    fn session_queue_depth_reads_planted_turn_and_ack() {
        let dir = tmp_dir("session-depth");
        let transcript = dir.join("transcript.jsonl");
        write_jsonl(&transcript, &[user_row(json!("first ask"), "u-1")]);
        let vars = pinned_operator_turns(&dir, &transcript);
        let get = |key: &str| vars.get(key).cloned();
        let home = crate::paths::AgentsHome::at(&dir.join("home"));
        assert_eq!(session_queue_depth(&get, &home, &dir, NOW), Ok(1));
        std::fs::write(
            ledger_path(&dir, "fixture-session"),
            "{\"turn_id\":\"u-1\"}\n",
        )
        .unwrap();
        assert_eq!(session_queue_depth(&get, &home, &dir, NOW), Ok(0));
    }

    #[test]
    fn session_queue_depth_names_unreadable_transcript() {
        let dir = tmp_dir("session-depth-errors");
        let missing = dir.join("missing.jsonl");
        let vars = pinned_operator_turns(&dir, &missing);
        let home = crate::paths::AgentsHome::at(&dir.join("home"));
        let get = |key: &str| vars.get(key).cloned();
        assert!(session_queue_depth(&get, &home, &dir, NOW)
            .unwrap_err()
            .contains(&missing.display().to_string()));
    }

    #[test]
    fn session_queue_depth_names_unsupported_harness() {
        let dir = tmp_dir("session-depth-unsupported");
        let mut vars = pinned_operator_turns(&dir, &dir.join("unused.jsonl"));
        vars.insert("FNO_OPERATOR_HARNESS".into(), "opencode".into());
        vars.remove("FNO_OPERATOR_TRANSCRIPT");
        let home = crate::paths::AgentsHome::at(&dir.join("home"));
        let get = |key: &str| vars.get(key).cloned();
        assert!(session_queue_depth(&get, &home, &dir, NOW)
            .unwrap_err()
            .contains("opencode"));
    }

    #[test]
    fn naive_timestamp_reads_as_utc_not_local() {
        let out = read_raw(
            "naive",
            &[user_row_ts(
                json!("ask"),
                "u-naive",
                "2026-09-06T21:00:00.000000",
            )],
        );
        assert_eq!(out.payload.turns[0].ts_epoch, Some(1788728400.0));
        let out = read_raw(
            "naive-date-only",
            &[user_row_ts(json!("ask"), "u-date", "2026-09-06")],
        );
        assert_eq!(out.payload.turns[0].ts_epoch, Some(1788652800.0));
    }

    #[test]
    fn duplicate_rows_derive_distinct_ids() {
        let out = read_raw(
            "dupes",
            &[
                user_row(json!("same text"), ""),
                user_row(json!("same text"), ""),
            ],
        );
        assert_eq!(out.payload.depth, 2);
        assert_ne!(out.payload.turns[0].turn_id, out.payload.turns[1].turn_id);
    }

    #[test]
    fn ac1_turn_beyond_two_mb_still_queues() {
        let dir = tmp_dir("ac1");
        let pad: Value = json!({"type": "user", "uuid": "pad"});
        let pad_line = format!("{pad}\n").repeat(2_000_000 / pad.to_string().len() + 1);
        let body = format!(
            "{}\n{pad_line}{}\n",
            user_row_ts(
                json!("old turn beyond any window"),
                "u-old",
                "2026-09-01T00:00:00.000Z"
            ),
            user_row(json!("fresh turn near EOF"), "u-new")
        );
        let tp = dir.join("transcript.jsonl");
        std::fs::write(&tp, body).unwrap();
        let out = read(&dir, &tp);
        assert_eq!(ids(&out), ["u-old", "u-new"]);
    }

    #[test]
    fn ac2_scan_cursor_saves_state_and_second_read_stays_incremental() {
        let dir = tmp_dir("ac2");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(&tp, &[user_row(json!("first ask"), "u-1")]);
        let first = read(&dir, &tp);
        assert_eq!(ids(&first), ["u-1"]);
        let scan = scan_path(&dir, "s");
        assert!(scan.is_file());
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&scan).unwrap()).unwrap()
                ["offset"]
                .as_u64(),
            Some(tp.metadata().unwrap().len())
        );
        append(&tp, &format!("{}\n", user_row(json!("second ask"), "u-2")));
        let second = read(&dir, &tp);
        assert_eq!(ids(&second), ["u-1", "u-2"]);
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&scan).unwrap()).unwrap()
                ["offset"]
                .as_u64(),
            Some(tp.metadata().unwrap().len())
        );
    }

    #[test]
    fn ac3_scan_state_resets_on_invalid_alien_past_eof_or_rotation() {
        let dir = tmp_dir("ac3");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(&tp, &[user_row(json!("ask"), "u-1")]);
        let fresh = read(&dir, &tp);
        let scan = scan_path(&dir, "s");

        let matches_baseline = |out: &QueueOutcome, baseline: &QueueOutcome| {
            out.payload.depth == baseline.payload.depth
                && ids(out) == ids(baseline)
                && out.payload.skipped == baseline.payload.skipped
        };

        std::fs::write(&scan, "{not json").unwrap();
        assert!(matches_baseline(&read(&dir, &tp), &fresh));

        std::fs::write(
            &scan,
            serde_json::to_string(&json!({
                "transcript": "/elsewhere.jsonl", "offset": 5, "line_no": 1,
                "head_sha256": "x", "turns": [], "skipped": {}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(matches_baseline(&read(&dir, &tp), &fresh));

        std::fs::write(
            &scan,
            serde_json::to_string(&json!({
                "transcript": tp.display().to_string(),
                "offset": tp.metadata().unwrap().len() + 10, "line_no": 1,
                "head_sha256": "x", "turns": [], "skipped": {}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(matches_baseline(&read(&dir, &tp), &fresh));

        // Rotation in place: the transcript is rewritten at a larger size, so
        // the saved head sample no longer matches the file's bytes.
        append(
            &tp,
            &format!("{}\n", user_row(json!("post-rewrite"), "u-2")),
        );
        let _ = std::fs::remove_file(&scan);
        let baseline = read(&dir, &tp);
        let saved =
            serde_json::from_str::<Value>(&std::fs::read_to_string(&scan).unwrap()).unwrap();
        let mut rotated = saved.clone();
        rotated["offset"] = json!(1);
        std::fs::write(&scan, rotated.to_string()).unwrap();
        assert!(matches_baseline(&read(&dir, &tp), &baseline));
    }

    #[test]
    fn ac4_ack_prunes_the_saved_scan_state_and_depth_drops() {
        let dir = tmp_dir("ac4");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(
            &tp,
            &[
                user_row(json!("doom"), "u-1"),
                user_row(json!("kept"), "u-2"),
            ],
        );
        let first = read(&dir, &tp);
        assert_eq!(first.payload.depth, 2);
        std::fs::write(ledger_path(&dir, "s"), "{\"turn_id\": \"u-1\"}\n").unwrap();
        let second = read(&dir, &tp);
        assert_eq!(second.payload.depth, 1);
        assert_eq!(ids(&second), ["u-2"]);
        let scan: Value =
            serde_json::from_str(&std::fs::read_to_string(scan_path(&dir, "s")).unwrap()).unwrap();
        let held: Vec<&str> = scan["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["turn_id"].as_str().unwrap())
            .collect();
        assert_eq!(held, ["u-2"]);
    }

    #[test]
    fn ac5_torn_trailing_row_waits_for_its_newline() {
        let dir = tmp_dir("ac5");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(&tp, &[user_row(json!("whole"), "u-1")]);
        append(&tp, &user_row(json!("torn"), "u-2").to_string());
        let first = read(&dir, &tp);
        assert_eq!(ids(&first), ["u-1"]);
        assert!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(scan_path(&dir, "s")).unwrap())
                .unwrap()["offset"]
                .as_u64()
                .unwrap()
                < tp.metadata().unwrap().len()
        );
        append(&tp, "\n");
        let second = read(&dir, &tp);
        assert_eq!(ids(&second), ["u-1", "u-2"]);
    }

    #[test]
    fn ac6_derived_turn_id_stable_across_reads_and_matches_the_python_digest() {
        let dir = tmp_dir("ac6");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(
            &tp,
            &[
                json!({"type": "user", "timestamp": "2026-09-06T21:00:00.000Z",
                     "message": {"role": "user", "content": "no id here"}}),
            ],
        );
        let first = read(&dir, &tp);
        assert_eq!(first.payload.turns[0].turn_id, "derived-4ee369cad436");
        append(&tp, &format!("{}\n", user_row(json!("later"), "u-later")));
        let second = read(&dir, &tp);
        assert_eq!(second.payload.turns[0].turn_id, "derived-4ee369cad436");
    }

    #[test]
    fn ac7_unwritable_capture_dir_still_answers() {
        let dir = tmp_dir("ac7");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(&tp, &[user_row(json!("ask"), "u-1")]);
        let ro = dir.join("ro-capture");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Capture the Result before restoring, so a panic cannot leak 0o500.
        let out = read_queue("s", &tp, &ro, NOW);
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = out.unwrap();
        assert_eq!(out.payload.depth, 1);
        let err = out.payload.cursor_error.as_deref().unwrap_or_default();
        assert!(err.starts_with(&format!("{}: ", scan_path(&ro, "s").display())));
        assert!(out
            .warnings
            .iter()
            .any(|w| w
                .starts_with("fno-agents compaction operator-turns: scan cursor not saved at ")));
    }

    #[test]
    fn ac8_machine_shapes_never_queue_and_are_counted() {
        let out = read_raw("machine", &[
            user_row(
                json!("<task-notification><task-id>b55cj2z2z</task-id><output-file>/tmp/out</output-file></task-notification>"),
                "u-tn",
            ),
            user_row(
                json!("Another Claude session sent a message:\n<teammate-message teammate_id=\"t1\" color=\"blue\">{}</teammate-message>"),
                "u-tm",
            ),
            user_row(
                json!("This session is being continued from a previous conversation that ran out of context. The summary below covers the work."),
                "u-cp",
            ),
            user_row(json!("[Request interrupted by user]"), "u-int1"),
            user_row(json!("[Request interrupted by user for tool use]"), "u-int2"),
            user_row(json!("<bash-input>git status</bash-input>"), "u-bi"),
            user_row(json!("<bash-stdout>nothing to commit</bash-stdout>"), "u-bs"),
            user_row(
                json!("<command-message>fno:target</command-message>\n<command-name>/fno:target</command-name>"),
                "u-cm",
            ),
            user_row(json!("status on your nodes?"), "u-prose"),
        ]);
        assert_eq!(ids(&out), ["u-prose"]);
        let skipped = &out.payload.skipped;
        assert_eq!(skipped.get("task_notification"), Some(&1));
        assert_eq!(skipped.get("teammate_message"), Some(&1));
        assert_eq!(skipped.get("compaction_preamble"), Some(&1));
        assert_eq!(skipped.get("interrupt_marker"), Some(&2));
        assert_eq!(skipped.get("bash_echo"), Some(&2));
        assert_eq!(skipped.get("command_invocation"), Some(&1));

        // The status half: depth excludes the machine turn and names the skip.
        let dir = tmp_dir("ac8-status");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(
            &tp,
            &[
                user_row_ts(
                    json!("<task-notification><task-id>t9</task-id></task-notification>"),
                    "u-tn",
                    "2026-09-06T20:30:00.000Z",
                ),
                user_row(json!("status on your nodes?"), "u-real"),
            ],
        );
        let out = read(&dir, &tp);
        assert_eq!(out.payload.depth, 1);
        assert_eq!(out.payload.oldest_turn_id.as_deref(), Some("u-real"));
        assert_eq!(out.payload.skipped.get("task_notification"), Some(&1));
    }

    #[test]
    fn prose_turn_queues_and_mail_turn_does_not() {
        let out = read_raw(
            "prose",
            &[
                user_row(json!("please widen the review gate"), "u-prose-1"),
                user_row(
                    json!(["<fno_mail from=\"peer\" harness=\"claude\">run the sweep</fno_mail>"]),
                    "u-mail-1",
                ),
            ],
        );
        assert_eq!(ids(&out), ["u-prose-1"]);
    }

    #[test]
    fn bare_command_and_system_only_turns_never_queue() {
        let out = read_raw(
            "bare",
            &[
                user_row(json!("/fno:setup"), "u-cmd-1"),
                user_row(json!("$fno:review medium"), "u-cmd-2"),
                user_row(
                    json!([{"type": "text", "text": "<system-reminder>hook output</system-reminder>"}]),
                    "u-hook-1",
                ),
                user_row(
                    json!([{"type": "tool_result", "tool_use_id": "t1", "content": "out"}]),
                    "u-tool-1",
                ),
            ],
        );
        assert_eq!(ids(&out), Vec::<String>::new());
    }

    #[test]
    fn command_with_following_prose_still_queues() {
        let out = read_raw(
            "prose-arg",
            &[user_row(
                json!("/fno:target x-1. A plan already exists at /tmp/plan.md, execute it"),
                "u-arg-1",
            )],
        );
        assert_eq!(ids(&out), ["u-arg-1"]);
    }

    #[test]
    fn bare_command_reads_both_sigils_and_rejects_paths() {
        assert!(is_bare_command("$target x-1"));
        assert!(is_bare_command("/fno:target x-1"));
        assert!(!is_bare_command("/Users/bb16/plan.md"));
    }

    #[test]
    fn codex_payload_row_queues() {
        let out = read_raw(
            "codex",
            &[json!({
                "payload": {"type": "message", "role": "user", "content": "from codex"},
                "timestamp": "2026-09-06T21:00:00.000Z"
            })],
        );
        assert_eq!(out.payload.depth, 1);
        assert_eq!(out.payload.turns[0].text, "from codex");
    }

    #[test]
    fn stand_down_turn_is_marked_in_the_payload() {
        let out = read_raw(
            "stand-down-payload",
            &[user_row(
                json!("Perhaps our reign has overstayed its welcome"),
                "u-stand-down",
            )],
        );
        let payload = serde_json::to_value(&out.payload).unwrap();
        assert_eq!(payload["turns"][0]["stand_down"], true);
    }

    #[test]
    fn ordinary_handoff_and_standup_turns_are_not_marked() {
        let out = read_raw(
            "not-stand-down",
            &[
                user_row(json!("hand off this doc to codex"), "u-handoff"),
                user_row(json!("fix the stand-up notes"), "u-standup"),
            ],
        );
        assert!(out.payload.turns.iter().all(|turn| !turn.stand_down));
    }

    #[test]
    fn acked_stand_down_turn_is_not_pending() {
        let dir = tmp_dir("stand-down-acked");
        let tp = dir.join("transcript.jsonl");
        write_jsonl(
            &tp,
            &[user_row(json!("the reign has overstayed"), "u-acked")],
        );
        std::fs::write(ledger_path(&dir, "s"), "{\"turn_id\":\"u-acked\"}\n").unwrap();
        assert!(pending_stand_down("s", &tp, &dir, NOW).unwrap().is_empty());
    }

    #[test]
    fn is_meta_row_does_not_queue() {
        let out = read_raw(
            "meta",
            &[json!({
                "type": "user", "isMeta": true, "uuid": "u-meta",
                "message": {"role": "user", "content": "meta"}
            })],
        );
        assert_eq!(out.payload.depth, 0);
    }

    #[test]
    fn run_flag_validation_exits_two() {
        assert_eq!(run(&[]), 2);
        assert_eq!(
            run(&[
                "--session".to_string(),
                "a/b".to_string(),
                "--transcript".to_string(),
                "t".to_string(),
                "--capture-dir".to_string(),
                "d".to_string(),
            ]),
            2
        );
        assert_eq!(
            run(&[
                "--session".to_string(),
                "s".to_string(),
                "--transcript".to_string(),
                "t".to_string(),
            ]),
            2
        );
    }

    #[test]
    fn ac14_hp_answer_ack_records_the_answered_row() {
        let dir = tmp_dir("ac14-hp");
        let home = crate::paths::AgentsHome::at(&dir.join("home"));
        let capture = dir.join("capture");
        std::fs::create_dir_all(&capture).unwrap();
        let row = ack_turn(
            &home,
            &capture,
            "s",
            "u-1",
            "answer:use the narrow reading",
            "",
        )
        .unwrap();
        assert_eq!(row["outcome"], "answer:use the narrow reading");
        assert_eq!(row["ref"], "use the narrow reading");
        // The durable answered row: the fold the attention arm reads.
        let index =
            std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
        assert!(
            index.contains("\"user_ask_answered\""),
            "index carries the answered row: {index}"
        );
        assert!(index.contains("use the narrow reading"));
    }

    #[test]
    fn ac14_err_empty_answer_ref_refuses_and_writes_nothing() {
        let dir = tmp_dir("ac14-err");
        let home = crate::paths::AgentsHome::at(&dir.join("home"));
        let capture = dir.join("capture");
        std::fs::create_dir_all(&capture).unwrap();
        let err = ack_turn(&home, &capture, "s", "u-1", "answer:", "").unwrap_err();
        assert!(err.contains("invalid --outcome"), "{err}");
        assert!(
            !capture.join("s.jsonl").exists(),
            "the refusal writes no ledger"
        );
        let index =
            std::fs::read_to_string(crate::provider_cap::questions_path(&home)).unwrap_or_default();
        assert!(
            !index.contains("user_ask_answered"),
            "the refusal writes no answered row: {index}"
        );
    }
}
