//! The thread read model over the conversation record plane. One JSON
//! projection of the chats store for the mux Messages tab: participants
//! joined to the registry, one thread per stored conversation, the
//! per-session System rows, the broadcast channels and the standing
//! announcements the bell reads.
//!
//! The store ([`crate::chats`]) is the record plane; this module only reads
//! it, so pair chat ids and fmail resolution stay in one place (the
//! handoff note on the node). A missing store reads as an empty projection,
//! `unreadable: 0`; a line that does not parse is counted and skipped,
//! never fatal (AC1-ERR).

use crate::chats::chats_dir;
use crate::paths::AgentsHome;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// Senders the read model drops (R8): the mail-hold digest is a delivery,
/// never a thread.
fn is_hold_sender(sender: &str) -> bool {
    crate::system_sender::canonical(sender) == "fno/mail-hold"
}

/// The registry's rows, tolerant: a missing or malformed file reads as none.
fn registry_rows() -> Vec<Value> {
    let text = std::fs::read_to_string(AgentsHome::from_env().registry_json()).unwrap_or_default();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("agents").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// The registry row that IS this address: session id first, then name and
/// aliases (law d-e952ed19: a name is a label, the session id is the key).
fn registry_lookup<'a>(rows: &'a [Value], key: &str) -> Option<&'a Value> {
    if key.is_empty() {
        return None;
    }
    rows.iter().find(|row| {
        row.get("fno_id").and_then(Value::as_str) == Some(key)
            || row.get("harness_session_id").and_then(Value::as_str) == Some(key)
            || row.get("session_id").and_then(Value::as_str) == Some(key)
            || row.get("name").and_then(Value::as_str) == Some(key)
            || row
                .get("aliases")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().filter_map(Value::as_str).any(|s| s == key))
    })
}

