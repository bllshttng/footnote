//! The opencode transcript source: the sqlite store opened read-only and
//! rendered as claude-shaped JSONL, so the shared classifier reads it with
//! no new rule. Sessions with a `parent_id` (subagent children) never list.
//!
//! The store is only ever read: `SQLITE_OPEN_READ_ONLY`, no migration, no
//! write of any kind. A store that cannot be opened or queried is a
//! `probe` error naming the store and the sqlite text, never a panic and
//! never a partial guess.

use crate::provenance::{
    claude_shaped_tool_uses, claude_shaped_turns, in_roots, within_window, SessionFile,
    TranscriptSource, Turn,
};
use rusqlite::OpenFlags;
use serde_json::Value;
use std::path::{Path, PathBuf};

const SESSION_SQL: &str = "SELECT s.id, s.directory, s.time_updated, \
     (SELECT coalesce(sum(length(p.data)), 0) FROM part p WHERE p.session_id = s.id) \
     FROM session s WHERE s.parent_id IS NULL";

/// Parts of one session, message-ordered (m.id breaks time ties and groups
/// the render).
const PART_SQL: &str = "SELECT m.id, m.data, p.data, m.time_created FROM part p \
     JOIN message m ON m.id = p.message_id WHERE p.session_id = ?1 \
     ORDER BY m.time_created, m.id, p.id";

/// `${XDG_DATA_HOME:-$HOME/.local/share}/opencode`, the directory opencode
/// keeps its per-channel stores in (`opencode.db`, `opencode-<channel>.db`).
fn stores_dir() -> PathBuf {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".local").join("share"),
            None => PathBuf::from("."),
        });
    data_home.join("opencode")
}

/// Every opencode store on this machine: `$OPENCODE_DB` when set and
/// non-empty, else every file matching `opencode*.db` in the data dir,
/// sorted. The glob covers the per-channel names without spawning opencode.
pub(crate) fn opencode_stores() -> Vec<PathBuf> {
    if let Some(db) = std::env::var_os("OPENCODE_DB").filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(db)];
    }
    let dir = stores_dir();
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("opencode") && n.ends_with(".db"))
        })
        .collect();
    out.sort();
    out
}

fn open_read_only(path: &Path) -> Result<rusqlite::Connection, String> {
    let conn = rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(conn)
}

/// The opencode transcript store: one or more sqlite databases read-only.
/// `dbs` is [`opencode_stores`]' result (tests inject fixture stores);
/// `roots` scopes the fold to the requested projects, `None` reads every
/// project.
pub(crate) struct OpencodeSource {
    pub(crate) dbs: Vec<PathBuf>,
    pub(crate) roots: Option<Vec<PathBuf>>,
}

impl OpencodeSource {
    /// Every declared store must open read-only and answer both queries;
    /// the first failure names the store and the sqlite text. An empty
    /// store list is a miss naming the searched dir.
    pub(crate) fn probe(&self) -> Result<(), String> {
        if self.dbs.is_empty() {
            return Err(format!(
                "no opencode store under {}",
                stores_dir().display()
            ));
        }
        for db in &self.dbs {
            let conn = open_read_only(db)?;
            conn.prepare(SESSION_SQL)
                .map_err(|e| format!("{}: {e}", db.display()))?;
            conn.prepare(PART_SQL)
                .map_err(|e| format!("{}: {e}", db.display()))?;
        }
        Ok(())
    }
}

