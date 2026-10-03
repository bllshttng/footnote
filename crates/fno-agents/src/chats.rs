//! chats.rs - the conversation record plane for agent mail.
//!
//! The bus stays the delivery plane exactly as it is: send, hold, inject,
//! cursors, receipts, rotation. This module is the durable record beside it:
//! one append-only messages.jsonl per chat under `paths.chats` (default
//! `<state_dir>/chats`), plus a derived SQLite index at
//! `<state_dir>/db/chats.db` that is rebuilt from the JSONL whenever lost or
//! stale and is never authoritative (user ruling of 2026-10-01).
//!
//! Two record line types:
//!   {"type":"message",  "chat_id":..., ...the bus envelope fields verbatim}
//!   {"type":"delivery", "chat_id":..., "id":<msg id>, "session":<receiver>,
//!    "how":"hosted|typed|landed", "ts":<delivered ts>}
//!
//! Sent time is the bus row's ts, set once at the bus write (the epic's
//! held-mail ruling); delivered time rides the delivery line; held time is
//! stored nowhere.
//!
//! The index is a rollup: chats rows (participants, created, last message,
//! count, last-line hash) for staleness checks and list reads, messages rows
//! for prefix resolution and delivery-line chat lookup. A lost or hand-edited
//! index is overwritten by the JSONL on the next read (AC4-ERR).
//!
//! chat_id derivation (plan R1): `chat-` + first 16 hex of SHA-256 over the
//! two participant keys sorted byte-wise and joined with \x1f; channels hash
//! `channel:\x1f<scope>` so a channel id is never pair-derived.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const CHAT_PREFIX: &str = "chat-";
const PARTICIPANT_SEP: char = '\u{1f}';
/// The bus kinds recorded as conversation messages. Everything else is
/// control traffic (withdraw, announce-wake) or a receipt (below).
pub(crate) const MESSAGE_KINDS: &[&str] = &["send", "announce"];
/// Per-chat lock bound; mirrors announce.rs LOCK_TIMEOUT.
const CHAT_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) fn is_message_kind(kind: &str) -> bool {
    MESSAGE_KINDS.contains(&kind)
}

fn hex16(digest: &[u8]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_parts(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            h.update(PARTICIPANT_SEP.to_string().as_bytes());
        }
        h.update(part.as_bytes());
    }
    let digest = h.finalize();
    hex16(&digest)
}

/// `chat-` + 16 hex over the pair, order-independent (AC3-HP).
pub(crate) fn chat_id_for_pair(a: &str, b: &str) -> String {
    let mut keys = [a, b];
    keys.sort_unstable();
    format!("{CHAT_PREFIX}{}", hash_parts(&keys))
}

/// `chat-` + 16 hex over the scope; never pair-derived (AC3-ERR).
pub(crate) fn chat_id_for_channel(scope: &str) -> String {
    format!("{CHAT_PREFIX}{}", hash_parts(&["channel", scope]))
}
// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// The chats dir: `config.paths.chats` from the config ladder, else
/// `<state_dir>/chats` (which is `~/.fno/chats` on a stock machine). The
/// override may point at a synced folder (the vault); the index never follows
/// it there (node requirement 3).
pub(crate) fn chats_dir() -> PathBuf {
    if let Some(raw) = config_chats_override() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return crate::backlog::settings::expand_home(trimmed);
        }
    }
    let mut dir = crate::backlog::settings::state_dir().unwrap_or_else(default_dot_fno);
    dir.push("chats");
    dir
}

/// The derived index. Always under the state dir, never under the (possibly
/// synced) chats dir.
pub(crate) fn index_path() -> PathBuf {
    let mut dir = crate::backlog::settings::state_dir().unwrap_or_else(default_dot_fno);
    dir.push("db");
    dir.push("chats.db");
    dir
}

fn default_dot_fno() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".fno")
}

/// `paths.chats` from `<cwd>/.fno/config.toml` then `<home>/.fno/config.toml`,
/// the same ladder finalize.rs resolves its paths overrides from.
fn config_chats_override() -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".fno/config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".fno/config.toml"));
    }
    candidates
        .iter()
        .find_map(|p| crate::finalize::read_path_setting(p, "chats"))
}

/// The recipient participant key: the registry row's `session_id` for the
/// addressee (exact match on session_id, harness_session_id or name - the
/// d-e952ed19 join, a name is a label), else the raw `to` string (AC1-ERR).
fn recipient_key(to: &str) -> String {
    let home = crate::paths::AgentsHome::from_env();
    let Ok(text) = std::fs::read_to_string(home.registry_json()) else {
        return to.to_string();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return to.to_string();
    };
    for row in parsed
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let matches = ["session_id", "harness_session_id", "name"]
            .iter()
            .any(|k| row.get(*k).and_then(Value::as_str) == Some(to));
        if matches {
            if let Some(sid) = row.get("session_id").and_then(Value::as_str) {
                return sid.to_string();
            }
        }
    }
    to.to_string()
}

fn scope_of(line: &Value) -> String {
    line.get("meta")
        .and_then(|m| m.get("scope"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            line.get("to")
                .and_then(Value::as_str)
                .and_then(|t| t.strip_prefix("fleet:"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "all".to_string())
}

/// Exclusive flock on the chat's sidecar lockfile
/// (`messages.jsonl + .lock`); the announce.rs BusLock pattern.
struct ChatLock {
    _file: std::fs::File,
}

impl ChatLock {
    fn acquire(chat_file: &Path) -> Result<ChatLock, String> {
        let lock_path = PathBuf::from(format!("{}.lock", chat_file.display()));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("chat lock open {}: {e}", lock_path.display()))?;
        let deadline = std::time::Instant::now() + CHAT_LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(ChatLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "chat lock timeout after {CHAT_LOCK_TIMEOUT:?} at {}",
                            lock_path.display()
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("chat lock: {e}")),
            }
        }
    }
}

/// Append one line to the chat file under the per-chat flock, owner-only
/// modes (the bus chmod convention, bus/log.py:417-429).
fn append_chat_line(chat_dir: &Path, line: &Value) -> Result<(), String> {
    std::fs::create_dir_all(chat_dir).map_err(|e| format!("chat dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(chat_dir, std::fs::Permissions::from_mode(0o700));
    }
    let mut line_s = serde_json::to_string(line).map_err(|e| format!("serialize: {e}"))?;
    line_s.push('\n');
    let chat_file = chat_dir.join("messages.jsonl");
    let _lock = ChatLock::acquire(&chat_file)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options
        .open(&chat_file)
        .map_err(|e| format!("chat file open {}: {e}", chat_file.display()))?;
    f.write_all(line_s.as_bytes())
        .map_err(|e| format!("chat append: {e}"))
}
// ---------------------------------------------------------------------------
// Record
// ---------------------------------------------------------------------------

/// What record() did with one bus line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recorded {
    Message { chat_id: String },
    Delivery { chat_id: String },
    Skipped,
}

/// The stored message line for a message-kind bus row: the chat id plus the
/// envelope verbatim with `type` and `chat_id` added. `None` for a row
/// without an id. record_at and migrate_import had written this shape twice,
/// verbatim; it lives once now.
fn message_line(line: &Value) -> Option<(String, Value)> {
    let id = line.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return None;
    }
    let kind = line.get("kind").and_then(Value::as_str).unwrap_or("");
    let from_key = line
        .get("from_session")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            line.get("from")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        })
        .to_string();
    let chat_id = if kind == "announce" {
        chat_id_for_channel(&scope_of(line))
    } else {
        let to = line.get("to").and_then(Value::as_str).unwrap_or("");
        chat_id_for_pair(&from_key, &recipient_key(to))
    };
    let mut rec = line.clone();
    if let Value::Object(map) = &mut rec {
        map.insert("type".into(), json!("message"));
        map.insert("chat_id".into(), json!(chat_id));
    }
    Some((chat_id, rec))
}