/// A system sender keys by its canonical `fno/<arm>` name, never by a
/// registry row, so a legacy stamp and its new name join as one voice.
fn participant_key(from_session: &str, name: &str, registry: &[Value]) -> String {
    if !from_session.is_empty() {
        return from_session.to_string();
    }
    if crate::system_sender::is_system_sender(name) {
        return crate::system_sender::canonical(name).to_string();
    }
    match registry_lookup(registry, name) {
        Some(row) => row
            .get("fno_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| row.get("harness_session_id").and_then(Value::as_str))
            .unwrap_or(name)
            .to_string(),
        None => name.to_string(),
    }
}

/// The addressee's key: the registry row of the addressed name, else the raw
/// string (a project address or a name no registry row ever held becomes its
/// own participant, the judge disposition).
fn recipient_key(to: &str, registry: &[Value]) -> String {
    if to.is_empty() {
        return to.to_string();
    }
    match registry_lookup(registry, to) {
        Some(row) => row
            .get("fno_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| row.get("harness_session_id").and_then(Value::as_str))
            .unwrap_or(to)
            .to_string(),
        None => to.to_string(),
    }
}

/// The broadcast scope an announce row answers to (the chats store's rule).
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

/// True while an announce row still stands: no usable `expires`, or one in
/// the future.
fn standing(line: &Value, now: u64) -> bool {
    match line.get("expires").and_then(Value::as_str) {
        Some(e) => crate::state::rfc3339_like_to_secs(e).is_none_or(|t| t > now),
        None => true,
    }
}

/// The projection of one chats store. `now` decides which announcements
/// still stand.
pub(crate) fn project_at(chats: &Path, registry: &[Value], now: u64) -> Value {
    let mut participants: BTreeMap<String, Value> = BTreeMap::new();
    // Counterparty scope counts and latest row per ended participant (R3).
    let mut scope_votes: BTreeMap<String, BTreeMap<String, (usize, String)>> = BTreeMap::new();
    let mut threads: Vec<Value> = Vec::new();
    let mut system: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut channels: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut announcements: Vec<Value> = Vec::new();
    let mut unreadable = 0usize;

    let rd = match std::fs::read_dir(chats) {
        Ok(rd) => rd,
        Err(_) => {
            return json!({
                "participants": [],
                "threads": [],
                "system": {},
                "channels": [],
                "announcements": [],
                "unreadable": 0,
            })
        }
    };
    for entry in rd.flatten() {
        let chat_id = match entry.file_name().into_string() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let file = entry.path().join("messages.jsonl");
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut rows: Vec<Value> = Vec::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                unreadable += 1;
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("message") {
                continue; // a delivery receipt, not a conversation row
            }
            let from = v.get("from").and_then(Value::as_str).unwrap_or("");
            let from_session = v.get("from_session").and_then(Value::as_str).unwrap_or("");
            let to = v.get("to").and_then(Value::as_str).unwrap_or("");
            if is_hold_sender(from) {
                continue; // R8: the digest never appears
            }
            let from_key = participant_key(from_session, from, registry);
            let to_key = recipient_key(to, registry);
            let reg_from = if from_session.is_empty() && !from.is_empty() {
                registry_lookup(registry, from)
            } else {
                registry_lookup(registry, &from_key)
            };
            let reg_to = registry_lookup(registry, to);
            note(&mut participants, &from_key, reg_from);
            note(&mut participants, &to_key, reg_to);
            let system_row = crate::system_sender::is_system_sender(from);
            let mut row = json!({
                "id": v.get("id").and_then(Value::as_str).unwrap_or(""),
                "ts": v.get("ts").and_then(Value::as_str).unwrap_or(""),
                "from": crate::system_sender::canonical(from),
                "from_key": from_key,
                "to_key": to_key,
                "to": to,
                "summary": crate::mail_header::summary_of(
                    v.get("body").and_then(Value::as_str).unwrap_or(""),
                ),
                "body": v.get("body").and_then(Value::as_str).unwrap_or(""),
                "expires": v.get("expires").and_then(Value::as_str),
                "in_reply_to": v.get("in_reply_to").and_then(Value::as_str),
                "delivery": v.get("delivery").and_then(Value::as_str),
                "system": system_row,
            });
            match v.get("kind").and_then(Value::as_str) {
                Some("announce") => {
                    let scope = scope_of(&v);
                    let sender_is_incident =
                        matches!(from, "fleet-incident" | "fno/fleet-incident");
                    if sender_is_incident {
                        if standing(&v, now) {
                            announcements.push(row);
                        }
                    } else if standing(&v, now) {
                        if let Some(obj) = row.as_object_mut() {
                            obj.insert("scope".into(), json!(scope));
                        }
                        channels.entry(scope).or_default().push(row);
                    }
                }
                _ => {
                    // A pair row. Vote its counterparty scope for R3 and,
                    // when the sender is a system arm, land it in the
                    // receiving session's System row (AC14-HP, inbound only).
                    if system_row {
                        system.entry(to_key.clone()).or_default().push(row.clone());
                    } else {
                        let receiver_scope = reg_to
                            .and_then(|r| r.get("crown_scope"))
                            .and_then(Value::as_str);
                        let sender_scope = reg_from
                            .and_then(|r| r.get("crown_scope"))
                            .and_then(Value::as_str);
                        let ts = v.get("ts").and_then(Value::as_str).unwrap_or("");
                        if let Some(scope) = sender_scope.filter(|_| !system_row) {
                            vote(&mut scope_votes, &to_key, scope, ts);
                        }
                        if let Some(scope) = receiver_scope {
                            vote(&mut scope_votes, &from_key, scope, ts);
                        }
                    }
                    rows.push(row);
                }
            }
        }
        if rows.is_empty() {
            continue;
        }
        rows.sort_by(|a, b| {
            a.get("ts")
                .and_then(Value::as_str)
                .cmp(&b.get("ts").and_then(Value::as_str))
        });
        let keys = {
            let mut k: Vec<String> = rows
                .iter()
                .filter_map(|r| r.get("from_key").and_then(Value::as_str))
                .chain(
                    rows.iter()
                        .filter_map(|r| r.get("to_key").and_then(Value::as_str)),
                )
                .filter(|k| participants.contains_key(*k))
                .map(str::to_string)
                .collect();
            k.sort_unstable();
            k.dedup();
            k
        };
        let last_ts = rows
            .last()
            .and_then(|r| r.get("ts"))
            .and_then(Value::as_str)
            .unwrap_or("");
        threads.push(json!({
            "chat_id": chat_id,
            "participants": keys,
            "rows": rows,
            "last_ts": last_ts,
        }));
    }
    // R3: an ended participant archives under the scope its registry row
    // still holds, else the scope it mailed most, latest on a tie.
    for (key, p) in participants.iter_mut() {
        let reg = registry_lookup(registry, key);
        let held = reg
            .and_then(|r| r.get("crown_scope"))
            .and_then(Value::as_str)
            .filter(|_| {
                reg.and_then(|r| r.get("liveness")).and_then(Value::as_str) != Some("alive")
                    && reg.and_then(|r| r.get("status")).and_then(Value::as_str) != Some("live")
            });
        let scope: Option<String> = held
            .map(str::to_string)
            .or_else(|| scope_votes.get(key).and_then(|votes| best_scope(votes)));
        if let Some(scope) = scope {
            if let Some(obj) = p.as_object_mut() {
                obj.insert("archive_scope".into(), json!(scope));
            }
        }
    }
    let mut sorted_threads = threads;
    sorted_threads.sort_by(|a, b| {
        a.get("last_ts")
            .and_then(Value::as_str)
            .cmp(&b.get("last_ts").and_then(Value::as_str))
    });
    json!({
        "participants": participants.values().cloned().collect::<Vec<_>>(),
        "threads": sorted_threads,
        "system": system,
        "channels": channels.into_iter().map(|(scope, rows)| json!({"scope": scope, "rows": rows})).collect::<Vec<_>>(),
        "announcements": announcements,
        "unreadable": unreadable,
    })
}

/// One participant row, inserted once per key with its registry join.
fn note<'a>(
    participants: &'a mut BTreeMap<String, Value>,
    key: &str,
    row: Option<&Value>,
) -> &'a mut Value {
    participants.entry(key.to_string()).or_insert_with(|| {
        let reg = row;
        json!({
            "key": key,
            "name": reg.and_then(|r| r.get("name")).and_then(Value::as_str)
                .filter(|_| !crate::system_sender::is_system_sender(key))
                .unwrap_or(key),
            "session_id": reg.and_then(|r| r.get("harness_session_id"))
                .and_then(Value::as_str),
            "fno_id": reg.and_then(|r| r.get("fno_id")).and_then(Value::as_str),
            "harness": reg.and_then(|r| r.get("harness")).and_then(Value::as_str),
            "model": reg.and_then(|r| r.get("model")).and_then(Value::as_str),
            "effort": reg.and_then(|r| r.get("effort")).and_then(Value::as_str),
            "node": reg.and_then(|r| r.get("node")).and_then(Value::as_str),
            "live": reg.is_some_and(|r| {
                r.get("liveness").and_then(Value::as_str) == Some("alive")
                    || r.get("status").and_then(Value::as_str) == Some("live")
            }),
            "crown_scope": reg.and_then(|r| r.get("crown_scope")).and_then(Value::as_str),
            "crown_level": reg.and_then(|r| r.get("crown_level")).and_then(Value::as_u64),
            "created_at": reg.and_then(|r| r.get("created_at")).and_then(Value::as_str),
            "exited_at": reg.and_then(|r| r.get("exited_at")).and_then(Value::as_str),
            "system": crate::system_sender::is_system_sender(key),
            "archive_scope": Value::Null,
        })
    })
}

