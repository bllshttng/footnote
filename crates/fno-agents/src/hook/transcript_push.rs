//! `fno-agents hook transcript-push` - the sandboxed-session transcript lane's
//! producer. A Stop hook fires with `{session_id, transcript_path}`; the
//! producer reads the transcript's new bytes since its cursor, posts them as
//! `agent.transcript-append`, and advances the cursor only on a daemon ack.
//!
//! Fire-and-forget like `session-state`: every failure inside is silent and
//! exits 0, so a turn never reds because the lane is down. The cursor lives
//! under the platform temp dir keyed by the transcript path, never in the
//! repo: a sandboxed session's worktree gets committed and pruned, and a
//! cursor in it would ship to git.

use super::*;
use serde_json::Value;

/// Per-fire byte bound: at most this much new transcript text posts per fire.
/// The remainder waits for the next fire, so a first fire on a long session
/// cannot build an unbounded RPC.
const MAX_PUSH_BYTES: u64 = 512 * 1024;

/// Per-fire record bound: the op commits one store transaction per record,
/// so the fire size also bounds the store work one turn hook can spend. The
/// cursor advances only over pushed lines; the remainder rides the next fire.
const MAX_PUSH_LINES: usize = 128;

/// The cursor path for one transcript: `<tmp>/fno-transcript-push/<16hex>.cursor`,
/// named by the transcript path so a rotated transcript starts fresh.
fn cursor_path(transcript: &Path) -> PathBuf {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(transcript.to_string_lossy().as_bytes());
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    std::env::temp_dir()
        .join("fno-transcript-push")
        .join(format!("{}.cursor", &digest[..16]))
}

/// The transcript's new complete lines since the cursor offset, and the next
/// offset. Bounded by [`MAX_PUSH_BYTES`]; the offset only advances over
/// complete lines, so a torn last line waits for the next fire. A shrunk file
/// (rotation) resets the cursor: next fire reads the new file from 0.
fn new_lines_since(transcript: &Path, from: u64) -> Option<(Vec<String>, u64)> {
    use std::io::{Read as _, Seek, SeekFrom};
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    if len <= from {
        // A shrunk file (rotation) restarts the read from 0: the offset the
        // caller receives is the CURSOR, and run() writes it back on ack.
        let next = if len < from { 0 } else { from };
        return Some((Vec::new(), next));
    }
    let end = len.min(from + MAX_PUSH_BYTES);
    let _ = file.seek(SeekFrom::Start(from));
    let mut buf = vec![0u8; (end - from) as usize];
    file.read_exact(&mut buf).ok()?;
    Some(complete_lines(&buf, from))
}

/// The complete lines of `buf`, and the offset after the last one pushed.
/// Bytes after the last newline stay unread (a torn line, or a multibyte
/// char split by the window bound: a newline byte is never part of one), so
/// the next fire re-reads them from the same offset. The record cap stops
/// the offset mid-window: the remainder rides the next fire. A prefix that
/// is not valid UTF-8 reads as nothing-new and retries: corrupting a line
/// to advance is worse than stalling, and the file-fallback readers still
/// answer.
fn complete_lines(buf: &[u8], from: u64) -> (Vec<String>, u64) {
    let Some(nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return (Vec::new(), from);
    };
    let Ok(text) = std::str::from_utf8(&buf[..nl]) else {
        return (Vec::new(), from);
    };
    let mut lines = Vec::new();
    let mut consumed = 0usize;
    for raw in text.split_inclusive('\n') {
        if lines.len() >= MAX_PUSH_LINES {
            break;
        }
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.trim().is_empty() {
            lines.push(line.to_string());
        }
        consumed += raw.len();
    }
    (lines, from + consumed as u64)
}

/// The cursor offset for one transcript (0 when absent or malformed).
fn read_cursor(transcript: &Path) -> u64 {
    std::fs::read_to_string(cursor_path(transcript))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Write the cursor, creating the temp root first. Best-effort.
fn write_cursor(transcript: &Path, offset: u64) {
    if let Some(parent) = cursor_path(transcript).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(cursor_path(transcript), offset.to_string());
}

/// Post the lines to the daemon and answer whether it acked. Daemon down
/// reads as not-acked: the cursor stays, the next fire retries.
fn post_lines(home: &crate::paths::AgentsHome, session_id: &str, lines: &[String]) -> bool {
    if lines.is_empty() {
        return true;
    }
    let records: Vec<Value> = lines.iter().map(|line| json!({ "line": line })).collect();
    let req = crate::protocol::Request::new(
        1,
        "agent.transcript-append",
        json!({ "session_id": session_id, "records": records }),
    );
    matches!(
        block_on(crate::client::call_if_running(home, &req)),
        Ok(Ok(_))
    )
}

/// A tiny block-on for hook processes: main() dispatches hooks before the
/// tokio runtime builds, so the producer builds its own current-thread one.
fn block_on<F: std::future::Future>(
    fut: F,
) -> Option<Result<crate::protocol::Response, crate::client::ClientError>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .map(|rt| rt.block_on(fut))
}

/// The hook entry: parse the payload, push the delta, advance the cursor on
/// ack. Always exit 0.
pub fn run(_args: &[String]) -> i32 {
    let payload: Value = match serde_json::from_str(&crate::hook::read_stdin()) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let Some(session_id) = payload.get("session_id").and_then(Value::as_str) else {
        return 0;
    };
    let Some(transcript) = payload.get("transcript_path").and_then(Value::as_str) else {
        return 0;
    };
    let transcript = PathBuf::from(transcript);
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return 0;
    };
    let from = read_cursor(&transcript);
    let Some((lines, next)) = new_lines_since(&transcript, from) else {
        return 0; // unreadable transcript: retry next fire
    };
    if post_lines(&home, session_id, &lines) {
        write_cursor(&transcript, next);
    }
    0
}