/// Record one bus line into the store (plan R2's shared seam target).
///
/// Message kinds (`send`, `announce`) become one message line: the bus
/// envelope verbatim plus `type` and the stored `chat_id`. A message-kind row
/// that also carries a `delivery` mark is the message AND its receipt in one
/// row (hosted and typed sends are written after delivery), so it records the
/// message line when the id is unrecorded, then the delivery line either way.
/// Receipt rows (`landed`) become one delivery line (R3). Anything else is
/// control traffic and is skipped. The write is the JSONL only; the index
/// catches up lazily on the next read (it is derived, never authoritative).
pub(crate) fn record_at(
    chats_dir: &Path,
    db: &Path,
    bus: &Path,
    line: &Value,
) -> Result<Recorded, String> {
    ensure_ready_at(chats_dir, db, bus)?;
    let kind = line.get("kind").and_then(Value::as_str).unwrap_or("");
    let delivery = line
        .get("delivery")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty());
    if is_message_kind(kind) && delivery.is_none() {
        let Some((chat_id, rec)) = message_line(line) else {
            return Ok(Recorded::Skipped);
        };
        append_chat_line(&chats_dir.join(&chat_id), &rec)?;
        Ok(Recorded::Message { chat_id })
    } else if is_message_kind(kind) {
        let id = line.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return Ok(Recorded::Skipped);
        }
        let chat_id = match chat_of_message(chats_dir, db, id)? {
            Some(chat_id) => chat_id,
            None => {
                let Some((chat_id, rec)) = message_line(line) else {
                    return Ok(Recorded::Skipped);
                };
                append_chat_line(&chats_dir.join(&chat_id), &rec)?;
                chat_id
            }
        };
        append_chat_line(
            &chats_dir.join(&chat_id),
            &json!({
                "type": "delivery",
                "chat_id": chat_id,
                "id": id,
                "session": line.get("to").and_then(Value::as_str).unwrap_or(""),
                "how": delivery.unwrap_or(""),
                "ts": line.get("ts").and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(crate::announce::now_iso),
            }),
        )?;
        Ok(Recorded::Delivery { chat_id })
    } else if kind == "landed" {
        let meta = line.get("meta").cloned().unwrap_or(Value::Null);
        let session = meta
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or_else(|| line.get("to").and_then(Value::as_str).unwrap_or(""));
        let (id, session, how) = (
            meta.get("landed")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            session.to_string(),
            "landed".to_string(),
        );
        if id.is_empty() {
            return Ok(Recorded::Skipped);
        }
        let Some(chat_id) = chat_of_message(chats_dir, db, &id)? else {
            // A receipt for an id this store never recorded has no chat to
            // land in; the bus row already stands.
            return Ok(Recorded::Skipped);
        };
        let rec = json!({
            "type": "delivery",
            "chat_id": chat_id,
            "id": id,
            "session": session,
            "how": how,
            "ts": line.get("ts").and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(crate::announce::now_iso),
        });
        append_chat_line(&chats_dir.join(&chat_id), &rec)?;
        Ok(Recorded::Delivery { chat_id })
    } else {
        Ok(Recorded::Skipped)
    }
}

/// The chat that holds `id`, from the index (the messages table), falling
/// back to a full scan of the chat files (plan Changes 1: the scan covers the
/// index being rebuilt or stale).
fn chat_of_message(chats_dir: &Path, db: &Path, id: &str) -> Result<Option<String>, String> {
    let conn = open_index(db)?;
    let mut stmt = conn
        .prepare("SELECT chat_id FROM messages WHERE id = ?1 LIMIT 1")
        .map_err(|e| format!("index query: {e}"))?;
    let mut rows = stmt
        .query_map([id], |r| r.get::<_, String>(0))
        .map_err(|e| format!("index query: {e}"))?;
    if let Some(first) = rows.next() {
        return Ok(Some(first.map_err(|e| format!("index row: {e}"))?));
    }
    drop(rows);
    drop(stmt);
    scan_chat_files_for_id(chats_dir, id)
}

fn scan_chat_files_for_id(chats_dir: &Path, id: &str) -> Result<Option<String>, String> {
    let rd = match std::fs::read_dir(chats_dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(None),
    };
    for entry in rd.flatten() {
        let file = entry.path().join("messages.jsonl");
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) == Some("message")
                && v.get("id").and_then(Value::as_str) == Some(id)
            {
                if let Some(chat) = entry.file_name().to_str() {
                    return Ok(Some(chat.to_string()));
                }
            }
        }
    }
    Ok(None)
}
// ---------------------------------------------------------------------------
// Index (derived; the JSONL is the only record)
// ---------------------------------------------------------------------------

use rusqlite::Connection;

fn open_index(db: &Path) -> Result<Connection, String> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("index dir: {e}"))?;
    }
    // First contact serializes on the store lock (backlog::open's pattern):
    // two processes creating the db race the journal_mode pragma.
    let _creation_lock = if db.exists() {
        None
    } else {
        Some(
            crate::graph_store::BoundedLock::acquire(db, std::time::Duration::from_secs(10))
                .map_err(|e| e.to_string())?,
        )
    };
    let conn = Connection::open(db).map_err(|e| format!("index open {}: {e}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| format!("index busy_timeout: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("index journal_mode: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chats(
            chat_id TEXT PRIMARY KEY,
            participants TEXT NOT NULL,
            created_ts TEXT NOT NULL,
            last_ts TEXT NOT NULL,
            last_msg_id TEXT,
            msg_count INTEGER NOT NULL DEFAULT 0,
            last_line_hash TEXT
        );
        CREATE TABLE IF NOT EXISTS messages(
            chat_id TEXT NOT NULL,
            id TEXT NOT NULL,
            ts TEXT NOT NULL,
            from_key TEXT NOT NULL,
            to_key TEXT NOT NULL,
            in_reply_to TEXT,
            PRIMARY KEY(chat_id, id)
        );
        CREATE INDEX IF NOT EXISTS messages_by_id ON messages(id);
        CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(|e| format!("index schema: {e}"))?;
    Ok(conn)
}

fn migrated_at(conn: &Connection) -> Result<Option<String>, String> {
    let mut stmt = conn
        .prepare("SELECT value FROM meta WHERE key = 'migrated_at'")
        .map_err(|e| format!("meta read: {e}"))?;
    let mut rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("meta read: {e}"))?;
    Ok(rows
        .next()
        .transpose()
        .map_err(|e| format!("meta row: {e}"))?)
}

fn set_migrated_at(conn: &Connection, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES('migrated_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [value],
    )
    .map_err(|e| format!("meta write: {e}"))?;
    Ok(())
}

fn has_any_chat_file(chats_dir: &Path) -> bool {
    std::fs::read_dir(chats_dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().join("messages.jsonl").is_file())
        })
        .unwrap_or(false)
}

/// One chat file parsed: message lines plus the rollup inputs. Malformed
/// lines are skipped (the JSONL tolerates a torn final line from a crash).
struct ChatScan {
    messages: Vec<Value>,
    msg_count: usize,
    last_line_hash: String,
}