/// One counterparty-scope vote for R3: count per scope, latest ts on a tie.
fn vote(
    votes: &mut BTreeMap<String, BTreeMap<String, (usize, String)>>,
    key: &str,
    scope: &str,
    ts: &str,
) {
    let entry = votes
        .entry(key.to_string())
        .or_default()
        .entry(scope.to_string())
        .or_insert((0, String::new()));
    entry.0 += 1;
    if ts > entry.1.as_str() {
        entry.1 = ts.to_string();
    }
}

/// The scope with the most votes, latest ts breaking a tie.
fn best_scope(votes: &BTreeMap<String, (usize, String)>) -> Option<String> {
    votes
        .iter()
        .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(a.1 .1.cmp(&b.1 .1)))
        .map(|(scope, _)| scope.clone())
}

/// The hidden `fno-agents mail-threads` verb: `--format json` (the only
/// form) prints the projection; `details --session <id>` prints that
/// session's token counters and ledger cost. Hidden from help, like
/// `court-fold` (ruling d-aef0ed7b).
pub fn run_mail_threads(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        return run_mail_threads(&["--format".into(), "json".to_string()]);
    };
    match sub.as_str() {
        "--format" | "-f" => {
            if args.get(1).map(String::as_str) != Some("json") {
                eprintln!("mail-threads: only --format json is supported");
                return 2;
            }
            let projection = project_at(&chats_dir(), &registry_rows(), now_secs());
            println!("{}", serde_json::to_string(&projection).unwrap_or_default());
            0
        }
        "details" => run_details(&args[1..]),
        _ => {
            eprintln!("usage: fno-agents mail-threads [--format json | details --session <id>]");
            2
        }
    }
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