impl TranscriptSource for OpencodeSource {
    fn harness(&self) -> &'static str {
        "opencode"
    }

    fn sessions(&self, days: u64) -> Vec<SessionFile> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut out = Vec::new();
        for db in &self.dbs {
            let Ok(conn) = open_read_only(db) else {
                continue;
            };
            let Ok(mut stmt) = conn.prepare(SESSION_SQL) else {
                continue;
            };
            let Ok(rows) = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())
            else {
                continue;
            };
            for row in rows.flatten() {
                let (id, directory, time_updated, size) = row;
                // Both stamps are milliseconds; the window reads seconds.
                let mtime = time_updated.max(0) as u64 / 1000;
                if !within_window(mtime, days, now) {
                    continue;
                }
                if !in_roots(directory.as_deref().map(Path::new), self.roots.as_deref()) {
                    continue;
                }
                out.push(SessionFile {
                    path: PathBuf::from(format!("{}#{id}", db.display())),
                    session_id: id,
                    mtime,
                    size: size.max(0) as u64,
                });
            }
        }
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        out
    }

    /// One session's transcript as claude-shaped JSONL, re-queried from the
    /// store the `path` names (`<db>#<session id>`).
    fn read(&self, file: &SessionFile) -> String {
        let display = file.path.to_string_lossy().into_owned();
        let Some((db, sid)) = display.rsplit_once('#') else {
            return String::new();
        };
        let Ok(conn) = open_read_only(Path::new(db)) else {
            return String::new();
        };
        let Ok(mut stmt) = conn.prepare(PART_SQL) else {
            return String::new();
        };
        let Ok(rows) = stmt
            .query_map([sid], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| e.to_string())
        else {
            return String::new();
        };
        // Parts of one message are contiguous in the ordering above; the
        // message id groups them.
        let mut current: Option<(String, String, Vec<(String, i64)>)> = None;
        let mut out = String::new();
        for row in rows.flatten() {
            let (m_id, m_data, p_data, time_created) = row;
            match &mut current {
                Some((cur_id, _, parts)) if *cur_id == m_id => parts.push((p_data, time_created)),
                _ => {
                    if let Some((_, m_data, parts)) = current.take() {
                        render_message(&mut out, &m_data, &parts);
                    }
                    current = Some((m_id, m_data, vec![(p_data, time_created)]));
                }
            }
        }
        if let Some((_, m_data, parts)) = current.take() {
            render_message(&mut out, &m_data, &parts);
        }
        out
    }

    fn turns(&self, raw: &str) -> Vec<Turn> {
        claude_shaped_turns(raw)
    }

    fn tool_uses(&self, raw: &str) -> usize {
        claude_shaped_tool_uses(raw)
    }
}

/// One message's parts rendered as claude-shaped JSONL lines. A user
/// message's typed text parts join into one user row, its `synthetic: true`
/// text parts become the same row with `isMeta`; each `type: tool` part
/// becomes an assistant row carrying one `tool_use` block. Other shapes
/// render nothing.
fn render_message(out: &mut String, m_data: &str, parts: &[(String, i64)]) {
    let Ok(msg) = serde_json::from_str::<Value>(m_data) else {
        return;
    };
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let ts = parts.first().map(|p| p.1).unwrap_or(0).max(0);
    if role == "user" {
        let mut typed: Vec<String> = Vec::new();
        let mut synthetic: Vec<String> = Vec::new();
        for (p_data, _) in parts {
            let Ok(part) = serde_json::from_str::<Value>(p_data) else {
                continue;
            };
            if part.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = part.get("text").and_then(|v| v.as_str()) else {
                continue;
            };
            if part
                .get("synthetic")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                synthetic.push(text.to_string());
            } else {
                typed.push(text.to_string());
            }
        }
        if !typed.is_empty() {
            push_row(
                out,
                &serde_json::json!({
                    "type": "user", "timestamp": rfc3339_ms(ts),
                    "message": {"role": "user", "content": typed.join("\n")},
                }),
            );
        }
        for text in synthetic {
            push_row(
                out,
                &serde_json::json!({
                    "type": "user", "isMeta": true, "timestamp": rfc3339_ms(ts),
                    "message": {"role": "user", "content": text},
                }),
            );
        }
    }
    for (p_data, _) in parts {
        let Ok(part) = serde_json::from_str::<Value>(p_data) else {
            continue;
        };
        if part.get("type").and_then(|v| v.as_str()) == Some("tool") {
            push_row(
                out,
                &serde_json::json!({
                    "type": "assistant",
                    "message": {"content": [{"type": "tool_use"}]},
                }),
            );
        }
    }
}

fn push_row(out: &mut String, row: &Value) {
    out.push_str(&row.to_string());
    out.push('\n');
}