fn scan_chat(chat_file: &Path) -> Result<ChatScan, String> {
    let text = std::fs::read_to_string(chat_file)
        .map_err(|e| format!("chat read {}: {e}", chat_file.display()))?;
    let mut messages = Vec::new();
    let mut last_line = String::new();
    for line in text.lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("type").and_then(Value::as_str) == Some("message") {
                messages.push(v);
            }
        }
        last_line.clear();
        last_line.push_str(line);
    }
    let msg_count = text.lines().filter(|l| !l.trim().is_empty()).count();
    let mut h = Sha256::new();
    h.update(last_line.as_bytes());
    let digest = h.finalize();
    Ok(ChatScan {
        messages,
        msg_count,
        last_line_hash: hex16(&digest),
    })
}

/// Re-derive one chat's rollup from its file and upsert it. Returns false
/// when the chat file is missing or empty (the caller drops the row).
fn refresh_chat(conn: &Connection, chat_dir: &Path, chat_id: &str) -> Result<bool, String> {
    let chat_file = chat_dir.join("messages.jsonl");
    if !chat_file.is_file() {
        conn.execute("DELETE FROM chats WHERE chat_id = ?1", [chat_id])
            .map_err(|e| format!("index delete: {e}"))?;
        conn.execute("DELETE FROM messages WHERE chat_id = ?1", [chat_id])
            .map_err(|e| format!("index delete: {e}"))?;
        return Ok(false);
    }
    let scan = scan_chat(&chat_file)?;
    let first = scan.messages.first().cloned();
    let last = scan.messages.last().cloned();
    let (Some(first), Some(last)) = (first, last) else {
        conn.execute("DELETE FROM chats WHERE chat_id = ?1", [chat_id])
            .map_err(|e| format!("index delete: {e}"))?;
        conn.execute("DELETE FROM messages WHERE chat_id = ?1", [chat_id])
            .map_err(|e| format!("index delete: {e}"))?;
        return Ok(false);
    };
    let key = |v: &Value, session: &str, fallback: &str| -> String {
        v.get(session)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| v.get(fallback).and_then(Value::as_str).unwrap_or("unknown"))
            .to_string()
    };
    let from_key = key(&first, "from_session", "from");
    let to_key = key(&first, "to_key", "to");
    let mut participants = [from_key.clone(), to_key.clone()];
    participants.sort_unstable();
    let created_ts = first.get("ts").and_then(Value::as_str).unwrap_or("");
    let last_ts = last.get("ts").and_then(Value::as_str).unwrap_or("");
    let last_msg_id = last.get("id").and_then(Value::as_str).unwrap_or("");
    conn.execute(
        "INSERT INTO chats(chat_id, participants, created_ts, last_ts, last_msg_id, msg_count, last_line_hash)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(chat_id) DO UPDATE SET
            participants = excluded.participants,
            created_ts = excluded.created_ts,
            last_ts = excluded.last_ts,
            last_msg_id = excluded.last_msg_id,
            msg_count = excluded.msg_count,
            last_line_hash = excluded.last_line_hash",
        rusqlite::params![
            chat_id,
            participants.join(","),
            created_ts,
            last_ts,
            last_msg_id,
            scan.msg_count as i64,
            scan.last_line_hash
        ],
    )
    .map_err(|e| format!("index upsert: {e}"))?;
    conn.execute("DELETE FROM messages WHERE chat_id = ?1", [chat_id])
        .map_err(|e| format!("index delete: {e}"))?;
    for m in &scan.messages {
        conn.execute(
            "INSERT OR IGNORE INTO messages(chat_id, id, ts, from_key, to_key, in_reply_to)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                chat_id,
                m.get("id").and_then(Value::as_str).unwrap_or(""),
                m.get("ts").and_then(Value::as_str).unwrap_or(""),
                key(m, "from_session", "from"),
                key(m, "to_key", "to"),
                m.get("in_reply_to").and_then(Value::as_str),
            ],
        )
        .map_err(|e| format!("index insert: {e}"))?;
    }
    Ok(true)
}

/// Reconcile the index with the chat files: refresh stale chats, drop
/// vanished ones. The staleness check (stored msg_count/last_line_hash vs the
/// file's actual count and last line) runs before any read trusts the index.
fn ensure_index(conn: &Connection, chats_dir: &Path) -> Result<(), String> {
    let rd = std::fs::read_dir(chats_dir).map_err(|e| format!("chats dir: {e}"))?;
    for entry in rd.flatten() {
        let Ok(chat_id) = entry.file_name().into_string() else {
            continue;
        };
        let chat_file = entry.path().join("messages.jsonl");
        let stored: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT msg_count, last_line_hash FROM chats WHERE chat_id = ?1",
                [&chat_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let actual = scan_chat(&chat_file)?;
        let fresh = stored.as_ref().is_some_and(|(count, hash)| {
            *count == actual.msg_count as i64
                && hash.as_deref() == Some(actual.last_line_hash.as_str())
        });
        if !fresh {
            refresh_chat(conn, &entry.path(), &chat_id)?;
        }
    }
    // Index rows whose chat file vanished: refresh_chat's missing-file branch
    // handles rows the pass above visited; sweep the rest.
    let stale_ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT chat_id FROM chats")
            .map_err(|e| format!("index read: {e}"))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| format!("index read: {e}"))?;
        rows.filter_map(|r| r.ok())
            .filter(|id| !chats_dir.join(id).join("messages.jsonl").is_file())
            .collect()
    };
    for id in stale_ids {
        conn.execute("DELETE FROM chats WHERE chat_id = ?1", [&id])
            .map_err(|e| format!("index delete: {e}"))?;
        conn.execute("DELETE FROM messages WHERE chat_id = ?1", [&id])
            .map_err(|e| format!("index delete: {e}"))?;
    }
    Ok(())
}

/// Rebuild the index from the JSONL only; never re-imports the bus (R4).
pub(crate) fn rebuild_index_at(db: &Path, chats_dir: &Path) -> Result<String, String> {
    let conn = open_index(db)?;
    conn.execute("DELETE FROM chats", [])
        .map_err(|e| format!("index clear: {e}"))?;
    conn.execute("DELETE FROM messages", [])
        .map_err(|e| format!("index clear: {e}"))?;
    let mut rebuilt = 0usize;
    if chats_dir.is_dir() {
        let rd = std::fs::read_dir(chats_dir).map_err(|e| format!("chats dir: {e}"))?;
        for entry in rd.flatten() {
            let Ok(chat_id) = entry.file_name().into_string() else {
                continue;
            };
            if entry.path().join("messages.jsonl").is_file()
                && refresh_chat(&conn, &entry.path(), &chat_id)?
            {
                rebuilt += 1;
            }
        }
    }
    Ok(format!("rebuilt {rebuilt} chat(s)"))
}

/// One resolved message row: the full id plus the fields a reply resolver
/// and a reader need without a second round trip.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub id: String,
    pub chat_id: String,
    pub from_key: String,
    pub ts: String,
}