/// The details action: one session's token counters over its transcript and
/// its node_costs sum from the graph. A missing source prints that key as
/// null; nothing here fails the whole read (AC3-EDGE spirit).
fn run_details(args: &[String]) -> i32 {
    let Some(session) = args
        .iter()
        .position(|a| a == "--session")
        .and_then(|i| args.get(i + 1))
    else {
        eprintln!("mail-threads details: --session <id> is required");
        return 2;
    };
    let registry = registry_rows();
    let row = match registry_lookup(&registry, session) {
        None => {
            eprintln!("mail-threads details: no registry row for {session:?}");
            return 1;
        }
        Some(v) => v.clone(),
    };
    let sid = row
        .get("harness_session_id")
        .and_then(Value::as_str)
        .unwrap_or(session.as_str());
    let harness = row.get("harness").and_then(Value::as_str).unwrap_or("");
    let tokens = transcript_tokens(sid, harness);
    let cost = ledger_cost(sid);
    println!(
        "{}",
        json!({
            "session": sid,
            "tokens": tokens,
            "cost_usd": cost,
        })
    );
    0
}

/// Token counters over the session's transcript: claude projects, codex
/// walks its sessions tree; any other harness has no reader here, so its
/// tokens read null rather than a wrong zero (AC3-EDGE).
fn transcript_tokens(sid: &str, harness: &str) -> Option<Value> {
    if sid.is_empty() {
        return None;
    }
    let codex = harness == "codex";
    if harness != "codex" && harness != "claude" && !harness.is_empty() {
        return None;
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let path = crate::announce::transcript_path(&home, sid, codex)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let act = if codex {
        crate::session_activity::codex_activity(&raw)
    } else {
        crate::session_activity::claude_activity(&raw)
    };
    let t = act.tokens;
    Some(json!({
        "input": t.input,
        "output": t.output,
        "cache_read": t.cache_read,
        "cache_write": t.cache_write,
    }))
}

/// The session's cost from the graph's node_costs rows. Any read failure
/// reads as null, never an error exit: this modal informs, it never blocks.
fn ledger_cost(sid: &str) -> Option<f64> {
    let db = crate::state_layout::place(&crate::backlog::settings::state_dir()?, "graph.db");
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(2)).ok()?;
    conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0.0) FROM node_costs WHERE session_id = ?1",
        [sid],
        |r| r.get::<_, Option<f64>>(0),
    )
    .ok()?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    static ENV_LOCK: std::sync::LazyLock<&'static std::sync::Mutex<()>> =
        std::sync::LazyLock::new(crate::claims::test_env_lock);

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mail-threads-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_chat(root: &Path, id: &str, lines: &[Value]) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let text = lines
            .iter()
            .map(|l| serde_json::to_string(l).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("messages.jsonl"), format!("{text}\n")).unwrap();
    }

    fn msg(id: &str, ts: &str, from: &str, to: &str, body: &str) -> Value {
        json!({
            "type": "message", "kind": "send", "v": 1,
            "id": id, "ts": ts, "thread": id,
            "from": from, "to": to, "body": body,
        })
    }

    #[test]
    fn mail_threads_contracts() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = crate::state::rfc3339_like_to_secs("2026-10-02T12:00:00Z").unwrap();
        // The registry: a lead, its live worker, an exited worker, and the
        // crowned successor over the same scope (AC2-HP).
        let registry = vec![
            json!({"name": "vellum", "fno_id": "s-vellum", "harness_session_id": "s-vellum",
                   "harness": "claude", "liveness": "alive", "status": "live",
                   "crown_level": 1, "crown_scope": "fno"}),
            json!({"name": "candor", "fno_id": "s-candor", "harness_session_id": "s-candor",
                   "harness": "codex", "liveness": "alive", "status": "live"}),
            json!({"name": "quill", "fno_id": "s-quill", "harness_session_id": "s-quill",
                   "harness": "claude", "liveness": "dead", "status": "exited",
                   "exited_at": "2026-10-01T10:00:00Z"}),
        ];
        let root = temp_root("projection");
        let chats = root.join("chats");
        // candor <-> vellum, two rows, second a reply.
        write_chat(
            &chats,
            "chat-aaaaaaaaaaaaaaaa",
            &[
                msg(
                    "fmail-111111111111",
                    "2026-10-01T09:00:00Z",
                    "s-candor",
                    "vellum",
                    "Ship the auth fix. It blocks the release.",
                ),
                json!({
                    "type": "message", "kind": "send", "v": 1,
                    "id": "fmail-222222222222", "ts": "2026-10-01T09:05:00Z", "thread": "fmail-111111111111",
                    "from": "s-vellum", "from_session": "s-vellum", "to": "candor",
                    "in_reply_to": "fmail-111111111111", "body": "On it.",
                }),
            ],
        );
        // The exited worker's thread (its Archive vote).
        write_chat(
            &chats,
            "chat-bbbbbbbbbbbbbbbb",
            &[msg(
                "fmail-333333333333",
                "2026-10-01T08:00:00Z",
                "s-quill",
                "vellum",
                "Opening the bell PR.",
            )],
        );
        // A system arm mails the live worker (the ONE System row, inbound).
        write_chat(
            &chats,
            "chat-cccccccccccccccc",
            &[msg(
                "fmail-444444444444",
                "2026-10-01T09:10:00Z",
                "king-settle",
                "candor",
                "Settle point reached.",
            )],
        );
        // Announcements: a team broadcast (a # channel), fleet-incident
        // (bell only, never a channel), one expired team row, and a hold
        // sender the projection must drop (R8).
        write_chat(
            &chats,
            "chat-dddddddddddddddd",
            &[
                json!({
                    "type": "message", "kind": "announce", "v": 1,
                    "id": "fmail-555555555555", "ts": "2026-10-01T09:00:00Z",
                    "from": "vellum", "to": "fleet:fno", "meta": {"scope": "fno"},
                    "body": "Test hold lifted at noon.",
                }),
                json!({
                    "type": "message", "kind": "announce", "v": 1,
                    "id": "fmail-666666666666", "ts": "2026-10-01T09:01:00Z",
                    "from": "fno/fleet-incident", "to": "fleet:fno", "meta": {"scope": "fno"},
                    "body": "Heavy benchmark running.",
                }),
                json!({
                    "type": "message", "kind": "announce", "v": 1,
                    "id": "fmail-777777777777", "ts": "2026-10-01T09:02:00Z",
                    "from": "vellum", "to": "fleet:fno", "meta": {"scope": "fno"},
                    "body": "expired", "expires": "2026-10-01T10:00:00Z",
                }),
                msg(
                    "fmail-888888888888",
                    "2026-10-01T09:03:00Z",
                    "fno-mail-hold",
                    "s-candor",
                    "digest line",
                ),
            ],
        );
        // One broken line (AC1-ERR).
        let broken = root.join("chats").join("chat-eeeeeeeeeeeeeeee");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("messages.jsonl"), "{not json\n").unwrap();
        let projection = project_at(&chats, &registry, now);
        let threads = projection.get("threads").and_then(Value::as_array).unwrap();
        let participants = projection
            .get("participants")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(threads.len(), 3, "pair chats only: {threads:?}");
        // The successor over the scope sees the same Archive: quill's
        // archive_scope names the scope it mailed most (AC2-HP).
        let quill = participants
            .iter()
            .find(|p| p.get("key").and_then(Value::as_str) == Some("s-quill"))
            .expect("quill participant");
        assert_eq!(
            quill.get("archive_scope").and_then(Value::as_str),
            Some("fno")
        );
        // The ONE System row per session, inbound only, badge carried by
        // the row flag (AC14-HP).
        let system = projection.get("system").and_then(Value::as_object).unwrap();
        assert_eq!(system.len(), 1, "one session holds system mail: {system:?}");
        let rows = system.values().next().unwrap().as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("from").and_then(Value::as_str),
            Some("fno/king-settle")
        );
        // Channels and announcements (AC4-HP, R12).
        let channels = projection
            .get("channels")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(channels.len(), 1, "one scope: {channels:?}");
        let chan_rows = channels[0].get("rows").and_then(Value::as_array).unwrap();
        assert_eq!(
            chan_rows.len(),
            1,
            "the expired row stands out: {chan_rows:?}"
        );
        assert_eq!(
            chan_rows[0].get("id").and_then(Value::as_str),
            Some("fmail-555555555555")
        );
        let ann = projection
            .get("announcements")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(ann.len(), 1);
        assert_eq!(
            ann[0].get("id").and_then(Value::as_str),
            Some("fmail-666666666666")
        );
        // The hold digest never appears (R8) and the broken line counts (AC1-ERR).
        let bodies = serde_json::to_string(&projection).unwrap();
        assert!(!bodies.contains("digest line"), "hold dropped: {bodies}");
        assert_eq!(projection.get("unreadable"), Some(&json!(1)));
        let _ = std::fs::remove_dir_all(&root);
    }
}