fn rfc3339_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms.div_euclid(1000), 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::BusIndex;
    use rusqlite::Connection;

    const NOW_MS: i64 = 1_785_781_960_124;

    fn user_msg() -> &'static str {
        r#"{"role":"user","time":{"created":1785781960124}}"#
    }

    /// The plan fixture: one top-level session in /fixture/project whose user
    /// message has one typed text part, one synthetic text part, and one tool
    /// part; one child session with a parent_id; one top-level session in
    /// /elsewhere.
    fn fixture_store(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-opencode-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);",
        )
        .unwrap();
        for (id, parent, directory) in [
            ("s1", None, "/fixture/project"),
            ("s2", Some("s1"), "/fixture/project"),
            ("s3", None, "/elsewhere"),
        ] {
            conn.execute(
                "INSERT INTO session VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, parent, directory, NOW_MS],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO message VALUES ('m1', 's1', ?1, ?2)",
            rusqlite::params![NOW_MS, user_msg()],
        )
        .unwrap();
        for (id, data) in [
            (
                r#"p1"#,
                r#"{"type":"text","text":"please widen the review gate"}"#,
            ),
            (
                "p2",
                r#"{"type":"text","text":"<environment_context>ignored</environment_context>","synthetic":true}"#,
            ),
            ("p3", r#"{"type":"tool","tool":"bash","callID":"c1"}"#),
        ] {
            conn.execute(
                "INSERT INTO part VALUES (?1, 'm1', 's1', ?2)",
                rusqlite::params![id, data],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn roots_narrow_to_the_project_and_drop_children() {
        let db = fixture_store("roots");
        let src = OpencodeSource {
            dbs: vec![db.clone()],
            roots: Some(vec![PathBuf::from("/fixture/project")]),
        };
        let sessions = src.sessions(0);
        assert_eq!(
            sessions.len(),
            1,
            "top-level s1 only, no child, no elsewhere"
        );
        assert_eq!(sessions[0].session_id, "s1");
        assert_eq!(
            sessions[0].path,
            PathBuf::from(format!("{}#s1", db.display()))
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn the_render_classifies_operator_synthetic_and_tool() {
        let db = fixture_store("render");
        let src = OpencodeSource {
            dbs: vec![db.clone()],
            roots: None,
        };
        let file = &src.sessions(0)[0];
        let raw = src.read(file);
        let turns = src.turns(&raw);
        let mut counters: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
        for turn in &turns {
            let p = crate::provenance::classify_turn(&turn.obj, &BusIndex::empty(), "s1");
            *counters.entry(p.label()).or_insert(0) += 1;
        }
        assert_eq!(counters.get("unknown"), Some(&1), "typed text part");
        assert_eq!(
            counters.get("harness_synthetic"),
            Some(&1),
            "synthetic text part"
        );
        assert_eq!(src.tool_uses(&raw), 1, "tool part");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn typed_text_parts_of_one_message_join_into_one_row() {
        let db = fixture_store("join");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m2', 's2', ?1, ?2)",
            rusqlite::params![NOW_MS, user_msg()],
        )
        .unwrap();
        for (id, data) in [
            ("p4", r#"{"type":"text","text":"first half"}"#),
            ("p5", r#"{"type":"text","text":"second half"}"#),
        ] {
            conn.execute(
                "INSERT INTO part VALUES (?1, 'm2', 's2', ?2)",
                rusqlite::params![id, data],
            )
            .unwrap();
        }
        drop(conn);
        let src = OpencodeSource {
            dbs: vec![db.clone()],
            roots: None,
        };
        let file = SessionFile {
            session_id: "s2".to_string(),
            path: PathBuf::from(format!("{}#s2", db.display())),
            mtime: 0,
            size: 0,
        };
        let raw = src.read(&file);
        let turns = src.turns(&raw);
        assert_eq!(turns.len(), 1, "two typed parts, one user row");
        assert_eq!(turns[0].text, "first half\nsecond half");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_store_missing_the_part_table_probes_as_an_error() {
        let dir = std::env::temp_dir().join(format!("fno-opencode-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, time_updated INTEGER);",
        )
        .unwrap();
        drop(conn);
        let src = OpencodeSource {
            dbs: vec![db.clone()],
            roots: None,
        };
        let err = src.probe().unwrap_err();
        assert!(
            err.contains(&db.display().to_string()),
            "names the store: {err}"
        );
        assert!(!err.is_empty(), "carries the sqlite text");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_store_list_probes_as_a_miss_naming_the_dir() {
        let src = OpencodeSource {
            dbs: Vec::new(),
            roots: None,
        };
        let err = src.probe().unwrap_err();
        assert!(
            err.starts_with("no opencode store under"),
            "names the searched dir: {err}"
        );
    }
}