/// Message rows from the index for a unique prefix (AC5-ERR): one match
/// resolves, zero is an error, several name the candidates.
pub(crate) fn resolve_prefix_at(
    db: &Path,
    chats_dir: &Path,
    prefix: &str,
) -> Result<Resolved, String> {
    let conn = open_index(db)?;
    ensure_index(&conn, chats_dir)?;
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("{escaped}%");
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT id, chat_id, ts, from_key FROM messages WHERE id LIKE ?1 ESCAPE '\\'",
        )
        .map_err(|e| format!("resolve query: {e}"))?;
    let rows = stmt
        .query_map([&pattern], |r| {
            Ok(Resolved {
                id: r.get(0)?,
                chat_id: r.get(1)?,
                from_key: r.get(3)?,
                ts: r.get(2)?,
            })
        })
        .map_err(|e| format!("resolve query: {e}"))?;
    let ids: Vec<Resolved> = rows.filter_map(|r| r.ok()).collect();
    match ids.len() {
        1 => Ok(ids.into_iter().next().unwrap()),
        0 => Err(format!("no stored id matches prefix {prefix:?}")),
        _ => Err(format!(
            "prefix {prefix:?} is ambiguous: {} candidates",
            ids.iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}
// ---------------------------------------------------------------------------
// Migration (plan R4: automatic, once, refuses re-import)
// ---------------------------------------------------------------------------

/// Counts the migration receipt reports; skipped rows are unparseable lines
/// and control traffic, never a fatal error (AC6-ERR).
#[derive(Debug, Default)]
pub(crate) struct MigrationReceipt {
    pub messages: usize,
    pub deliveries: usize,
    pub skipped: usize,
}

impl MigrationReceipt {
    fn summary(&self) -> String {
        format!(
            "imported {} message(s), {} delivery(ies), skipped {}",
            self.messages, self.deliveries, self.skipped
        )
    }
}

fn bus_live_path() -> PathBuf {
    let mut dir = crate::backlog::settings::state_dir().unwrap_or_else(default_dot_fno);
    dir.push("bus");
    dir.push("messages.jsonl");
    dir
}

/// Import the retained bus rows oldest-first: `send`/`announce` rows become
/// message lines (original ids kept), receipt rows become delivery lines
/// joined on their message id. Rows without an id or of a control kind are
/// skipped and counted. Each chat's file is written chronologically in bus
/// order.
fn migrate_import(chats_dir: &Path, bus: &Path) -> Result<MigrationReceipt, String> {
    let rows = crate::announce::read_bus_segments(bus);
    let mut receipt = MigrationReceipt::default();
    // The id -> chat map the delivery join reads (first message occurrence
    // wins; ids are unique per send), and the staged lines per chat in bus
    // order. One pass: a receipt always follows its message on the bus.
    let mut id_to_chat: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut chats: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    let stage = |chats: &mut std::collections::HashMap<String, Vec<Value>>,
                 order: &mut Vec<String>,
                 chat_id: String,
                 rec: Value| {
        if !chats.contains_key(&chat_id) {
            order.push(chat_id.clone());
        }
        chats.entry(chat_id).or_default().push(rec);
    };
    for line in &rows {
        let kind = line.get("kind").and_then(Value::as_str).unwrap_or("");
        let delivery = line
            .get("delivery")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty());
        if is_message_kind(kind) && delivery.is_none() {
            let Some((chat_id, rec)) = message_line(line) else {
                receipt.skipped += 1;
                continue;
            };
            let id = line.get("id").and_then(Value::as_str).unwrap_or("");
            if id_to_chat.contains_key(id) {
                // A duplicate of one already staged records once, not twice.
                receipt.skipped += 1;
                continue;
            }
            id_to_chat
                .entry(id.to_string())
                .or_insert_with(|| chat_id.clone());
            stage(&mut chats, &mut order, chat_id, rec);
            receipt.messages += 1;
        } else if kind == "landed" {
            let meta = line.get("meta").cloned().unwrap_or(Value::Null);
            let (id, session, how) = (
                meta.get("landed")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                meta.get("session")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| line.get("to").and_then(Value::as_str).unwrap_or(""))
                    .to_string(),
                "landed".to_string(),
            );
            if id.is_empty() {
                receipt.skipped += 1;
                continue;
            }
            let Some(chat_id) = id_to_chat.get(&id) else {
                receipt.skipped += 1;
                continue;
            };
            let rec = json!({
                "type": "delivery",
                "chat_id": chat_id,
                "id": id,
                "session": session,
                "how": how,
                "ts": line.get("ts").and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(crate::announce::now_iso),
            });
            stage(&mut chats, &mut order, chat_id.clone(), rec);
            receipt.deliveries += 1;
        } else if is_message_kind(kind) {
            // A hosted or typed send is ONE row carrying the message and its
            // receipt. An unrecorded id stages the message line first, then
            // the delivery line; a staged id gets only the delivery line.
            let id = line.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                receipt.skipped += 1;
                continue;
            }
            let chat_id = match id_to_chat.get(id) {
                Some(chat_id) => chat_id.clone(),
                None => {
                    let Some((chat_id, rec)) = message_line(line) else {
                        receipt.skipped += 1;
                        continue;
                    };
                    id_to_chat
                        .entry(id.to_string())
                        .or_insert_with(|| chat_id.clone());
                    stage(&mut chats, &mut order, chat_id.clone(), rec);
                    receipt.messages += 1;
                    chat_id
                }
            };
            let rec = json!({
                "type": "delivery",
                "chat_id": chat_id,
                "id": id,
                "session": line.get("to").and_then(Value::as_str).unwrap_or(""),
                "how": delivery.unwrap_or(""),
                "ts": line.get("ts").and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(crate::announce::now_iso),
            });
            stage(&mut chats, &mut order, chat_id.clone(), rec);
            receipt.deliveries += 1;
        } else {
            receipt.skipped += 1;
        }
    }
    for chat_id in &order {
        for line in &chats[chat_id] {
            append_chat_line(&chats_dir.join(chat_id), line)?;
        }
    }
    Ok(receipt)
}

/// The `migrate --missing` backfill: import every message-kind bus row whose
/// id the store does not hold, oldest first, each with its delivery line when
/// the row carries a mark. Rows already recorded are skipped and counted, so
/// a second run imports 0 (AC3). Never touches the migrated stamp and never
/// refuses: it runs against a store that migrated long ago.
fn migrate_missing(chats_dir: &Path, db: &Path, bus: &Path) -> Result<MigrationReceipt, String> {
    let rows = crate::announce::read_bus_segments(bus);
    let mut receipt = MigrationReceipt::default();
    let mut order: Vec<String> = Vec::new();
    let mut chats: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    let mut staged_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in &rows {
        let kind = line.get("kind").and_then(Value::as_str).unwrap_or("");
        if !is_message_kind(kind) {
            continue;
        }
        let id = line.get("id").and_then(Value::as_str).unwrap_or("");
        let known = !id.is_empty()
            && (staged_ids.contains(id) || chat_of_message(chats_dir, db, id)?.is_some());
        if known {
            receipt.skipped += 1;
            continue;
        }
        let Some((chat_id, rec)) = message_line(line) else {
            receipt.skipped += 1;
            continue;
        };
        if !chats.contains_key(&chat_id) {
            order.push(chat_id.clone());
        }
        staged_ids.insert(id.to_string());
        chats.entry(chat_id.clone()).or_default().push(rec);
        receipt.messages += 1;
        let delivery = line
            .get("delivery")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty());
        if let Some(how) = delivery {
            let rec = json!({
                "type": "delivery",
                "chat_id": chat_id,
                "id": id,
                "session": line.get("to").and_then(Value::as_str).unwrap_or(""),
                "how": how,
                "ts": line.get("ts").and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(crate::announce::now_iso),
            });
            chats.entry(chat_id).or_default().push(rec);
            receipt.deliveries += 1;
        }
    }
    for chat_id in &order {
        for line in &chats[chat_id] {
            append_chat_line(&chats_dir.join(chat_id), line)?;
        }
    }
    Ok(receipt)
}

/// Stamp-preserving readiness check at explicit paths (the testable core).
fn ensure_ready_at(chats_dir: &Path, db: &Path, bus: &Path) -> Result<(), String> {
    let conn = open_index(db)?;
    if migrated_at(&conn)?.is_some() {
        return Ok(());
    }
    if has_any_chat_file(chats_dir) {
        set_migrated_at(&conn, &format!("recovered:{}", crate::announce::now_iso()))?;
        return Ok(());
    }
    let receipt = migrate_import(chats_dir, bus)?;
    set_migrated_at(
        &conn,
        &format!("{}:{}", crate::announce::now_iso(), receipt.summary()),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Verb doors (the hidden `fno-agents chats` family; never advertised)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Show (the `fno agents mail show` read model; the door is run_chats "show")
// ---------------------------------------------------------------------------

/// One show invocation's flags and the caller identity the privacy rule
/// reads. `caller` is the resolved session id; `None` reads as outside
/// unless `all` lifts the scope.
pub(crate) struct ShowQuery {
    pub thread: bool,
    pub json: bool,
    pub all: bool,
    pub limit: usize,
    pub caller: Option<String>,
}

fn participant_key(v: &Value, session: &str, fallback: &str) -> String {
    v.get(session)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| v.get(fallback).and_then(Value::as_str).unwrap_or("unknown"))
        .to_string()
}

/// The privacy rule a body prints under: the caller is the message's sender
/// or recipient; `--all` is the operator override.
fn caller_in_message(line: &Value, caller: &str) -> bool {
    let norm = crate::mail_hold::identity_key;
    norm(&participant_key(line, "from_session", "from")) == norm(caller)
        || norm(&participant_key(line, "to_key", "to")) == norm(caller)
}

fn message_body(line: &Value) -> &str {
    line.get("body").and_then(Value::as_str).unwrap_or("")
}

/// One message as the reader sees it: the delivered header line, then the
/// full body. The auto summary stands in until the Rust send verb sets
/// subjects (wave 2); the id is what the receiver answers and resolves with.
fn render_message(line: &Value) -> String {
    format!(
        "{}\n{}",
        crate::mail_header::render_header(
            crate::mail_header::HeaderForm::Mention,
            line.get("from")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            line.get("id").and_then(Value::as_str).unwrap_or(""),
            crate::mail_header::summary_of(message_body(line)),
        ),
        message_body(line),
    )
}

/// A chat file's message and delivery lines, in file order. Malformed lines
/// are skipped (the JSONL tolerates a torn final line).
fn read_chat_lines(chat_file: &Path) -> (Vec<Value>, Vec<Value>) {
    let mut messages = Vec::new();
    let mut deliveries = Vec::new();
    if let Ok(text) = std::fs::read_to_string(chat_file) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                match v.get("type").and_then(Value::as_str) {
                    Some("message") => messages.push(v),
                    Some("delivery") => deliveries.push(v),
                    _ => {}
                }
            }
        }
    }
    (messages, deliveries)
}

fn ts_of(line: &Value) -> &str {
    line.get("ts").and_then(Value::as_str).unwrap_or("")
}

/// `show <id>`: resolve the prefix, privacy-gate, render one message -- or
/// with `--thread`, the whole chat in ts order, each delivery line as one
/// `delivered <how> <ts>` line under its message. The error string carries
/// the door's wording; "ambiguous" maps to exit 2 there.
pub(crate) fn show_at(
    chats_dir: &Path,
    db: &Path,
    id: &str,
    q: &ShowQuery,
) -> Result<String, String> {
    let hit = resolve_prefix_at(db, chats_dir, id)?;
    let (messages, deliveries) =
        read_chat_lines(&chats_dir.join(&hit.chat_id).join("messages.jsonl"));
    let Some(msg) = messages
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(hit.id.as_str()))
    else {
        return Err(format!("no stored id matches {id:?}"));
    };
    if !q.all
        && !q
            .caller
            .as_deref()
            .map(|c| caller_in_message(msg, c))
            .unwrap_or(false)
    {
        return Err(format!(
            "{id} is not addressed to or from the caller; --all is the operator view"
        ));
    }
    if q.json {
        return Ok(msg.to_string());
    }
    if !q.thread {
        return Ok(render_message(msg));
    }
    let mut sorted = messages.clone();
    sorted.sort_by(|a, b| ts_of(a).cmp(ts_of(b)));
    let mut out = String::new();
    for m in &sorted {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&render_message(m));
        let mid = m.get("id").and_then(Value::as_str).unwrap_or("");
        for d in deliveries
            .iter()
            .filter(|d| d.get("id").and_then(Value::as_str) == Some(mid))
        {
            out.push_str(&format!(
                "\ndelivered {} {}",
                d.get("how").and_then(Value::as_str).unwrap_or(""),
                d.get("ts").and_then(Value::as_str).unwrap_or(""),
            ));
        }
    }
    Ok(out)
}

/// `show` with no id: the listing `view` printed, scoped to the caller's own
/// session (`--all` lifts it), the most recent `limit` messages by ts.
pub(crate) fn show_list_at(chats_dir: &Path, q: &ShowQuery) -> Result<String, String> {
    let mut rows: Vec<Value> = Vec::new();
    if chats_dir.is_dir() {
        let rd = std::fs::read_dir(chats_dir).map_err(|e| format!("chats dir: {e}"))?;
        for entry in rd.flatten() {
            let (messages, _) = read_chat_lines(&entry.path().join("messages.jsonl"));
            rows.extend(messages);
        }
    }
    if !q.all {
        let Some(caller) = q.caller.as_deref() else {
            return Ok(String::new());
        };
        rows.retain(|m| caller_in_message(m, caller));
    }
    rows.sort_by(|a, b| ts_of(a).cmp(ts_of(b)));
    if q.limit > 0 && rows.len() > q.limit {
        rows.drain(..rows.len() - q.limit);
    }
    if q.json {
        return Ok(Value::Array(rows).to_string());
    }
    let mut out = String::new();
    for m in rows {
        let mut body1 = message_body(m).trim().replace('\n', " ");
        if body1.chars().count() > 80 {
            body1 = format!("{}...", body1.chars().take(77).collect::<String>());
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "{}  {} -> {} ({}/{}): {}",
            ts_of(m),
            m.get("from").and_then(Value::as_str).unwrap_or("unknown"),
            m.get("to").and_then(Value::as_str).unwrap_or(""),
            m.get("kind").and_then(Value::as_str).unwrap_or(""),
            m.get("delivery").and_then(Value::as_str).unwrap_or(""),
            body1
        ));
    }
    Ok(out)
}

fn valid_chat_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix(CHAT_PREFIX) else {
        return false;
    };
    rest.len() == 16 && rest.bytes().all(|b| b.is_ascii_hexdigit())
}

fn usage() -> i32 {
    eprintln!("usage: fno-agents chats <append|migrate|rebuild|list|read|resolve|show> ...");
    2
}

/// Verb entrypoint reached from bin/client.rs's direct dispatch. `pub`: the
/// bin target sees the lib as an external crate (the announce.rs note).
pub fn run_chats(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        return usage();
    };
    let dir = chats_dir();
    match sub.as_str() {
        "append" => {
            // The Python seam's door: one bus line on stdin. A non-zero exit
            // aborts a message-kind send (fail closed, plan R2).
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_err() {
                eprintln!("chats append: could not read the bus line from stdin");
                return 1;
            }
            let line: Value = match serde_json::from_str(input.trim()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("chats append: not a JSON bus line: {e}");
                    return 1;
                }
            };
            match record_at(&dir, &index_path(), &bus_live_path(), &line) {
                Ok(Recorded::Message { chat_id }) => {
                    println!(
                        "{{\"recorded\":true,\"type\":\"message\",\"chat_id\":\"{chat_id}\"}}"
                    );
                    0
                }
                Ok(Recorded::Delivery { chat_id }) => {
                    println!(
                        "{{\"recorded\":true,\"type\":\"delivery\",\"chat_id\":\"{chat_id}\"}}"
                    );
                    0
                }
                Ok(Recorded::Skipped) => {
                    println!("{{\"recorded\":false,\"reason\":\"not a recordable row\"}}");
                    0
                }
                Err(e) => {
                    eprintln!("chats append: {e}");
                    1
                }
            }
        }
        "migrate" => {
            if args.iter().any(|a| a == "--missing") {
                return match migrate_missing(&dir, &index_path(), &bus_live_path()) {
                    Ok(receipt) => {
                        println!(
                            "{{\"migrated\":true,\"missing\":true,\"messages\":{},\"deliveries\":{},\"skipped\":{}}}",
                            receipt.messages, receipt.deliveries, receipt.skipped
                        );
                        0
                    }
                    Err(e) => {
                        eprintln!("chats migrate: {e}");
                        1
                    }
                };
            }
            let conn = match open_index(&index_path()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("chats migrate: {e}");
                    return 1;
                }
            };
            if let Some(ts) = migrated_at(&conn).unwrap_or(None) {
                eprintln!("chats migrate: refused, already migrated ({ts})");
                return 1;
            }
            match migrate_import(&dir, &bus_live_path()) {
                Ok(receipt) => {
                    let _ = set_migrated_at(
                        &conn,
                        &format!("{}:{}", crate::announce::now_iso(), receipt.summary()),
                    );
                    println!(
                        "{{\"migrated\":true,\"messages\":{},\"deliveries\":{},\"skipped\":{}}}",
                        receipt.messages, receipt.deliveries, receipt.skipped
                    );
                    0
                }
                Err(e) => {
                    eprintln!("chats migrate: {e}");
                    1
                }
            }
        }
        "rebuild" => match rebuild_index(&dir) {
            Ok(summary) => {
                println!("{{\"rebuilt\":true,\"summary\":\"{summary}\"}}");
                0
            }
            Err(e) => {
                eprintln!("chats rebuild: {e}");
                1
            }
        },
        "list" => match list_chats(&dir) {
            Ok(rows) => {
                for row in rows {
                    println!("{row}");
                }
                0
            }
            Err(e) => {
                eprintln!("chats list: {e}");
                1
            }
        },
        "read" => {
            let Some(chat_id) = args
                .iter()
                .position(|a| a == "--chat")
                .and_then(|i| args.get(i + 1))
            else {
                eprintln!("chats read: --chat <chat_id> is required");
                return 2;
            };
            if !valid_chat_id(chat_id) {
                eprintln!("chats read: {chat_id:?} is not a chat id (chat-<16 hex>)");
                return 2;
            }
            let file = dir.join(chat_id).join("messages.jsonl");
            match std::fs::read_to_string(&file) {
                Ok(text) => {
                    for line in text.lines() {
                        println!("{line}");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("chats read: {}: {e}", file.display());
                    1
                }
            }
        }
        "show" => {
            let mut id: Option<String> = None;
            let mut q = ShowQuery {
                thread: false,
                json: false,
                all: false,
                limit: 50,
                caller: None,
            };
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--thread" => q.thread = true,
                    "--json" => q.json = true,
                    "--all" => q.all = true,
                    "-n" | "--limit" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
                        Some(n) => q.limit = n,
                        None => {
                            eprintln!("chats show: -n/--limit needs a number");
                            return 2;
                        }
                    },
                    "--help" => {
                        println!("usage: fno-agents chats show [<id-or-prefix>] [--thread] [--json] [--all] [-n N]");
                        return 0;
                    }
                    other => {
                        if other.starts_with('-') || id.is_some() {
                            eprintln!("chats show: unexpected argument {other:?}");
                            return 2;
                        }
                        id = Some(other.to_string());
                    }
                }
            }
            let home = crate::paths::AgentsHome::from_env();
            q.caller = crate::spawn_context::resolve_self_identity(
                &|k| std::env::var(k).ok(),
                None,
                None,
                &home,
            )
            .session_id;
            let out = match &id {
                Some(id) => show_at(&dir, &index_path(), id, &q),
                None if q.thread => {
                    eprintln!("chats show: --thread needs an id");
                    return 2;
                }
                None => show_list_at(&dir, &q),
            };
            match out {
                Ok(s) if s.is_empty() && id.is_none() && !q.json => {
                    println!("no messages");
                    0
                }
                Ok(s) => {
                    println!("{s}");
                    0
                }
                Err(e) => {
                    eprintln!("chats show: {e}");
                    if e.contains("ambiguous") {
                        2
                    } else {
                        1
                    }
                }
            }
        }
        "resolve" => {
            let Some(prefix) = args
                .iter()
                .position(|a| a == "--prefix")
                .and_then(|i| args.get(i + 1))
            else {
                eprintln!("chats resolve: --prefix <prefix> is required");
                return 2;
            };
            match resolve_prefix(prefix) {
                Ok(hit) => {
                    println!(
                        "{}",
                        json!({
                            "id": hit.id,
                            "chat_id": hit.chat_id,
                            "from_key": hit.from_key,
                            "ts": hit.ts,
                        })
                    );
                    0
                }
                Err(e) => {
                    eprintln!("chats resolve: {e}");
                    if e.contains("ambiguous") {
                        2
                    } else {
                        1
                    }
                }
            }
        }
        _ => usage(),
    }
}

/// One JSON line per chat, index rollup fields (the thread read model's API).
fn list_chats(dir: &Path) -> Result<Vec<String>, String> {
    let conn = open_index(&index_path())?;
    ensure_index(&conn, dir)?;
    let mut stmt = conn
        .prepare("SELECT chat_id, participants, created_ts, last_ts, last_msg_id, msg_count FROM chats ORDER BY last_ts")
        .map_err(|e| format!("index read: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "chat_id": r.get::<_, String>(0)?,
                "participants": r.get::<_, String>(1)?,
                "created_ts": r.get::<_, String>(2)?,
                "last_ts": r.get::<_, String>(3)?,
                "last_msg_id": r.get::<_, Option<String>>(4)?,
                "msg_count": r.get::<_, i64>(5)?,
            })
            .to_string())
        })
        .map_err(|e| format!("index read: {e}"))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}
// ---------------------------------------------------------------------------
// Env-path wrappers (the doors resolve through the state dir; tests pin paths)
// ---------------------------------------------------------------------------

pub(crate) fn record(chats_dir: &Path, line: &Value) -> Result<Recorded, String> {
    record_at(chats_dir, &index_path(), &bus_live_path(), line)
}

#[allow(dead_code)]
pub(crate) fn rebuild_index(chats_dir: &Path) -> Result<String, String> {
    rebuild_index_at(&index_path(), chats_dir)
}

/// The full id behind a prefix, at the env paths (the doors use this).
#[allow(dead_code)]
pub(crate) fn resolve_prefix(prefix: &str) -> Result<Resolved, String> {
    resolve_prefix_at(&index_path(), &chats_dir(), prefix)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
// ---------------------------------------------------------------------------
// Tests (one fn: the gate is shrink-only, and every contract below asserts a
// distinct AC of the store)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// One temp root per test; env-mutating tests share the process, so the
    /// mutex keeps FNO_* pins from racing.
    static ENV_LOCK: std::sync::LazyLock<&'static std::sync::Mutex<()>> =
        std::sync::LazyLock::new(crate::claims::test_env_lock);

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chats-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bus_line(id: &str, from: &str, to: &str, kind: &str) -> Value {
        serde_json::json!({
            "v": 1, "id": id, "ts": "2026-10-01T19:00:00Z", "thread": id,
            "from": from, "to": to, "kind": kind, "body": "hello",
        })
    }

    #[test]
    fn chats_store_contracts() {
        // The recipient-key read resolves the registry through AgentsHome;
        // pin a declared test root or the home-fallback fence fires.
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home_pin = std::env::temp_dir().join(format!("chats-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&home_pin).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", &home_pin);

        // --- derivation: pair symmetry, width, channel divergence (AC3).
        let ab = chat_id_for_pair("alpha", "beta");
        assert_eq!(
            ab,
            chat_id_for_pair("beta", "alpha"),
            "pair(a,b) == pair(b,a)"
        );
        assert!(ab.starts_with("chat-"));
        assert_eq!(ab.len(), "chat-".len() + 16);
        let chan = chat_id_for_channel("dev");
        assert_ne!(ab, chan, "channel ids are never pair-derived");
        assert_eq!(chan, chat_id_for_channel("dev"), "one scope, one chat");
        assert_ne!(chan, chat_id_for_channel("ops"), "two scopes, two chats");

        // --- record: message line shape + owner-only modes (AC1-HP, AC7-HP).
        let root = temp_root("record");
        let chats = root.join("chats");
        let db = root.join("db").join("chats.db");
        let bus = root.join("bus").join("messages.jsonl");
        let line = bus_line("fmail-aaaaaaaaaaaa", "sess-a", "sess-b", "send");
        let Recorded::Message { chat_id } = record_at(&chats, &db, &bus, &line).unwrap() else {
            panic!("expected a message record");
        };
        assert_eq!(chat_id, chat_id_for_pair("sess-a", "sess-b"));
        let file = chats.join(&chat_id).join("messages.jsonl");
        let text = std::fs::read_to_string(&file).unwrap();
        let rec: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(rec["type"], "message");
        assert_eq!(rec["chat_id"], *chat_id);
        assert_eq!(rec["id"], "fmail-aaaaaaaaaaaa");
        assert_eq!(text.lines().count(), 1, "one line per send");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(chats.join(&chat_id))
                .unwrap()
                .permissions()
                .mode();
            let file_mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700, "chat dirs are owner-only");
            assert_eq!(file_mode & 0o777, 0o600, "chat files are owner-only");
        }

        // --- record: announce scopes diverge; unregistered addressee keys raw;
        // control rows and receipts for unknown ids skip (AC3-ERR, AC1-ERR).
        let mut dev = bus_line("fmail-bbbbbbbbbbbb", "op", "fleet:dev", "announce");
        dev["meta"] = serde_json::json!({"scope": "dev"});
        let mut ops = dev.clone();
        ops["id"] = serde_json::json!("fmail-cccccccccccc");
        ops["meta"] = serde_json::json!({"scope": "ops"});
        let Recorded::Message { chat_id: dev_id } = record_at(&chats, &db, &bus, &dev).unwrap()
        else {
            panic!()
        };
        let Recorded::Message { chat_id: ops_id } = record_at(&chats, &db, &bus, &ops).unwrap()
        else {
            panic!()
        };
        assert_ne!(dev_id, ops_id);
        assert_eq!(dev_id, chat_id_for_channel("dev"));
        let stranger = bus_line("fmail-eeeeeeeeeeee", "sess-a", "stranger@nowhere", "send");
        let Recorded::Message {
            chat_id: fallback_id,
        } = record_at(&chats, &db, &bus, &stranger).unwrap()
        else {
            panic!()
        };
        assert_eq!(fallback_id, chat_id_for_pair("sess-a", "stranger@nowhere"));
        let mut withdraw = bus_line("msg-000002", "a", "b", "withdraw");
        withdraw["meta"] = serde_json::json!({"withdraws": "msg-000003"});
        assert_eq!(
            record_at(&chats, &db, &bus, &withdraw).unwrap(),
            Recorded::Skipped
        );
        let orphan = serde_json::json!({
            "v": 1, "id": "msg-000004", "ts": "2026-10-01T19:02:00Z",
            "from": "a", "to": "b", "kind": "landed",
            "body": "", "meta": {"landed": "fmail-000000000000", "session": "b"},
        });
        assert_eq!(
            record_at(&chats, &db, &bus, &orphan).unwrap(),
            Recorded::Skipped,
            "a receipt for an unrecorded id has no chat to land in"
        );

        // --- reads: delivery join, prefix unique/ambiguous/miss (AC2-HP, AC5-ERR).
        let landed = serde_json::json!({
            "v": 1, "id": "msg-000001", "ts": "2026-10-01T19:01:00Z",
            "from": "sess-a", "to": "sess-b", "kind": "landed",
            "body": "", "meta": {"landed": "fmail-aaaaaaaaaaaa", "session": "sess-b"},
        });
        let delivered = record_at(&chats, &db, &bus, &landed).unwrap();
        assert_eq!(
            delivered,
            Recorded::Delivery {
                chat_id: chat_id.clone()
            }
        );
        let last: Value = serde_json::from_str(
            std::fs::read_to_string(chats.join(&chat_id).join("messages.jsonl"))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(last["type"], "delivery");
        assert_eq!(last["how"], "landed");
        let other = bus_line("fmail-111111112222", "sess-a", "sess-c", "send");
        record_at(&chats, &db, &bus, &other).unwrap();
        let other2 = bus_line("fmail-111111111133", "sess-a", "sess-d", "send");
        record_at(&chats, &db, &bus, &other2).unwrap();
        let err = resolve_prefix_at(&db, &chats, "fmail-11111111").unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        let dd = bus_line("fmail-dddddddddddd", "sess-a", "sess-e", "send");
        record_at(&chats, &db, &bus, &dd).unwrap();
        let ok = resolve_prefix_at(&db, &chats, "fmail-dddddddddddd").unwrap();
        assert_eq!(ok.id, "fmail-dddddddddddd");
        assert_eq!(ok.from_key, "sess-a", "the reply adapter reads from_key");
        let none = resolve_prefix_at(&db, &chats, "fmail-99999999");
        assert!(none.is_err());

        // --- hosted and typed sends: one row records message + delivery
        // (AC1: unrecorded id gets both lines; AC2: recorded id gets only
        // the delivery line); migrate --missing backfills and is idempotent
        // (AC3); show gates bodies on the caller and reads in ts order
        // (AC4, AC5, AC6).
        let root = temp_root("hosted");
        let chats = root.join("chats");
        let db = root.join("db").join("chats.db");
        let bus = root.join("bus").join("messages.jsonl");
        std::fs::create_dir_all(bus.parent().unwrap()).unwrap();
        let mut hosted = bus_line("fmail-111111111111", "sess-a", "sess-b", "send");
        hosted["delivery"] = serde_json::json!("hosted");
        assert_eq!(
            record_at(&chats, &db, &bus, &hosted).unwrap(),
            Recorded::Delivery {
                chat_id: chat_id_for_pair("sess-a", "sess-b")
            }
        );
        let text = std::fs::read_to_string(
            chats
                .join(chat_id_for_pair("sess-a", "sess-b"))
                .join("messages.jsonl"),
        )
        .unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "message line then delivery line");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["type"], "message");
        assert_eq!(second["type"], "delivery");
        assert_eq!(second["how"], "hosted");
        assert_eq!(
            resolve_prefix_at(&db, &chats, "fmail-111111111111")
                .unwrap()
                .id,
            "fmail-111111111111",
            "the hosted send's id resolves"
        );

        // AC2: an id already recorded as a message takes only the delivery line.
        let plain = bus_line("fmail-222222222222", "sess-a", "sess-b", "send");
        let Recorded::Message { .. } = record_at(&chats, &db, &bus, &plain).unwrap() else {
            panic!("expected a message record");
        };
        let mut hosted2 = plain.clone();
        hosted2["delivery"] = serde_json::json!("typed");
        assert_eq!(
            record_at(&chats, &db, &bus, &hosted2).unwrap(),
            Recorded::Delivery {
                chat_id: chat_id_for_pair("sess-a", "sess-b")
            }
        );
        let text = std::fs::read_to_string(
            chats
                .join(chat_id_for_pair("sess-a", "sess-b"))
                .join("messages.jsonl"),
        )
        .unwrap();
        let messages_for: usize = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["type"] == "message" && v["id"] == "fmail-222222222222")
            .count();
        assert_eq!(messages_for, 1, "the message is never duplicated");
        let lines: Vec<&str> = text.lines().collect();
        let last: Value = serde_json::from_str(lines[lines.len() - 1]).unwrap();
        assert_eq!(last["type"], "delivery");
        assert_eq!(last["how"], "typed");

        // AC3: the backfill imports unrecorded ids and is idempotent.
        let mut rows = Vec::new();
        for pair in ["sess-c", "sess-d", "sess-e"] {
            let id = format!("fmail-3333333333{}", pair.chars().last().unwrap());
            let mut row = bus_line(&id, "sess-a", pair, "send");
            row["delivery"] = serde_json::json!("hosted");
            rows.push(row);
        }
        let recorded_plain = bus_line("fmail-444444444444", "sess-a", "sess-f", "send");
        let Recorded::Message { .. } = record_at(&chats, &db, &bus, &recorded_plain).unwrap()
        else {
            panic!("expected a message record");
        };
        rows.push(recorded_plain.clone());
        std::fs::write(
            &bus,
            rows.iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let first_run = migrate_missing(&chats, &db, &bus).unwrap();
        assert_eq!(first_run.messages, 3, "three hosted sends import");
        assert_eq!(first_run.deliveries, 3);
        assert_eq!(first_run.skipped, 1, "the recorded plain send skips");
        let second_run = migrate_missing(&chats, &db, &bus).unwrap();
        assert_eq!(second_run.messages, 0, "a second run imports none");
        assert_eq!(second_run.skipped, 4);
        let backfilled = std::fs::read_to_string(
            chats
                .join(chat_id_for_pair("sess-a", "sess-c"))
                .join("messages.jsonl"),
        )
        .unwrap();
        assert_eq!(
            backfilled.lines().count(),
            2,
            "message then its delivery line"
        );

        // show: the sender reads header plus body; a stranger does not; an
        // ambiguous prefix names candidates; the thread reads in ts order.
        let q = |caller: Option<&str>, thread: bool| ShowQuery {
            thread,
            json: false,
            all: false,
            limit: 50,
            caller: caller.map(str::to_string),
        };
        let shown = show_at(&chats, &db, "fmail-444444444444", &q(Some("sess-a"), false)).unwrap();
        assert!(
            shown.starts_with("`@sess-a · fmail-444444444444 · "),
            "{shown}"
        );
        assert!(
            shown.contains("hello"),
            "the body follows the header: {shown}"
        );
        let amb = show_at(&chats, &db, "fmail-3333333333", &q(Some("sess-a"), false));
        assert!(amb.unwrap_err().contains("ambiguous"));
        let stranger = show_at(&chats, &db, "fmail-444444444444", &q(Some("sess-z"), false));
        assert!(stranger
            .unwrap_err()
            .contains("not addressed to or from the caller"));
        let ids_ts = [
            ("fmail-555555555553", "2026-10-03T19:03:00Z"),
            ("fmail-555555555551", "2026-10-03T19:01:00Z"),
            ("fmail-555555555552", "2026-10-03T19:02:00Z"),
        ];
        for (id, ts) in ids_ts {
            let mut row = bus_line(id, "sess-a", "sess-g", "send");
            row["ts"] = serde_json::json!(ts);
            let Recorded::Message { .. } = record_at(&chats, &db, &bus, &row).unwrap() else {
                panic!("expected a message record");
            };
        }
        let thread = show_at(&chats, &db, "fmail-555555555551", &q(Some("sess-a"), true)).unwrap();
        let (i1, i2, i3) = (
            thread.find("fmail-555555555551").unwrap(),
            thread.find("fmail-555555555552").unwrap(),
            thread.find("fmail-555555555553").unwrap(),
        );
        assert!(i1 < i2 && i2 < i3, "the thread reads in ts order: {thread}");
        let mut excluded = bus_line("fmail-666666666666", "sess-x", "sess-y", "send");
        excluded["ts"] = serde_json::json!("2026-10-03T20:00:00Z");
        let Recorded::Message { .. } = record_at(&chats, &db, &bus, &excluded).unwrap() else {
            panic!("expected a message record");
        };
        let list = show_list_at(&chats, &q(Some("sess-a"), false)).unwrap();
        assert!(list.contains("fmail-444444444444"));
        assert!(
            !list.contains("fmail-666666666666"),
            "another pair's mail stays out of the caller's listing"
        );
        let mut q_all = q(Some("sess-z"), false);
        q_all.all = true;
        let list_all = show_list_at(&chats, &q_all).unwrap();
        assert!(list_all.contains("fmail-666666666666"));
        let _ = std::fs::remove_dir_all(&root);

        // --- migration + index: import once, cutover refuses, lost index
        // rebuilds identically, hand-edited rollup is overwritten
        // (AC4-HP, AC4-ERR, AC5-ERR, AC6-HP).
        let root = temp_root("index");
        let chats = root.join("chats");
        let db = root.join("db").join("chats.db");
        let bus = root.join("bus").join("messages.jsonl");
        std::fs::create_dir_all(bus.parent().unwrap()).unwrap();
        std::fs::write(
            &bus,
            format!(
                "{}\n",
                serde_json::to_string(&bus_line("msg-abcdef", "sess-a", "sess-b", "send")).unwrap()
            ),
        )
        .unwrap();
        ensure_ready_at(&chats, &db, &bus).unwrap();
        let imported = std::fs::read_to_string(
            chats
                .join(chat_id_for_pair("sess-a", "sess-b"))
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(imported.contains("\"type\":\"message\""));
        assert!(imported.contains("msg-abcdef"), "original id kept");
        let conn = open_index(&db).unwrap();
        assert!(migrated_at(&conn).unwrap().is_some(), "cutover stamped");
        drop(conn);
        std::fs::remove_file(&db).unwrap();
        let full = resolve_prefix_at(&db, &chats, "msg-abcdef").unwrap().id;
        assert_eq!(full, "msg-abcdef", "old 6-hex ids keep resolving");
        let rebuilt = rebuild_index_at(&db, &chats).unwrap();
        assert!(rebuilt.contains("1 chat"), "{rebuilt}");
        let hand_edited = open_index(&db).unwrap();
        hand_edited
            .execute("UPDATE chats SET msg_count = 999", [])
            .unwrap();
        drop(hand_edited);
        let full2 = resolve_prefix_at(&db, &chats, "msg-abc").unwrap().id;
        assert_eq!(full2, "msg-abcdef", "the index is never authoritative");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root);
    }
}
